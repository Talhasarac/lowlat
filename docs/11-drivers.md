# 11 - Drivers

**Status:** planned 2026-10-15, at W3's interview; the display's driver and its package built
the same day (W3.0), and its controller (W3.1); the rest planned. Built by [impl-plan-windows.md](impl-plan-windows.md) W3,
where the decisions and their reasons are recorded; this document is the design they add up to.

On Windows the host carries drivers of its own where the system offers no interface for what a
host needs: a display, for a machine with no monitor and for pictures the host reads without the
system's own duplication pass (§1), and later an input device for the guests' mice, keyboards and
pads (§2). Both are user-mode drivers: a library the system's user-mode driver host loads, the
system's own kernel components beneath it, nothing of this project's in the kernel. Their C
interface is the library's third half ([06](06-api.md)); the host's side of the display is
[05 §2](05-host.md); installing and signing them is [07 §10](07-platforms.md).

## §1 The display

### §1.1 The device

- One root-enumerated device of the display class: an indirect display adapter with up to
  sixteen monitors. The system composes each monitor's desktop on a GPU and hands the driver each
  composed picture; the driver shows them nowhere.
- Written in Rust in this workspace. The declarations of the user-mode driver framework and of
  the indirect display class are generated once from the driver kit's headers, against the
  oldest versions the floor needs -- the class at 1.4, the framework at 2.25 -- and committed; a
  later call is made only behind its availability check. A callback never unwinds into the
  system's code: a panic ends the driver's host process, which the system starts again.
- A host process of its own, never shared with another driver, so the process the host takes
  handles from holds this driver alone; raised in priority, since its wake lies on every frame's
  path (measured with and without).
- The floor is Windows 10 1903, the first release whose display class lets a driver name the GPU
  to render on.
- Its log goes to the debugger stream: the driver's host process has no console and no file it
  may write.

### §1.2 Two interfaces

| Interface | Who opens it | What it carries |
|---|---|---|
| control | any interactive user, administrators, the system account | a plug, bound or kept, at a slot asked for or any; an unplug; a replug with a mode; the list; a reload of the file; the version |
| frames | the system account alone | the section, the buffers and the fences of §1.6 |

**The frame interface is a screen-capture channel**, refused at open to any caller but the system
account, since one device carries one access list. Plugging a display asks no more than a person
at the desk may do, so the control interface is open to any interactive user, and ownership
(§1.3) keeps one user's monitors from another's requests. Both are answered on the display
class's own request path.

### §1.3 Monitors, and how long they live

A monitor lives one of two ways, chosen when it is plugged:

- **Bound**: owned by the handle that plugged it. Only that handle re-modes or unplugs it, and
  the handle's end -- a close, or its process's exit or crash -- unplugs it at once. The host's
  own display is bound to the host, so a host that ends, however it ends, leaves no display
  behind.
- **Kept**: owned by no handle. Any controller may re-mode or unplug it; it lives until removed
  or until the driver stops, and it may be marked to come back at start (§1.4).

**Nothing keeps a monitor alive by traffic.** A driver whose monitors belong to nobody can read
only a stream of pings as somebody still wanting them, and silence then removes every monitor;
any stall of the pinging thread -- a debugger, a resume from sleep, a starved thread -- takes a
display away mid-session. Ownership by handle removes a monitor exactly when its owner goes.

**Each monitor is a slot**, 0 to 15, which a plug may ask for. Its identity -- its container and
the serial in its EDID -- derives from the slot and never changes, so the system keeps each
display's resolution, position and scale across a replug and a reboot. The EDID is generated once
per slot, or is a real monitor's dumped EDID given in the file (§1.4), its serial patched per
slot. **Each monitor is reported as a DisplayPort monitor**, as its EDID says. The host's own
display takes the highest free slot, so it keeps one identity and never takes an application's
first free one.

### §1.4 What persists

