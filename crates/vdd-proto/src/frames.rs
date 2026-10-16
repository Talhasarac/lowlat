//! The frame channel: what the display driver publishes of the pictures a
//! monitor is handed, in one page the host maps (docs/11-drivers.md section
//! 1.6).
//!
//! **One writer, any number of readers, and nobody waits on anybody.** The
//! driver's worker for a monitor writes the page under a sequence lock -- the
//! sequence odd while it writes -- and a reader that overlapped a write reads
//! again. Every word is an atomic, so a reader in another process that meets
//! a write half done reads values, never torn memory, and the sequence tells
//! it to read again. A reader maps the page read-only and writes nothing.
//!
//! The page names the monitor's swapchain -- its generation, the GPU it
//! renders on, its size and refresh, the fence the driver signals after each
//! acquire, each buffer as it is first seen -- and the latest picture: its
//! buffer, its sequence (the fence's value for it), its frame number and when
//! the driver acquired it. Its handles are values in the driver's process,
//! which a reader takes from that process (section 1.2).
//!
//! **A picture stays intact while the system cycles its other buffers**: the
//! system writes a buffer again only once the driver has acquired as many
//! newer pictures as the swapchain has buffers, less one, so a reader checks
//! [`Snapshot::overtaken`] after it has queued its read, and reads the latest
//! instead when the picture it took was overtaken.

// The crate takes no dependency, so the model checker's atomics are named
// here rather than through the shared shim; nothing else of it is needed.
#[cfg(not(loom))]
use core::sync::atomic::{AtomicU64, Ordering, fence};
#[cfg(loom)]
use loom::sync::atomic::{AtomicU64, Ordering, fence};

/// The channel's own version, in its page and in the answer to a request for
/// a monitor's frames; a reader refuses a page of another.
pub const VERSION: u32 = 1;

/// The page: one of the system's.
pub const PAGE: usize = 4096;

/// The most buffers of one swapchain the page names.
pub const MAX_BUFFERS: usize = 16;

/// What the page's first word holds once the driver has prepared it.
pub const MAGIC: u64 = u64::from_le_bytes(*b"lowlatfc");

// The page, word by word: the magic and the version, written once before any
// reader can have the page; the sequence; the snapshot's words under it.
const W_MAGIC: usize = 0;
const W_VERSION: usize = 1;
const W_SEQUENCE: usize = 2;
const W_DATA: usize = 3;

// The snapshot's words.
const D_EVENT: usize = 0;
const D_GENERATION: usize = 1;
const D_RENDER: usize = 2;
const D_SIZE: usize = 3;
const D_REFRESH: usize = 4;
const D_FENCE: usize = 5;
const D_BUFFERS: usize = 6;
const D_BUFFER: usize = 7;
const D_SEQUENCE: usize = D_BUFFER + MAX_BUFFERS;
const D_INDEX: usize = D_SEQUENCE + 1;
const D_FRAME: usize = D_SEQUENCE + 2;
const D_ACQUIRED: usize = D_SEQUENCE + 3;
const D_FLAGS: usize = D_SEQUENCE + 4;
const D_PICTURES: usize = D_SEQUENCE + 5;
const D_GAPS: usize = D_SEQUENCE + 6;
/// The snapshot's length in words.
pub const DATA: usize = D_SEQUENCE + 7;

/// The words of the page this version uses, from its start.
pub const WORDS: usize = W_DATA + DATA;

// The page holds every word this version uses.
const _: () = assert!(WORDS * 8 <= PAGE);

const REPEAT: u64 = 1;
const PROTECTED: u64 = 2;

/// How many times a reader tries before taking the writer to be stuck mid
/// write, which a writer that has died is.
const ATTEMPTS: u32 = 64;

