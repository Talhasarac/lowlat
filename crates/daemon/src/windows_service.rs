//! The Windows service: the role the service manager starts, and the host's
//! side of the events it shares with that role.
//!
//! **The service starts and watches one host in the console session.** It
//! holds the token, the job and the log; the host captures, encodes and talks
//! to guests. The two are one binary and two roles, and everything between
//! them is unnamed events inherited by that host alone: the service asks it to
//! end, and the host asks for the secure attention call. Nothing is named, so
//! no other process can find or signal either
//! (docs/impl-plan-windows.md W2.5).
//!
//! The loop reconciles on every pass rather than reacting to one notification:
//! the console's session, the host's session, and how it last exited. The same
//! decision therefore survives a missed notification, and its table is tested
//! without a service manager (`next`).

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicIsize, AtomicPtr, Ordering};

use lowlat_common::clock::{Time, elapsed_ms};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_FAILED_SERVICE_CONTROLLER_CONNECT,
    ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, GetLastError, HANDLE, HANDLE_FLAG_INHERIT,
    INVALID_HANDLE_VALUE, SYSTEMTIME, SetHandleInformation, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::{
    DuplicateTokenEx, SECURITY_ATTRIBUTES, SecurityImpersonation, SetTokenInformation,
    TOKEN_ALL_ACCESS, TokenPrimary, TokenSessionId,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_APPEND_DATA, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_ALWAYS, WriteFile,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};
use windows_sys::Win32::System::Registry::{
    HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE, KEY_SET_VALUE, KEY_WOW64_64KEY, REG_DWORD, RegCloseKey,
    RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
};
use windows_sys::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
use windows_sys::Win32::System::Services::{
    RegisterServiceCtrlHandlerExW, SERVICE_ACCEPT_PRESHUTDOWN, SERVICE_ACCEPT_SESSIONCHANGE,
    SERVICE_ACCEPT_STOP, SERVICE_CONTROL_CONTINUE, SERVICE_CONTROL_INTERROGATE,
    SERVICE_CONTROL_PRESHUTDOWN, SERVICE_CONTROL_SESSIONCHANGE, SERVICE_CONTROL_SHUTDOWN,
    SERVICE_CONTROL_STOP, SERVICE_RUNNING, SERVICE_START_PENDING, SERVICE_STATUS,
    SERVICE_STATUS_HANDLE, SERVICE_STOP_PENDING, SERVICE_STOPPED, SERVICE_TABLE_ENTRYW,
    SERVICE_WIN32_OWN_PROCESS, SetServiceStatus, StartServiceCtrlDispatcherW,
};
use windows_sys::Win32::System::SystemInformation::GetSystemTime;
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateEventW,
    CreateProcessAsUserW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
    GetCurrentProcess, GetExitCodeProcess, INFINITE, InitializeProcThreadAttributeList,
    OpenProcessToken, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_INFORMATION, ResetEvent,
    ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW, SetEvent, TerminateProcess,
    UpdateProcThreadAttribute, WaitForMultipleObjects, WaitForSingleObject,
};

/// The service's own name and what the service manager shows.
pub(crate) const SERVICE_NAME: &str = "lowlatd";
pub(crate) const DISPLAY_NAME: &str = "lowlat host";

/// The host's inherited handles, named only in its environment.
const LOG_VAR: &str = "LOWLATD_LOG_HANDLE";
const END_VAR: &str = "LOWLATD_END_EVENT";
const ATTENTION_VAR: &str = "LOWLATD_ATTENTION_EVENT";

/// The exit code a process gets when the session it is in ends.
const SESSION_ENDED: u32 = 0x4001_0004;

/// A failed exit is restarted after this long; an end request gets this long
/// before the job is used (docs/impl-plan-windows.md W2.5 defaults 2 and 5).
const RESTART_MS: f64 = 2000.0;
const END_BOUND_MS: f64 = 5000.0;
/// The same bound in the unit the wait takes.
const END_BOUND_WAIT: u32 = 5000;

/// The safety net under the notifications: the loop still looks at the world
/// this often when nothing signals it.
const PASS_MS: u32 = 2000;

/// The log grows to this and then becomes `lowlatd.log.1` at the next host
/// start, when nothing else is writing it.
const LOG_LIMIT: u64 = 16 * 1024 * 1024;

static STATE: AtomicPtr<State> = AtomicPtr::new(std::ptr::null_mut());
static LOG_HANDLE: AtomicIsize = AtomicIsize::new(0);
static END_HANDLE: AtomicIsize = AtomicIsize::new(0);
static ATTENTION_HANDLE: AtomicIsize = AtomicIsize::new(0);
static START: std::sync::OnceLock<Time> = std::sync::OnceLock::new();

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn program_data() -> Option<PathBuf> {
    std::env::var_os("ProgramData").map(|base| PathBuf::from(base).join("lowlat"))
}

fn utc_now() -> String {
    let mut time = SYSTEMTIME::default();
    // SAFETY: the structure is plain data, written by the call.
    unsafe { GetSystemTime(&raw mut time) };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        time.wYear, time.wMonth, time.wDay, time.wHour, time.wMinute, time.wSecond
    )
}