One file, `%ProgramData%\lowlat\display\displays.conf`, in a folder of its own, holds what
outlives the driver, a setting a line: the GPU the monitors render on (`gpu`), whether the
pointer is drawn into the pictures (`pointer`), the custom modes (`mode`, §1.5), the kept
monitors marked to come back, each at its slot and mode (`monitor`), and an optional EDID
(`edid`). **The driver reads it when the adapter starts, before any request, and again when a
controller asks; it never writes it** -- a user-mode driver has no right to -- and the
controller writes it, under a lock any user's controller takes, replacing it whole. **Any user
may write it, so the driver reads it as hostile**: every line bounded, a line it cannot read
skipped and counted, a word it does not know skipped quietly so an older driver reads a newer
file, nothing in it able to stop the driver starting. The folder, made by the installer, lets
users change it, so an unelevated program keeps a display, and lets the driver's own account
read it; the host's configuration beside it stays readable by the system account and
administrators alone ([07 §10](07-platforms.md)). The GPU is named by vendor and device, since
an adapter's identifier changes across boots (§1.7).

### §1.5 Modes

- **A monitor's modes are fixed when it arrives**: a built-in list of common sizes at common
  rates, the custom modes, and the mode the plug asks for, kept within about ninety entries -- a
  monitor with around a hundred has been seen not to arrive. The preferred one is the plug's.
- **A listed mode is set through the system's display configuration.** A kept monitor's is saved
  in the system's database of arrangements, as any monitor's is, so it comes back after a reboot
  where it was; a bound monitor's never is. **An arrangement in which only virtual displays would
  be active is never saved**: one that only a virtual display can satisfy holds a later boot
  waiting for the driver.
- **A mode not listed is a replug of that monitor alone**, with the mode added first, then set
  once the replacement is ready. Requests during a replug are coalesced into one more, and a
  replug waits until the old monitor's swapchain is torn down. Such a mode is not kept; a custom
  mode is. Updating a live monitor's list in place leaves the system's cached modes stale, so it
  is not done.
- The adapter asks the system for the smallest mode that holds the desktop, without which a
  monitor is sometimes refused.
- **A mode asks for no display bandwidth.** The adapter declares no budget, and the system keeps
  the active modes' sum within it, so a mode asking for any is never made active.
- **A plugged monitor is made active by the controller when the system does not.** The system
  extends a set of monitors it has not seen and restores a set it saved, at once; a set it
  remembers with nothing saved it leaves dark. The controller then adds the monitor's path to
  the active ones, saved as a mode is. **A saved arrangement overrules the mode a plug asks
  for**, so the controller reads the mode back and sets the one asked for.
- **The host's display holds its mode** for as long as the host runs: another monitor's arrival
  and a program leaving exclusive fullscreen each put a saved mode back on a monitor, so the host
  puts its own back whenever the system's differs.

### §1.6 Pictures

The driver converts nothing: formats, depth, full chroma and scaling stay the host's, so a change
to them never re-signs the driver. The display is standard dynamic range, and every buffer is
eight-bit BGRA. **The host reads the system's own buffer** ([05 §2](05-host.md)):

- **Per swapchain**, the driver shares each distinct buffer once, read-only, with a fence of the
  device the swapchain was given, and publishes their handles in a section the host maps. The
  host takes the handles from the driver's process and keeps a view per buffer until the
  swapchain is replaced.
- **Per picture**, the driver acquires, notes the time, signals the fence, publishes the buffer
  with its sequence and frame number, and finishes the frame at once; then it waits on three
  things together -- the host's release reaching that sequence, the swapchain's end, and a bound
  -- before it acquires again, which is when the system may write that buffer next.
- **The host claims a sequence before reading it**, waits for the fence on its device, converts
  into the encoder's input and releases the sequence: after the conversion, not after the encode.
- **At the bound** the driver lets go of an unclaimed picture, which the host then skips, and of
  a claimed one too, counting it. The bound sits well inside a frame period, since a held buffer
  holds back the compositor, and is set by measurement.
- **The driver reads the host's release on the processor only** and never waits for it on the
  device, so a host that dies cannot stall the display. **The swapchain's end ends a hold at
  once**: a frame held across a monitor's departure stalls the system's teardown until its
  watchdog ends the driver.
- **A picture's time is the driver's acquire.** The time the system attaches to a frame is when
  it should be shown, which lies ahead of the hand-over.
- A frame repeating the last frame number is no new picture, and the system's marker for a still
  desktop is a present of the same picture; after it the system sends nothing, and the host's own
  clock keeps the stream's pace.
- Each acquired buffer carries a reference the driver releases at its next acquire.