/// The latest picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Picture {
    /// The fence's value for it, counted from one in each generation; zero
    /// before the first.
    pub sequence: u64,
    /// Which of the swapchain's buffers holds it.
    pub index: u32,
    /// The system's number for the picture; a repeat is no new picture.
    pub frame: u32,
    /// When the driver acquired it, on the system's performance counter.
    pub acquired: u64,
    /// Its frame number repeats the one before: the same picture again.
    pub repeat: bool,
    /// The system marked its buffer as holding protected content.
    pub protected: bool,
}

/// One consistent reading of the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Snapshot {
    /// The event the driver sets at each publish, a value in its process.
    pub event: u64,
    /// The swapchain's, one more for each the driver is given; zero while the
    /// monitor has none.
    pub generation: u64,
    /// The GPU the swapchain renders on: its adapter identifier, the high
    /// half above the low.
    pub render: u64,
    pub width: u32,
    pub height: u32,
    /// The refresh the system committed the monitor to, as a fraction.
    pub refresh: (u32, u32),
    /// The fence the driver signals after each acquire, at the picture's
    /// sequence, a value in its process.
    pub fence: u64,
    /// The swapchain's buffers seen so far, each a handle value in the
    /// driver's process, in the order the system first handed them over.
    pub buffers: u32,
    pub buffer: [u64; MAX_BUFFERS],
    pub picture: Picture,
    /// Pictures the driver acquired since it began sharing this monitor's, and
    /// frame numbers skipped between them.
    pub pictures: u64,
    pub gaps: u64,
}

impl Snapshot {
    /// Whether the buffer of the picture a reader took at `sequence` of
    /// `generation` may have been written again since. **Only once the driver
    /// has acquired as many newer pictures as the swapchain has buffers, less
    /// one**: until then the system writes its other buffers. A swapchain
    /// whose buffers are not all seen yet counts as having fewer, which errs
    /// early. Another generation's buffers the system no longer writes at
    /// all.
    #[must_use]
    pub fn overtaken(&self, generation: u64, sequence: u64) -> bool {
        if self.generation != generation || sequence == 0 {
            return false;
        }
        let slack = u64::from(self.buffers).saturating_sub(1).max(1);
        self.picture.sequence >= sequence.saturating_add(slack)
    }

    /// The handle value of buffer `index`, when the page names one there.
    #[must_use]
    pub fn buffer(&self, index: u32) -> Option<u64> {
        if index >= self.buffers {
            return None;
        }
        self.buffer
            .get(usize::try_from(index).ok()?)
            .copied()
            .filter(|&value| value != 0)
    }

    fn encode(&self) -> [u64; DATA] {
        let mut words = [0u64; DATA];
        let mut put = |at: usize, value: u64| {
            if let Some(word) = words.get_mut(at) {
                *word = value;
            }
        };
        put(D_EVENT, self.event);
        put(D_GENERATION, self.generation);
        put(D_RENDER, self.render);
        put(
            D_SIZE,
            (u64::from(self.width) << 32) | u64::from(self.height),
        );
        put(
            D_REFRESH,
            (u64::from(self.refresh.0) << 32) | u64::from(self.refresh.1),
        );
        put(D_FENCE, self.fence);
        put(D_BUFFERS, u64::from(self.buffers));
        for (i, &value) in self.buffer.iter().enumerate() {
            put(D_BUFFER + i, value);
        }
        put(D_SEQUENCE, self.picture.sequence);
        put(D_INDEX, u64::from(self.picture.index));
        put(D_FRAME, u64::from(self.picture.frame));
        put(D_ACQUIRED, self.picture.acquired);
        put(
            D_FLAGS,
            if self.picture.repeat { REPEAT } else { 0 }
                | if self.picture.protected { PROTECTED } else { 0 },
        );
        put(D_PICTURES, self.pictures);
        put(D_GAPS, self.gaps);
        words
    }