/// The service's lines and the host's, one whole line per write.
///
/// **One append handle and one write per line**, so the two processes cannot
/// interleave halves of a line. Each process's clock is elapsed time like any
/// other, and the `utc=` field on the start lines anchors it to a wall clock.
fn write_log(level: lowlat_common::log::Level, message: &str) {
    let handle = LOG_HANDLE.load(Ordering::Relaxed) as HANDLE;
    if handle.is_null() {
        return;
    }
    let since = START.get_or_init(Time::now);
    let line = format!(
        "[{:9.3}] [{}] {}\n",
        elapsed_ms(*since) / 1000.0,
        level.tag(),
        message
    );
    let mut written = 0u32;
    // SAFETY: the handle is one this process opened for appending and keeps
    // open for its life; the buffer lives for the call.
    unsafe {
        WriteFile(
            handle,
            line.as_ptr().cast(),
            u32::try_from(line.len()).unwrap_or(0),
            &raw mut written,
            std::ptr::null_mut(),
        );
    }
}

fn install_log_sink() {
    let _ = lowlat_common::log::set_sink(write_log);
}

fn open_log() -> Result<HANDLE, String> {
    let dir = program_data().ok_or_else(|| "ProgramData is not in the environment".to_string())?;
    std::fs::create_dir_all(&dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    open_append(&dir.join("lowlatd.log"))
}

/// One inheritable append handle on the log.
///
/// **Shared for deletion too**, because the service holds this handle while
/// it renames the file to `lowlatd.log.1`: a rename needs every open handle to
/// share deletion, and without it the log never rotates.
fn open_append(path: &std::path::Path) -> Result<HANDLE, String> {
    let path_w = wide(&path.to_string_lossy());
    // The host inherits this handle, so it is created inheritable; the host
    // clears the flag on its own copy when it takes it.
    let attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(core::mem::size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(0),
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 1,
    };
    // SAFETY: a terminated path, a security attributes block that lives for
    // the call, and flags the call documents.
    let handle = unsafe {
        CreateFileW(
            path_w.as_ptr(),
            FILE_APPEND_DATA,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            &attributes,
            OPEN_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE || handle.is_null() {
        return Err(format!(
            "{} could not be opened, error={}",
            path.display(),
            unsafe { GetLastError() }
        ));
    }
    Ok(handle)
}

/// True when the log was long enough to become `lowlatd.log.1`.
fn rotate_log(dir: &std::path::Path) -> bool {
    let path = dir.join("lowlatd.log");
    let Ok(meta) = std::fs::metadata(&path) else {
        return false;
    };
    if meta.len() < LOG_LIMIT {
        return false;
    }
    let old = dir.join("lowlatd.log.1");
    let _ = std::fs::remove_file(&old);
    std::fs::rename(&path, &old).is_ok()
}

/// What one pass sees.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Pass {
    console: Option<u32>,
    host: HostState,
    last_exit: Option<Exit>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum HostState {
    None,
    Running { session: u32 },
    AskedEnd { since_ms: f64 },
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Exit {
    at_ms: f64,
    code: u32,
    session: u32,
    /// The service had asked it to end. A clean exit then is the answer to
    /// that request, not a host with nothing to do.
    asked: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Wait,
    Start { session: u32 },
    AskEnd,
    Terminate,
}

/// The whole policy, as a table over one pass. Times are milliseconds on the
/// loop's own monotonic clock.
///
/// A clean exit of its own is not restarted while the console stays where it
/// was: a host that is not logged in exits at once, and restarting it every
/// two seconds would loop a line forever. It is restarted when the console
/// moves. A host that ended because it was asked to is followed at once, and
/// one that ended with its session at once into a console that moved; a
/// failed exit, and a session's end with the console on the same number, after
/// two seconds. A host that was asked to end gets five.
fn next(pass: Pass, now_ms: f64) -> Step {
    if let HostState::AskedEnd { since_ms } = pass.host {
        return if now_ms - since_ms >= END_BOUND_MS {
            Step::Terminate
        } else {
            Step::Wait
        };
    }
    let Some(console) = pass.console else {
        return match pass.host {
            HostState::Running { .. } => Step::AskEnd,
            _ => Step::Wait,
        };
    };
    match pass.host {
        HostState::Running { session } => {
            if session == console {
                Step::Wait
            } else {
                Step::AskEnd
            }
        }
        HostState::AskedEnd { .. } => Step::Wait,
        HostState::None => match pass.last_exit {
            None => Step::Start { session: console },
            // The answer to the service's request: the console is owed a host
            // whether or not its session moved, since one that read none for a
            // moment and came back is the same console.
            Some(exit) if exit.code == 0 && exit.asked => Step::Start { session: console },
            Some(exit) if exit.code == 0 => {
                if exit.session == console {
                    Step::Wait
                } else {
                    Step::Start { session: console }
                }
            }
            // Ended with its session, as at a logoff. The console's number
            // alone cannot say the session is new: the same number is either
            // the old session still going, which refuses a launch, or a new one
            // that took the number. So only a moved console is started at once.
            Some(exit) if exit.code == SESSION_ENDED && exit.session != console => {
                Step::Start { session: console }
            }
            Some(exit) => {
                if now_ms - exit.at_ms >= RESTART_MS {
                    Step::Start { session: console }
                } else {
                    Step::Wait
                }
            }
        },
    }
}

struct Job(HANDLE);

impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: the job was made by this process and is closed once.
        unsafe { CloseHandle(self.0) };
    }
}

struct HostRun {
    process: HANDLE,
    session: u32,
    asked_ms: Option<f64>,
}

struct State {
    stop: HANDLE,
    wake: HANDLE,
    end: HANDLE,
    attention: HANDLE,
    log: HANDLE,
    job: Job,
    args: String,
    environment: Vec<(String, String)>,
}

unsafe extern "system" fn handler(
    control: u32,
    _event_type: u32,
    _event_data: *mut c_void,
    context: *mut c_void,
) -> u32 {
    // SAFETY: the context is the state this service made and keeps for its
    // life, and both events exist before the handler is registered.
    let state = unsafe { &*(context as *const State) };
    match control {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN | SERVICE_CONTROL_PRESHUTDOWN => {
            // SAFETY: an event this service created.
            unsafe { SetEvent(state.stop) };
            0
        }
        SERVICE_CONTROL_SESSIONCHANGE => {
            // SAFETY: as above.
            unsafe { SetEvent(state.wake) };
            0
        }
        SERVICE_CONTROL_INTERROGATE | SERVICE_CONTROL_CONTINUE => 0,
        _ => 120, // ERROR_CALL_NOT_IMPLEMENTED
    }
}

fn report(handle: SERVICE_STATUS_HANDLE, current: u32, controls: u32, checkpoint: u32, hint: u32) {
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: current,
        dwControlsAccepted: controls,
        dwWin32ExitCode: 0,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: checkpoint,
        dwWaitHint: hint,
    };
    // SAFETY: the handle came from RegisterServiceCtrlHandlerExW and the
    // structure lives for the call.
    unsafe { SetServiceStatus(handle, &raw const status) };
}

fn create_event(manual: bool) -> Result<HANDLE, String> {
    let attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(core::mem::size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(0),
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 1,
    };
    // SAFETY: an attributes block that lives for the call, no name.
    let handle = unsafe { CreateEventW(&attributes, i32::from(manual), 0, std::ptr::null()) };
    if handle.is_null() {
        return Err(format!("CreateEventW failed, error={}", unsafe {
            GetLastError()
        }));
    }
    Ok(handle)
}

fn create_job() -> Result<Job, String> {
    // SAFETY: no attributes and no name.
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return Err(format!("CreateJobObjectW failed, error={}", unsafe {
            GetLastError()
        }));
    }
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { core::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: the structure and its size are correct for the class.
    let ok = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            u32::try_from(core::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
                .unwrap_or(0),
        )
    };
    if ok == 0 {
        let error = unsafe { GetLastError() };
        // SAFETY: the handle was just made and is owned here.
        unsafe { CloseHandle(job) };
        return Err(format!("kill-on-close could not be set, error={error}"));
    }
    Ok(Job(job))
}