If the system's buffer will not open in the host, or holding it costs more than it saves, the
driver converts into targets of its own, then copies, by measurement.

### §1.7 The GPU, and the pointer

- **One GPU for every monitor**, since the display class takes one for the whole adapter and
  re-creates every swapchain when it changes: the file's, by vendor and device, else the first
  hardware GPU in the system's high-performance order, named by the driver at its start before
  any monitor arrives. The system may choose another, and each swapchain says which it got; the
  host builds on that one and never names a GPU itself.
- **The system lists the monitor's output under the GPU that renders it**, as that GPU's own
  output, not under the display adapter.
- **The pointer is a hardware cursor**, alpha and full XOR up to 256 a side -- the largest the
  wire carries -- so the pictures carry no pointer, and the host reads the shape, the hotspot and
  the visibility from the driver, the position and the show state as for any display
  ([05 §8](05-host.md)). The file's setting draws it into the pictures instead, for software that
  wants it there.

### §1.8 Lifecycle

- A swapchain comes with a device on the GPU it renders on; the driver sets the device first and
  does the rest after. A device that cannot be made abandons the swapchain; a swapchain that
  refuses its device, or is lost, ends its worker; either way the system assigns another. The
  swapchain is deleted on every exit of its worker.
- **The system takes back the first one to three swapchains after a mode is set** -- unassigned
  before the device is set, or lost at the first acquire -- and assigns another within about a
  quarter of a second; each ends and is deleted at once.
- An unassignment takes the worker from its monitor under the lock the callbacks share and waits,
  outside it, for the worker to delete its swapchain; an assignment first ends a worker still
  there, so a deletion that calls the unassignment back would find nothing. A replacement monitor is
  never created inside the old one's unassignment.
- A display switched off takes the monitor's path out of the topology; the host keeps the display
  awake while it captures.
- An update of the driver: the host closes its handle when the driver asks to be removed, and
  opens it again when the driver is back.
- A GPU reset reassigns every swapchain; no monitor departs during one.

### §1.9 The controller, and people

The controller is the driver's control interface from user mode, behind the library's third half
([06 §3c](06-api.md)), the host's display and `lowlatd display`. **Every call waits for the
system, within about three seconds**: a plug until the monitor is active at its mode, a mode
until it is current, an unplug until the monitor has gone; past the bound a status of its own
says the monitor is plugged and the system has not finished. Calls on one handle take turns.

`lowlatd display` drives the same controller, so a person with no program of their own manages
displays from a shell: `list`; `add WxH[@Hz] [--restore] [--slot N]`; `hold WxH[@Hz] [--slot
N] [--for SECONDS]`, a bound monitor for as long as the command runs; `remove SLOT`; `mode SLOT
WxH[@Hz]`; and the file's settings, `modes [add|remove WxH@Hz]`, `gpu [auto|VVVV:DDDD]`,
`pointer [hardware|drawn]` and `edid [FILE|none]`. A display a person adds is kept, since a
command's process ends with it. A tray is later.

## §2 Input

**Deferred.** A HID device of the host's own for the guests' mice, keyboards and pads, whose input
the system takes as a device's rather than a program's. Until it is planned, input on Windows is
[05 §7](05-host.md)'s: the system's batch input call, and pads on a virtual bus as Xbox 360
pads.

## §3 Verification status

Planned 2026-10-15. **The driver and its package are built and checked live** (W3.0,
2026-10-15): installed silently with no test mode; a bound monitor plugged by an unelevated
program, made active and composing on the GPU the system named, at 1920x1080 at 60 and 2560x1440
at 120; unplugged by a request and by its handle's end, a killed process's included. **The controller is
built and checked live** (W3.1, 2026-10-15): kept and bound monitors from an unelevated shell
and through the C interface, a mode set listed and unlisted, a bound monitor refused to another
program and gone with its holder, a kept one back at its saved mode and place after the driver
restarted and again after a reboot, the file's settings changed. The
pictures' hand-over (§1.6) and reading the pointer (§1.7) are not built.
W3's gate ([impl-plan-windows.md](impl-plan-windows.md)) is the first check of the whole, and its
comparison decides whether the host reads pictures as §1.6 describes or through the system's
duplication of this display.