    /// The snapshot `words` describe, or none when they cannot be one: more
    /// buffers than a page names, or a picture in a buffer it does not name.
    /// **A page is read as hostile**: another process wrote it.
    fn decode(words: &[u64; DATA]) -> Option<Self> {
        let get = |at: usize| words.get(at).copied().unwrap_or(0);
        let half = |word: u64| {
            (
                u32::try_from(word >> 32).unwrap_or(0),
                u32::try_from(word & 0xffff_ffff).unwrap_or(0),
            )
        };
        let buffers = u32::try_from(get(D_BUFFERS)).ok()?;
        if usize::try_from(buffers).ok()? > MAX_BUFFERS {
            return None;
        }
        let mut buffer = [0u64; MAX_BUFFERS];
        for (i, value) in buffer.iter_mut().enumerate() {
            *value = get(D_BUFFER + i);
        }
        let flags = get(D_FLAGS);
        let picture = Picture {
            sequence: get(D_SEQUENCE),
            index: u32::try_from(get(D_INDEX)).ok()?,
            frame: u32::try_from(get(D_FRAME)).ok()?,
            acquired: get(D_ACQUIRED),
            repeat: flags & REPEAT != 0,
            protected: flags & PROTECTED != 0,
        };
        if picture.sequence != 0 && picture.index >= buffers {
            return None;
        }
        let (width, height) = half(get(D_SIZE));
        Some(Self {
            event: get(D_EVENT),
            generation: get(D_GENERATION),
            render: get(D_RENDER),
            width,
            height,
            refresh: half(get(D_REFRESH)),
            fence: get(D_FENCE),
            buffers,
            buffer,
            picture,
            pictures: get(D_PICTURES),
            gaps: get(D_GAPS),
        })
    }
}

/// Prepare `page` before any reader can have it: its magic and version.
/// False when the page is shorter than the channel's words.
pub fn prepare(page: &[AtomicU64]) -> bool {
    let (Some(magic), Some(version)) = (page.get(W_MAGIC), page.get(W_VERSION)) else {
        return false;
    };
    if page.len() < WORDS {
        return false;
    }
    version.store(u64::from(VERSION), Ordering::Relaxed);
    // Release: a reader that sees the magic sees the version too.
    magic.store(MAGIC, Ordering::Release);
    true
}

/// Publish `snapshot` into `page`. **The one writer's alone**: two writers
/// would each take the other's sequence for their own.
pub fn publish(page: &[AtomicU64], snapshot: &Snapshot) {
    let (Some(lock), Some(words)) = (page.get(W_SEQUENCE), page.get(W_DATA..WORDS)) else {
        return;
    };
    write_locked(lock, words, &snapshot.encode());
}

/// The latest snapshot of `page`: none when the page is not this version's,
/// when the writer overlapped every attempt -- a writer that died mid write
/// leaves its sequence odd for good -- or when what it says cannot be.
pub fn read(page: &[AtomicU64]) -> Option<Snapshot> {
    let (Some(magic), Some(version)) = (page.get(W_MAGIC), page.get(W_VERSION)) else {
        return None;
    };
    // Acquire: pairs with the magic's release in `prepare`.
    if magic.load(Ordering::Acquire) != MAGIC
        || version.load(Ordering::Relaxed) != u64::from(VERSION)
    {
        return None;
    }
    let (Some(lock), Some(words)) = (page.get(W_SEQUENCE), page.get(W_DATA..WORDS)) else {
        return None;
    };
    let mut data = [0u64; DATA];
    for _ in 0..ATTEMPTS {
        if read_locked(lock, words, &mut data) {
            return Snapshot::decode(&data);
        }
        core::hint::spin_loop();
    }
    None
}

/// Write `data` into `words` under `lock`. The sequence goes odd before the
/// first word and even again after the last; the release fence keeps every
/// word's store after the odd one, so a reader that sees any of them sees the
/// sequence moved.
pub(crate) fn write_locked(lock: &AtomicU64, words: &[AtomicU64], data: &[u64]) {
    let sequence = lock.load(Ordering::Relaxed);
    lock.store(sequence.wrapping_add(1), Ordering::Relaxed);
    fence(Ordering::Release);
    for (word, &value) in words.iter().zip(data) {
        word.store(value, Ordering::Relaxed);
    }
    // Release: a reader that sees the even sequence sees every word above.
    lock.store(sequence.wrapping_add(2), Ordering::Release);
}