fn active_console() -> Option<u32> {
    // SAFETY: no arguments, no state.
    let id = unsafe { WTSGetActiveConsoleSessionId() };
    (id != 0xFFFF_FFFF).then_some(id)
}

fn environment_block(
    file: &[(String, String)],
    log: HANDLE,
    end: HANDLE,
    attention: HANDLE,
) -> Vec<u16> {
    // The service's own environment first, the file's variables over it, and
    // the handle names last so a file cannot shadow them.
    let mut vars: Vec<(String, String)> = std::env::vars_os()
        .map(|(key, value)| {
            (
                key.to_string_lossy().into_owned(),
                value.to_string_lossy().into_owned(),
            )
        })
        .collect();
    let mut overlay = |key: &str, value: String| {
        vars.retain(|(name, _)| !name.eq_ignore_ascii_case(key));
        vars.push((key.to_string(), value));
    };
    for (key, value) in file {
        overlay(key, value.clone());
    }
    overlay(LOG_VAR, (log as usize).to_string());
    overlay(END_VAR, (end as usize).to_string());
    overlay(ATTENTION_VAR, (attention as usize).to_string());

    let mut block = Vec::new();
    for (key, value) in vars {
        if key.is_empty() || key.contains('=') || key.contains('\0') || value.contains('\0') {
            continue;
        }
        block.extend(format!("{key}={value}").encode_utf16());
        block.push(0);
    }
    block.push(0);
    block
}

