//! A monitor's modes, fixed when it arrives (docs/11-drivers.md section 1.5).

/// A size and a whole refresh rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
}

impl Mode {
    pub const fn new(width: u32, height: u32, refresh_hz: u32) -> Self {
        Self {
            width,
            height,
            refresh_hz,
        }
    }

    /// Within what a monitor of this display may be: no smaller than the
    /// system's own floor for a desktop, no larger than the largest picture an
    /// encoder here takes, and a rate a compositor can keep.
    pub const fn is_valid(&self) -> bool {
        self.width >= 640
            && self.width <= 7680
            && self.height >= 480
            && self.height <= 4320
            && self.refresh_hz >= 24
            && self.refresh_hz <= 480
    }
}

/// What a monitor is plugged at when nothing is asked.
pub const DEFAULT: Mode = Mode::new(1920, 1080, 60);

/// The common sizes every monitor lists.
pub const SIZES: [(u32, u32); 13] = [
    (3840, 2160),
    (3440, 1440),
    (2560, 1600),
    (2560, 1440),
    (2560, 1080),
    (1920, 1200),
    (1920, 1080),
    (1680, 1050),
    (1600, 900),
    (1440, 900),
    (1366, 768),
    (1280, 800),
    (1280, 720),
];

/// The rates every listed size comes at.
pub const RATES: [u32; 4] = [60, 120, 144, 240];

/// The most modes a monitor lists. A monitor with around a hundred has been
/// seen not to arrive, so the list stays well short of that.
pub const MAX_MODES: usize = 72;

/// The modes a monitor arrives with: the plug's own first and preferred, the
/// plug's size at every listed rate, then every listed size at every listed
/// rate, each once.
#[derive(Debug, Clone, Copy)]
pub struct ModeList {
    modes: [Mode; MAX_MODES],
    len: usize,
}

impl ModeList {
    pub fn for_plug(preferred: Mode) -> Self {
        let mut list = Self {
            modes: [Mode::default(); MAX_MODES],
            len: 0,
        };
        list.push(preferred);
        for rate in RATES {
            list.push(Mode::new(preferred.width, preferred.height, rate));
        }
        for (width, height) in SIZES {
            for rate in RATES {
                list.push(Mode::new(width, height, rate));
            }
        }
        list
    }

    fn push(&mut self, mode: Mode) {
        if self.as_slice().contains(&mode) {
            return;
        }
        if let Some(free) = self.modes.get_mut(self.len) {
            *free = mode;
            self.len += 1;
        }
    }

    pub fn as_slice(&self) -> &[Mode] {
        self.modes.get(..self.len).unwrap_or(&[])
    }

    /// The plug's own mode, which the monitor prefers.
    pub fn preferred(&self) -> Mode {
        self.as_slice().first().copied().unwrap_or(DEFAULT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plugs_mode_comes_first_and_every_mode_once() {
        let asked = Mode::new(2880, 1800, 90);
        let list = ModeList::for_plug(asked);
        let modes = list.as_slice();
        assert_eq!(modes.first(), Some(&asked));
        assert_eq!(list.preferred(), asked);
        for (i, a) in modes.iter().enumerate() {
            for b in modes.iter().skip(i + 1) {
                assert_ne!(a, b, "{a:?} listed twice");
            }
        }
        // The plug's size at the listed rates, then the listed sizes.
        assert!(modes.contains(&Mode::new(2880, 1800, 240)));
        assert!(modes.contains(&DEFAULT));
        assert!(modes.len() <= MAX_MODES);
    }

    #[test]
    fn a_listed_mode_is_not_listed_again() {
        let list = ModeList::for_plug(DEFAULT);
        assert_eq!(list.as_slice().len(), SIZES.len() * RATES.len());
        let fresh = ModeList::for_plug(Mode::new(3000, 2000, 30));
        assert_eq!(
            fresh.as_slice().len(),
            1 + RATES.len() + SIZES.len() * RATES.len()
        );
        assert!(
            fresh.as_slice().len() <= MAX_MODES,
            "the cap holds the most a plug adds"
        );
    }

    #[test]
    fn every_listed_mode_is_one_a_monitor_may_be() {
        for (width, height) in SIZES {
            for rate in RATES {
                assert!(Mode::new(width, height, rate).is_valid());
            }
        }
        assert!(DEFAULT.is_valid());
        // Each bound, on both sides.
        assert!(Mode::new(640, 480, 24).is_valid());
        assert!(Mode::new(7680, 4320, 480).is_valid());
        assert!(!Mode::new(639, 480, 60).is_valid());
        assert!(!Mode::new(640, 479, 60).is_valid());
        assert!(!Mode::new(1920, 1080, 23).is_valid());
        assert!(!Mode::new(7681, 4320, 60).is_valid());
        assert!(!Mode::new(7680, 4321, 60).is_valid());
        assert!(!Mode::new(1920, 1080, 481).is_valid());
    }
}