/// Read `words` under `lock` into `out`: true when no write overlapped.
/// The acquire fence after the words keeps the sequence's second load after
/// them, so a word from a later write is caught by the sequence it moved.
pub(crate) fn read_locked(lock: &AtomicU64, words: &[AtomicU64], out: &mut [u64]) -> bool {
    let before = lock.load(Ordering::Acquire);
    if before & 1 == 1 {
        return false;
    }
    for (value, word) in out.iter_mut().zip(words) {
        *value = word.load(Ordering::Relaxed);
    }
    fence(Ordering::Acquire);
    lock.load(Ordering::Relaxed) == before
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use std::vec::Vec;

    fn page() -> Vec<AtomicU64> {
        (0..PAGE / 8).map(|_| AtomicU64::new(0)).collect()
    }

    fn snapshot() -> Snapshot {
        let mut buffer = [0u64; MAX_BUFFERS];
        buffer[0] = 0x7b0;
        buffer[1] = 0x7bc;
        buffer[2] = 0x7c0;
        Snapshot {
            event: 0x654,
            generation: 3,
            render: (1u64 << 32) | 0x14805,
            width: 2560,
            height: 1440,
            refresh: (120, 1),
            fence: 0x7b8,
            buffers: 3,
            buffer,
            picture: Picture {
                sequence: 1234,
                index: 2,
                frame: 18_223,
                acquired: 0x0000_0123_4567_89ab,
                repeat: true,
                protected: false,
            },
            pictures: 30_304,
            gaps: 1,
        }
    }

    #[test]
    fn the_page_fits_and_its_words_do_not_overlap() {
        assert_eq!(DATA, 30);
        let words = snapshot().encode();
        assert_eq!(Snapshot::decode(&words), Some(snapshot()));
        // Each field in a word of its own: one changed changes one word.
        let mut other = snapshot();
        other.picture.frame += 1;
        let changed = words
            .iter()
            .zip(other.encode().iter())
            .filter(|(a, b)| a != b)
            .count();
        assert_eq!(changed, 1);
    }

    #[test]
    fn a_published_snapshot_is_read_back_whole() {
        let page = page();
        assert_eq!(read(&page), None, "an unprepared page reads as nothing");
        assert!(prepare(&page));
        assert_eq!(read(&page), Some(Snapshot::default()));
        publish(&page, &snapshot());
        assert_eq!(read(&page), Some(snapshot()));
        assert_eq!(page[W_SEQUENCE].load(Ordering::Relaxed), 2);
        let mut next = snapshot();
        next.picture.sequence += 1;
        next.picture.index = 0;
        publish(&page, &next);
        assert_eq!(read(&page), Some(next));
        assert_eq!(page[W_SEQUENCE].load(Ordering::Relaxed), 4);
    }

    #[test]
    fn a_page_of_another_version_or_none_is_refused() {
        let page = page();
        assert!(prepare(&page));
        publish(&page, &snapshot());
        page[W_VERSION].store(u64::from(VERSION) + 1, Ordering::Relaxed);
        assert_eq!(read(&page), None);
        page[W_VERSION].store(u64::from(VERSION), Ordering::Relaxed);
        page[W_MAGIC].store(0, Ordering::Relaxed);
        assert_eq!(read(&page), None);
        let short: Vec<AtomicU64> = (0..WORDS - 1).map(|_| AtomicU64::new(0)).collect();
        assert!(!prepare(&short));
        assert_eq!(read(&short), None);
    }

    #[test]
    fn a_write_in_progress_is_not_taken() {
        let page = page();
        assert!(prepare(&page));
        publish(&page, &snapshot());
        // A writer stopped mid write leaves the sequence odd for good.
        page[W_SEQUENCE].store(3, Ordering::Relaxed);
        assert_eq!(read(&page), None);
        page[W_SEQUENCE].store(4, Ordering::Relaxed);
        assert_eq!(read(&page), Some(snapshot()));
    }

    #[test]
    fn a_page_that_cannot_be_is_refused() {
        let mut words = snapshot().encode();
        words[D_BUFFERS] = MAX_BUFFERS as u64 + 1;
        assert_eq!(Snapshot::decode(&words), None, "more buffers than named");
        let mut words = snapshot().encode();
        words[D_INDEX] = 3;
        assert_eq!(
            Snapshot::decode(&words),
            None,
            "a picture in no named buffer"
        );
        let mut words = snapshot().encode();
        words[D_INDEX] = 3;
        words[D_SEQUENCE] = 0;
        assert!(
            Snapshot::decode(&words).is_some(),
            "no picture yet names no buffer"
        );
        let mut words = snapshot().encode();
        words[D_FRAME] = u64::from(u32::MAX) + 1;
        assert_eq!(Snapshot::decode(&words), None);
    }

    #[test]
    fn a_picture_is_overtaken_once_the_system_can_write_its_buffer_again() {
        let mut latest = snapshot();
        latest.picture.sequence = 101;
        // Three buffers: the picture taken at 100 is written again only for
        // the third composition after it, which the driver acquires as 102.
        assert!(!latest.overtaken(3, 100));
        latest.picture.sequence = 102;
        assert!(latest.overtaken(3, 100));
        assert!(!latest.overtaken(3, 101));
        // Another generation's buffer is no longer written at all.
        assert!(!latest.overtaken(2, 100));
        // Nothing taken, nothing overtaken.
        assert!(!latest.overtaken(3, 0));
        // Fewer buffers seen errs early: with one or two, one newer picture.
        latest.buffers = 2;
        latest.picture.sequence = 101;
        assert!(latest.overtaken(3, 100));
        latest.buffers = 1;
        assert!(latest.overtaken(3, 100));
        latest.buffers = 0;
        assert!(latest.overtaken(3, 100));
    }

    #[test]
    fn a_buffer_is_named_only_where_the_page_names_one() {
        let taken = snapshot();
        assert_eq!(taken.buffer(0), Some(0x7b0));
        assert_eq!(taken.buffer(2), Some(0x7c0));
        assert_eq!(taken.buffer(3), None, "past the buffers seen");
        let mut gap = snapshot();
        gap.buffer[1] = 0;
        assert_eq!(gap.buffer(1), None, "a slot with no handle");
        assert_eq!(gap.buffer(u32::MAX), None);
    }
}