/// Start one host in `session`, with the log as its standard output and the
/// two events inherited.
fn launch(state: &State, session: u32) -> Result<(HANDLE, u32), String> {
    let exe = std::env::current_exe()
        .map_err(|error| format!("this program's path could not be read: {error}"))?;
    let exe_w = wide(&exe.to_string_lossy());
    let mut command = format!("\"{}\"", exe.display());
    let args = state.args.trim();
    if !args.is_empty() {
        command.push(' ');
        command.push_str(args);
    }
    let mut command_w = wide(&command);
    let environment = environment_block(&state.environment, state.log, state.end, state.attention);
    let dir = program_data().ok_or_else(|| "ProgramData is not in the environment".to_string())?;
    let dir_w = wide(&dir.to_string_lossy());
    let desktop = wide("winsta0\\default");

    // The service's own token, moved into the console's session. A service is
    // the system account in session 0; the host must be that account in the
    // session whose desktop it captures.
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: the pseudo-handle for this process needs no closing, and the
    // token handle is written by the call.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &raw mut token) } == 0 {
        return Err(format!("OpenProcessToken failed, error={}", unsafe {
            GetLastError()
        }));
    }
    let mut primary: HANDLE = std::ptr::null_mut();
    // SAFETY: a token this process owns; the duplicate is written by the call.
    let duplicated = unsafe {
        DuplicateTokenEx(
            token,
            TOKEN_ALL_ACCESS,
            std::ptr::null(),
            SecurityImpersonation,
            TokenPrimary,
            &raw mut primary,
        )
    };
    // SAFETY: the handle came from OpenProcessToken and is closed once.
    unsafe { CloseHandle(token) };
    if duplicated == 0 {
        return Err(format!("DuplicateTokenEx failed, error={}", unsafe {
            GetLastError()
        }));
    }
    let session_value = session;
    // SAFETY: the value is four bytes as TokenSessionId requires, and lives
    // for the call. A service process holds the privilege this needs.
    let moved = unsafe {
        SetTokenInformation(
            primary,
            TokenSessionId,
            (&raw const session_value).cast(),
            u32::try_from(core::mem::size_of::<u32>()).unwrap_or(0),
        )
    };
    if moved == 0 {
        let error = unsafe { GetLastError() };
        // SAFETY: the duplicate is owned here.
        unsafe { CloseHandle(primary) };
        return Err(format!(
            "the token was not moved to session {session}, error={error}"
        ));
    }

    let mut startup: STARTUPINFOEXW = unsafe { core::mem::zeroed() };
    startup.StartupInfo.cb = u32::try_from(core::mem::size_of::<STARTUPINFOEXW>()).unwrap_or(0);
    startup.StartupInfo.lpDesktop = desktop.as_ptr() as *mut u16;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = std::ptr::null_mut();
    startup.StartupInfo.hStdOutput = state.log;
    startup.StartupInfo.hStdError = state.log;

    let mut size: usize = 0;
    // SAFETY: the first call asks for the size, as documented.
    unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &raw mut size) };
    let mut storage = vec![0u8; size];
    let list = storage.as_mut_ptr().cast();
    // SAFETY: a buffer of the size the first call gave.
    if unsafe { InitializeProcThreadAttributeList(list, 1, 0, &raw mut size) } == 0 {
        // SAFETY: the duplicate is owned here.
        unsafe { CloseHandle(primary) };
        return Err(format!(
            "the attribute list was not made, error={}",
            unsafe { GetLastError() }
        ));
    }
    let handles: [HANDLE; 3] = [state.log, state.end, state.attention];
    // SAFETY: the list is initialized and the handle array lives for the call.
    let listed = unsafe {
        UpdateProcThreadAttribute(
            list,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            handles.as_ptr().cast(),
            core::mem::size_of_val(&handles),
            std::ptr::null_mut(),
            std::ptr::null(),
        )
    };
    startup.lpAttributeList = list;
    if listed == 0 {
        // SAFETY: the list and the token are owned here.
        unsafe {
            DeleteProcThreadAttributeList(list);
            CloseHandle(primary);
        }
        return Err(format!("the handle list was not set, error={}", unsafe {
            GetLastError()
        }));
    }

    let mut process: PROCESS_INFORMATION = unsafe { core::mem::zeroed() };
    let flags = CREATE_SUSPENDED
        | CREATE_NO_WINDOW
        | CREATE_UNICODE_ENVIRONMENT
        | EXTENDED_STARTUPINFO_PRESENT;
    // SAFETY: every pointer lives for the call, the environment block is
    // terminated as the flag says, and the structures are the documented ones.
    let created = unsafe {
        CreateProcessAsUserW(
            primary,
            exe_w.as_ptr(),
            command_w.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
            flags,
            environment.as_ptr().cast(),
            dir_w.as_ptr(),
            &startup.StartupInfo,
            &raw mut process,
        )
    };
    let error = unsafe { GetLastError() };
    // SAFETY: the list and the token are owned here and not used again.
    unsafe {
        DeleteProcThreadAttributeList(list);
        CloseHandle(primary);
    }
    if created == 0 {
        return Err(format!(
            "CreateProcessAsUserW failed, session={session} error={error}"
        ));
    }
    // SAFETY: the job and the process handle are owned here.
    if unsafe { AssignProcessToJobObject(state.job.0, process.hProcess) } == 0 {
        lowlat_common::log_warn!(
            "lowlatd: the host was not put in the job, error={}",
            unsafe { GetLastError() }
        );
    }
    // SAFETY: the thread handle came from the creation and is closed once.
    unsafe {
        ResumeThread(process.hThread);
        CloseHandle(process.hThread);
    }
    Ok((process.hProcess, process.dwProcessId))
}

/// The machine's policy for the secure attention call, set around one call.
///
/// The value is written only when it does not already allow services, put back
/// afterwards, and deleted when it was absent. A write refused with
/// `ACCESS_DENIED` is a policy the service must not change: the call is made
/// only if the value already allows it, and otherwise refused with a line
/// naming the policy (docs/impl-plan-windows.md W2.5, D8).
fn secure_attention() {
    let key = wide(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System");
    let value_name = wide("SoftwareSASGeneration");
    let mut key_handle = std::ptr::null_mut();
    // SAFETY: a terminated subkey and a handle written by the call.
    let rc = unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            0,
            KEY_QUERY_VALUE | KEY_SET_VALUE | KEY_WOW64_64KEY,
            &raw mut key_handle,
        )
    };
    if rc != ERROR_SUCCESS {
        lowlat_common::log_warn!("lowlatd: the SAS policy key was not opened, error={rc}");
        return;
    }
    let mut value: u32 = 0;
    let mut kind: u32 = 0;
    let mut size = u32::try_from(core::mem::size_of::<u32>()).unwrap_or(0);
    let mut existed = true;
    // SAFETY: the value buffer is a u32 and `size` says so.
    let rc = unsafe {
        RegQueryValueExW(
            key_handle,
            value_name.as_ptr(),
            std::ptr::null_mut(),
            &raw mut kind,
            (&raw mut value).cast(),
            &raw mut size,
        )
    };
    if rc == ERROR_FILE_NOT_FOUND {
        existed = false;
        value = 0;
    } else if rc != ERROR_SUCCESS || kind != REG_DWORD || size != 4 {
        lowlat_common::log_warn!(
            "lowlatd: the SAS policy value was not read, error={rc} kind={kind}"
        );
        // SAFETY: the key is open here.
        unsafe { RegCloseKey(key_handle) };
        return;
    }

    let mut changed = false;
    if value & 1 == 0 {
        let allowed = value | 1;
        // SAFETY: a u32 value and its size, on an open key.
        let rc = unsafe {
            RegSetValueExW(
                key_handle,
                value_name.as_ptr(),
                0,
                REG_DWORD,
                (&raw const allowed).cast(),
                u32::try_from(core::mem::size_of::<u32>()).unwrap_or(0),
            )
        };
        if rc == ERROR_ACCESS_DENIED {
            // SAFETY: the key is open here.
            unsafe { RegCloseKey(key_handle) };
            lowlat_common::log_warn!(
                "lowlatd: the SAS policy is managed and does not allow services, refused=policy"
            );
            return;
        }
        if rc != ERROR_SUCCESS {
            // SAFETY: the key is open here.
            unsafe { RegCloseKey(key_handle) };
            lowlat_common::log_warn!("lowlatd: the SAS policy was not set, error={rc}");
            return;
        }
        changed = true;
    }

    // SAFETY: sas.dll's only call, with the flag the established host passes.
    unsafe { SendSAS(0) };

    // Put back what was found, on every path after a change, including one
    // where the call above did nothing.
    if changed {
        // SAFETY: the key is open and the value is a u32 or absent.
        unsafe {
            if existed {
                RegSetValueExW(
                    key_handle,
                    value_name.as_ptr(),
                    0,
                    REG_DWORD,
                    (&raw const value).cast(),
                    u32::try_from(core::mem::size_of::<u32>()).unwrap_or(0),
                );
            } else {
                RegDeleteValueW(key_handle, value_name.as_ptr());
            }
            RegCloseKey(key_handle);
        }
    } else {
        // SAFETY: the key is open here.
        unsafe { RegCloseKey(key_handle) };
    }
    lowlat_common::log_info!("lowlatd: secure attention, asked=service");
}

#[link(name = "sas")]
unsafe extern "system" {
    fn SendSAS(as_user: i32);
}