/// The sequence lock under the model checker: every interleaving of a writer
/// publishing twice and a reader reading, each word written from the same
/// number, so a reading taken from two writes shows two numbers.
#[cfg(all(test, loom))]
mod model {
    use super::{AtomicU64, Ordering, read_locked, write_locked};
    use loom::sync::Arc;
    use std::vec::Vec;

    const N: usize = 3;

    #[test]
    fn a_reading_is_one_write_whole_or_refused() {
        loom::model(|| {
            let lock = Arc::new(AtomicU64::new(0));
            let words: Arc<Vec<AtomicU64>> = Arc::new((0..N).map(|_| AtomicU64::new(0)).collect());
            let writer = {
                let (lock, words) = (Arc::clone(&lock), Arc::clone(&words));
                loom::thread::spawn(move || {
                    write_locked(&lock, &words, &[1; N]);
                    write_locked(&lock, &words, &[2; N]);
                })
            };
            let mut out = [u64::MAX; N];
            if read_locked(&lock, &words, &mut out) {
                assert!(
                    out.iter().all(|&v| v == out[0]),
                    "a reading mixed two writes: {out:?}"
                );
            }
            writer.join().unwrap();
            assert_eq!(lock.load(Ordering::Relaxed), 4);
            let mut last = [0; N];
            assert!(read_locked(&lock, &words, &mut last));
            assert_eq!(last, [2; N]);
        });
    }
}