/// The service manager's entry point.
unsafe extern "system" fn service_main(_argc: u32, _argv: *mut *mut u16) {
    let pointer = STATE.load(Ordering::Acquire);
    if pointer.is_null() {
        return;
    }
    // SAFETY: the state was put here before the dispatcher started and outlives
    // this thread's use of it.
    let state = unsafe { &mut *pointer };

    // The log first, so a failure below is readable; then the events, before
    // the handler can be asked to signal one.
    match open_log() {
        Ok(handle) => {
            state.log = handle;
            LOG_HANDLE.store(handle as isize, Ordering::Release);
            install_log_sink();
        }
        Err(error) => {
            lowlat_common::log_error!("lowlatd: the log was not opened: {error}");
        }
    }
    state.stop = match create_event(true) {
        Ok(handle) => handle,
        Err(error) => {
            lowlat_common::log_error!("lowlatd: {error}");
            return;
        }
    };
    state.wake = match create_event(false) {
        Ok(handle) => handle,
        Err(error) => {
            lowlat_common::log_error!("lowlatd: {error}");
            return;
        }
    };
    state.end = match create_event(true) {
        Ok(handle) => handle,
        Err(error) => {
            lowlat_common::log_error!("lowlatd: {error}");
            return;
        }
    };
    state.attention = match create_event(false) {
        Ok(handle) => handle,
        Err(error) => {
            lowlat_common::log_error!("lowlatd: {error}");
            return;
        }
    };

    let name = wide(SERVICE_NAME);
    // SAFETY: a terminated name and a context that outlives the service.
    let status =
        unsafe { RegisterServiceCtrlHandlerExW(name.as_ptr(), Some(handler), pointer.cast()) };
    if status.is_null() {
        lowlat_common::log_error!(
            "lowlatd: the service handler was not registered, error={}",
            unsafe { GetLastError() }
        );
        return;
    }
    report(status, SERVICE_START_PENDING, 0, 1, 0);

    lowlat_common::log_info!(
        "lowlatd: service start utc={} pid={} session=0",
        utc_now(),
        std::process::id()
    );

    // The configuration is the file the login tool writes. A file that is not
    // there is not fatal: the host exits with its own line and this loop leaves
    // it down, exactly as a host that has not been logged in.
    if let Some(path) = lowlat_common::env_file::service_path() {
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                state.environment = lowlat_common::env_file::parse(&text);
                state.args = state
                    .environment
                    .iter()
                    .find(|(key, _)| key == "LOWLATD_ARGS")
                    .map(|(_, value)| value.clone())
                    .unwrap_or_default();
            }
            Err(error) => {
                lowlat_common::log_warn!(
                    "lowlatd: configuration not read, path={} error={error}",
                    path.display()
                );
            }
        }
    }
    // The first word of the arguments chooses the host's role, so it must be a
    // flag: a file that says `display` or `uninstall` must not turn the host
    // into one of those (docs/impl-plan-windows.md W2.5, R4).
    if !state.args.trim().is_empty() && !state.args.trim_start().starts_with('-') {
        lowlat_common::log_error!(
            "lowlatd: LOWLATD_ARGS starts with a role word, refusing to start the host"
        );
        report(status, SERVICE_STOPPED, 0, 0, 0);
        return;
    }

    let controls = SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_PRESHUTDOWN | SERVICE_ACCEPT_SESSIONCHANGE;
    report(status, SERVICE_RUNNING, controls, 0, 0);

    let clock = Time::now();
    let mut host: Option<HostRun> = None;
    let mut last_exit: Option<Exit> = None;

    loop {
        // A stop, a pre-shutdown or a shutdown all mean this service is going.
        // SAFETY: an event this service owns.
        if unsafe { WaitForSingleObject(state.stop, 0) } == WAIT_OBJECT_0 {
            report(status, SERVICE_STOP_PENDING, controls, 2, 5000);
            break;
        }

        let now_ms = elapsed_ms(clock);
        let host_state = match &host {
            None => HostState::None,
            Some(run) => match run.asked_ms {
                Some(since_ms) => HostState::AskedEnd { since_ms },
                None => HostState::Running {
                    session: run.session,
                },
            },
        };
        let step = next(
            Pass {
                console: active_console(),
                host: host_state,
                last_exit,
            },
            now_ms,
        );
        match step {
            Step::Wait => {}
            Step::Start { session } => {
                if let Some(dir) = program_data() {
                    if rotate_log(&dir) {
                        let previous = state.log;
                        match open_log() {
                            Ok(handle) => {
                                state.log = handle;
                                LOG_HANDLE.store(handle as isize, Ordering::Release);
                                if !previous.is_null() {
                                    // SAFETY: the service's own old handle.
                                    unsafe { CloseHandle(previous) };
                                }
                                lowlat_common::log_info!("lowlatd: log rotated");
                            }
                            Err(error) => {
                                lowlat_common::log_warn!("lowlatd: {error}");
                            }
                        }
                    }
                }
                // **The end request is cleared before every host.** The event
                // is manual-reset and each host inherits the same one, so a
                // request left set for the last host would end this one the
                // moment it started.
                // SAFETY: an event this service owns.
                unsafe { ResetEvent(state.end) };
                match launch(state, session) {
                    Ok((process, pid)) => {
                        lowlat_common::log_info!(
                            "lowlatd: host start pid={pid} session={session} args={:?} utc={}",
                            state.args,
                            utc_now()
                        );
                        host = Some(HostRun {
                            process,
                            session,
                            asked_ms: None,
                        });
                        last_exit = None;
                    }
                    Err(error) => {
                        lowlat_common::log_error!("lowlatd: host start failed: {error}");
                        last_exit = Some(Exit {
                            at_ms: now_ms,
                            code: 1,
                            session,
                            asked: false,
                        });
                    }
                }
            }
            Step::AskEnd => {
                if let Some(run) = &mut host {
                    // SAFETY: an event this service owns.
                    unsafe { SetEvent(state.end) };
                    run.asked_ms = Some(now_ms);
                    lowlat_common::log_info!("lowlatd: host asked to end, session={}", run.session);
                }
            }
            Step::Terminate => {
                if let Some(run) = host.take() {
                    // SAFETY: a process this service started and still owns.
                    unsafe {
                        TerminateProcess(run.process, 1);
                        CloseHandle(run.process);
                    }
                    lowlat_common::log_warn!(
                        "lowlatd: host did not end in time, terminated session={}",
                        run.session
                    );
                    last_exit = Some(Exit {
                        at_ms: now_ms,
                        code: 1,
                        session: run.session,
                        asked: true,
                    });
                }
            }
        }

        let mut handles: Vec<HANDLE> = vec![state.stop, state.wake, state.attention];
        if let Some(run) = &host {
            handles.push(run.process);
        }
        // SAFETY: every handle is an event or process this service owns, and
        // the array lives for the call.
        let waited = unsafe {
            WaitForMultipleObjects(
                u32::try_from(handles.len()).unwrap_or(0),
                handles.as_ptr(),
                0,
                PASS_MS,
            )
        };
        match waited {
            WAIT_OBJECT_0 => {}
            i if i == WAIT_OBJECT_0 + 1 => {}
            i if i == WAIT_OBJECT_0 + 2 => secure_attention(),
            i if i == WAIT_OBJECT_0 + 3 => {
                if let Some(run) = host.take() {
                    let mut code = 0u32;
                    // SAFETY: a process this service started and owns.
                    let read = unsafe { GetExitCodeProcess(run.process, &raw mut code) } != 0;
                    // SAFETY: the handle is closed once.
                    unsafe { CloseHandle(run.process) };
                    lowlat_common::log_info!(
                        "lowlatd: host exit code={} session={} read={}",
                        code,
                        run.session,
                        u8::from(read)
                    );
                    last_exit = Some(Exit {
                        at_ms: elapsed_ms(clock),
                        code: if read { code } else { 1 },
                        session: run.session,
                        asked: run.asked_ms.is_some(),
                    });
                }
            }
            WAIT_TIMEOUT => {}
            _ => lowlat_common::log_warn!("lowlatd: the service's wait failed, error={}", unsafe {
                GetLastError()
            }),
        }
    }

    // The host is asked to end and given the same five seconds the table gives
    // it; the job takes it if it does not answer.
    if let Some(run) = host.take() {
        // SAFETY: an event this service owns.
        unsafe { SetEvent(state.end) };
        // SAFETY: a process this service started and owns.
        let ended = unsafe { WaitForSingleObject(run.process, END_BOUND_WAIT) } != WAIT_TIMEOUT;
        if !ended {
            lowlat_common::log_warn!("lowlatd: host did not end at stop, terminated");
            // SAFETY: as above.
            unsafe { TerminateProcess(run.process, 1) };
        }
        // SAFETY: the handle is closed once.
        unsafe { CloseHandle(run.process) };
    }
    report(status, SERVICE_STOPPED, 0, 0, 0);
}

/// Run under the service manager, which is the only way this role is entered.
///
/// A program run outside it gets the dispatcher's own refusal, which is the
/// first of the two answers this role can give.
pub(crate) fn run_service() -> Result<(), Box<dyn std::error::Error>> {
    let state = Box::new(State {
        stop: std::ptr::null_mut(),
        wake: std::ptr::null_mut(),
        end: std::ptr::null_mut(),
        attention: std::ptr::null_mut(),
        log: std::ptr::null_mut(),
        job: create_job().map_err(|error| format!("the job was not made: {error}"))?,
        args: String::new(),
        environment: Vec::new(),
    });
    let pointer = Box::into_raw(state);
    STATE.store(pointer, Ordering::Release);
    let mut name = wide(SERVICE_NAME);
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: name.as_mut_ptr(),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW {
            lpServiceName: std::ptr::null_mut(),
            lpServiceProc: None,
        },
    ];
    // SAFETY: the table is null-terminated and lives for the call, which
    // returns when the service stops.
    let ok = unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) };
    // SAFETY: the state was made by Box::into_raw above and the dispatcher has
    // returned, so no service thread still uses it.
    let state = unsafe { Box::from_raw(pointer) };
    STATE.store(std::ptr::null_mut(), Ordering::Release);
    if ok == 0 {
        let error = unsafe { GetLastError() };
        if error == ERROR_FAILED_SERVICE_CONTROLLER_CONNECT {
            return Err(
                "not running under the service manager; lowlatd install registers it".into(),
            );
        }
        return Err(format!("the service dispatcher failed, error={error}").into());
    }
    // SAFETY: handles the service made and still owns; each is closed once.
    unsafe {
        for handle in [
            state.stop,
            state.wake,
            state.end,
            state.attention,
            state.log,
        ] {
            if !handle.is_null() {
                CloseHandle(handle);
            }
        }
    }
    Ok(())
}

/// Install and uninstall live beside this module, where the service manager's
/// registration, the files' access and the driver's package are the subject.
pub(crate) fn install() -> Result<(), Box<dyn std::error::Error>> {
    crate::windows_install::install()
}

pub(crate) fn uninstall() -> Result<(), Box<dyn std::error::Error>> {
    crate::windows_install::uninstall()
}

/// The host's side: take the handles the service gave it, clear their
/// inheritance so nothing it starts gets them, and install the log sink.
pub(crate) fn host_handles() {
    let read = |name: &str| {
        std::env::var(name)
            .ok()
            .and_then(|text| text.trim().parse::<usize>().ok())
            .filter(|value| *value != 0)
            .map(|value| value as HANDLE)
    };
    let log = read(LOG_VAR);
    let end = read(END_VAR);
    let attention = read(ATTENTION_VAR);
    if let Some(handle) = log {
        // SAFETY: a handle inherited from the service; clearing its inherit
        // flag keeps it out of anything this host starts.
        unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) };
        LOG_HANDLE.store(handle as isize, Ordering::Release);
        install_log_sink();
    }
    if let Some(handle) = end {
        // SAFETY: as above.
        unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) };
        END_HANDLE.store(handle as isize, Ordering::Release);
    }
    if let Some(handle) = attention {
        // SAFETY: as above.
        unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) };
        ATTENTION_HANDLE.store(handle as isize, Ordering::Release);
    }
}

/// Ask the service for the secure attention call. False when this host was
/// not started by the service.
pub(crate) fn attention() -> bool {
    let handle = ATTENTION_HANDLE.load(Ordering::Acquire) as HANDLE;
    if handle.is_null() {
        return false;
    }
    // SAFETY: an event the service made and this process inherited.
    (unsafe { SetEvent(handle) }) != 0
}

/// The event the service signals to end this host, as a stop flag.
pub(crate) fn stop_signal() -> crate::service::Stop {
    let stop = crate::service::Stop::new();
    let end = END_HANDLE.load(Ordering::Acquire) as HANDLE;
    if end.is_null() {
        return stop;
    }
    let thread_stop = stop.clone();
    let end = end as isize;
    let spawned = std::thread::Builder::new()
        .name("lowlatd-end".into())
        .spawn(move || {
            let end = end as HANDLE;
            // SAFETY: an event the service made and this process inherited.
            unsafe { WaitForSingleObject(end, INFINITE) };
            thread_stop.fire();
        });
    if let Err(error) = spawned {
        lowlat_common::log_warn!("lowlatd: the end event was not watched: {error}");
    }
    stop
}

#[cfg(test)]
mod tests {
    use super::{CloseHandle, Exit, HostState, Pass, SESSION_ENDED, Step, next, open_append};

    const T0: f64 = 0.0;

    /// The service renames the log while it still holds its handle; a handle
    /// that does not share deletion fails that rename, and the log grows for
    /// good.
    #[test]
    fn the_log_can_be_renamed_while_its_handle_is_open() {
        let dir = std::env::temp_dir().join(format!("lowlatd-log-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("lowlatd.log");
        let handle = open_append(&path).expect("open");
        let renamed = std::fs::rename(&path, dir.join("lowlatd.log.1"));
        // SAFETY: the handle was opened above and is closed once.
        unsafe { CloseHandle(handle) };
        let _ = std::fs::remove_dir_all(&dir);
        renamed.expect("renamed while held");
    }

    fn pass(console: Option<u32>, host: HostState, last_exit: Option<Exit>) -> Pass {
        Pass {
            console,
            host,
            last_exit,
        }
    }

    #[test]
    fn a_first_pass_with_a_console_starts() {
        assert_eq!(
            next(pass(Some(1), HostState::None, None), T0),
            Step::Start { session: 1 }
        );
    }

    #[test]
    fn no_console_starts_nothing_and_ends_a_stray_host() {
        assert_eq!(next(pass(None, HostState::None, None), T0), Step::Wait);
        assert_eq!(
            next(pass(None, HostState::Running { session: 1 }, None), T0),
            Step::AskEnd
        );
    }

    #[test]
    fn a_host_in_the_console_is_left_alone_and_one_elsewhere_is_asked_to_end() {
        assert_eq!(
            next(pass(Some(2), HostState::Running { session: 2 }, None), T0),
            Step::Wait
        );
        assert_eq!(
            next(pass(Some(2), HostState::Running { session: 1 }, None), T0),
            Step::AskEnd
        );
    }

    fn exit(code: u32, session: u32, asked: bool) -> Exit {
        Exit {
            at_ms: T0,
            code,
            session,
            asked,
        }
    }

    #[test]
    fn a_clean_exit_waits_for_the_console_to_move() {
        let exit = exit(0, 1, false);
        assert_eq!(
            next(pass(Some(1), HostState::None, Some(exit)), 7001.0),
            Step::Wait
        );
        assert_eq!(
            next(pass(Some(2), HostState::None, Some(exit)), 7001.0),
            Step::Start { session: 2 }
        );
    }

    /// A console that read none for a moment and came back to the same number
    /// is still owed a host, and one that moved gets it at once.
    #[test]
    fn a_host_that_ended_when_asked_is_followed_at_once() {
        let exit = exit(0, 1, true);
        assert_eq!(
            next(pass(Some(1), HostState::None, Some(exit)), T0),
            Step::Start { session: 1 }
        );
        assert_eq!(
            next(pass(Some(2), HostState::None, Some(exit)), T0),
            Step::Start { session: 2 }
        );
        assert_eq!(
            next(pass(None, HostState::None, Some(exit)), T0),
            Step::Wait
        );
    }

    /// A logoff's end: at once into a console that moved, and after the
    /// failure's delay into the same number, which may be a new session.
    #[test]
    fn a_host_ended_with_its_session_is_followed_even_on_the_same_number() {
        let exit = exit(SESSION_ENDED, 1, false);
        assert_eq!(
            next(pass(Some(2), HostState::None, Some(exit)), T0),
            Step::Start { session: 2 }
        );
        assert_eq!(
            next(pass(Some(1), HostState::None, Some(exit)), 1001.0),
            Step::Wait
        );
        assert_eq!(
            next(pass(Some(1), HostState::None, Some(exit)), 2001.0),
            Step::Start { session: 1 }
        );
    }

    #[test]
    fn a_failed_exit_is_restarted_after_two_seconds() {
        // On its own, and terminated at the bound after being asked.
        for asked in [false, true] {
            let exit = exit(1, 1, asked);
            assert_eq!(
                next(pass(Some(1), HostState::None, Some(exit)), 1001.0),
                Step::Wait
            );
            assert_eq!(
                next(pass(Some(1), HostState::None, Some(exit)), 2001.0),
                Step::Start { session: 1 }
            );
        }
    }

    #[test]
    fn an_asked_host_is_waited_for_then_terminated() {
        let asked = pass(Some(1), HostState::AskedEnd { since_ms: T0 }, None);
        assert_eq!(next(asked, 4999.0), Step::Wait);
        assert_eq!(next(asked, 5001.0), Step::Terminate);
    }
}
