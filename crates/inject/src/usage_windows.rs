//! Peer key codes to the scancodes Windows produces for the same keys.
//!
//! A peer reports a physical key by its usage code, not by the character the
//! key produces (docs/01-protocol.md section 11.1). Layout belongs to the far
//! side; this host injects the key that was pressed and lets the local layout
//! decide what it means.
//!
//! **The table is the system's own** ([`crate::scancode`]): generated from the
//! translation a USB keyboard's keys go through on their way into the system,
//! so an injected key and a key typed on a keyboard plugged into this machine
//! reach it as the same scancode. As on Linux, any other table would be a
//! second opinion about a question the system has already answered.
//!
//! **A code is the Set-1 make code with the E0 prefix in bit 7.** Every make
//! code is below 0x80, so every key fits a byte and so the expander's held-key
//! slots; the input call takes the prefix as a flag beside the code.
//!
//! Zero means the usage code names no key here: a slot the standard reserves,
//! a key the system has no code for, a key this host refuses, or a code past
//! the end of the table.

use crate::scancode::SET1;

/// The six codes a peer sends above the usage range, which are not usage
/// codes at all: the peer's enumeration goes on past the end of the keyboard
/// page with its own values for the media keys. Each lands on the scancode a
/// keyboard's own media key sends: next track, previous track, stop, play and
/// pause, mute, media select.
const EXTRA_FIRST: u16 = 258;
const EXTRA: [u8; 6] = [0x99, 0x90, 0xa4, 0xa2, 0xa0, 0xed];

/// Codes peers send that no keyboard does, where the key they mean is.
///
/// **The context-menu key has two names** and peers send either for the one
/// key, so the second lands where the first does. **Mute and the volume keys
/// also exist on the keyboard page**, where a keyboard never puts them, and
/// land on the codes a keyboard's own media keys send.
const ADDED: [(u16, u8); 4] = [(118, 0xdd), (127, 0xa0), (128, 0xb0), (129, 0xae)];

/// Keys this host does not take from a guest, though the system has codes
/// for them: power, sleep and wake. **A guest must not be able to put the
/// host to sleep or turn it off**; the machine it would leave unreachable is
/// the one the guest is using.
const REFUSED: [u16; 4] = [102, 233, 234, 235];

/// The scancode for a peer's key code, or `None` when it names none.
#[must_use]
pub fn key_code(usage: u16) -> Option<u16> {
    if REFUSED.contains(&usage) {
        return None;
    }
    if let Some(&(_, code)) = ADDED.iter().find(|&&(from, _)| from == usage) {
        return Some(u16::from(code));
    }
    let code = match usage {
        // The index is in range by the arm's own bound, and the fallible
        // form keeps the crate free of panicking indexing.
        0..=255 => *SET1.get(usize::from(usage))?,
        _ => {
            let offset = usize::from(usage.checked_sub(EXTRA_FIRST)?);
            *EXTRA.get(offset)?
        }
    };
    (code != 0).then_some(u16::from(code))
}

/// The usage a key was scanned as, qualified by its page, or `None` past the
/// usage range. The expander announces it ahead of a key for the kernel's
/// input layer; the system's input call takes the scancode alone, so here it
/// goes nowhere.
#[must_use]
pub fn scan_code(usage: u16) -> Option<i32> {
    (usage <= 255).then(|| USAGE_PAGE | i32::from(usage))
}

/// The standard's keyboard page, which the scanned code is qualified by.
const USAGE_PAGE: i32 = 0x0007_0000;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scancode::{MAKE_ONLY, PAUSE, SYSRQ};

    /// Spot checks across the table's shape: letters, digits, the modifier
    /// block at both ends, keypad keys either side of the E0 split, and the
    /// navigation keys, which are the E0 half.
    #[test]
    fn known_keys_land_where_the_system_puts_them() {
        assert_eq!(key_code(4), Some(0x1e)); // a
        assert_eq!(key_code(29), Some(0x2c)); // z
        assert_eq!(key_code(30), Some(0x02)); // 1
        assert_eq!(key_code(40), Some(0x1c)); // enter
        assert_eq!(key_code(41), Some(0x01)); // escape
        assert_eq!(key_code(44), Some(0x39)); // space
        assert_eq!(key_code(57), Some(0x3a)); // caps lock
        assert_eq!(key_code(83), Some(0x45)); // num lock
        assert_eq!(key_code(224), Some(0x1d)); // left control
        assert_eq!(key_code(228), Some(0x9d)); // right control
        assert_eq!(key_code(231), Some(0xdc)); // right meta
        assert_eq!(key_code(89), Some(0x4f)); // keypad 1
        assert_eq!(key_code(84), Some(0xb5)); // keypad divide
        assert_eq!(key_code(88), Some(0x9c)); // keypad enter
        assert_eq!(key_code(76), Some(0xd3)); // delete
    }

    /// The keys that do not answer with one code are recorded in the form
    /// the system gives them under a modifier, and the sink is told which
    /// they are by the same constants.
    #[test]
    fn print_screen_and_pause_are_the_specials_the_sink_knows() {
        assert_eq!(key_code(70), Some(u16::from(SYSRQ.0)));
        assert_eq!(SYSRQ, (0xb7, 0x54));
        assert_eq!(key_code(72), Some(u16::from(PAUSE)));
        assert_eq!(PAUSE, 0xc6);
    }

    #[test]
    fn both_backslash_usage_codes_map_to_one_key() {
        assert_eq!(key_code(49), key_code(50));
        assert_eq!(key_code(49), Some(0x2b));
    }

    /// **Both of the peer's names for the context-menu key reach it.** The
    /// system has no code for the second, so without the addition the key
    /// would do nothing from half the peers that send it.
    #[test]
    fn both_names_for_the_context_menu_key_reach_it() {
        assert_eq!(key_code(101), Some(0xdd));
        assert_eq!(key_code(118), Some(0xdd));
        assert_eq!(SET1.get(118), Some(&0), "the system gained a code for it");
    }

    /// The media keys above the usage range, and mute and volume on the
    /// keyboard page, land on the keyboard's own media keys: the two mutes
    /// are one key.
    #[test]
    fn the_media_keys_land_on_the_keyboards_own() {
        assert_eq!(key_code(258), Some(0x99));
        assert_eq!(key_code(263), Some(0xed));
        assert_eq!(key_code(262), key_code(127));
        assert_eq!(key_code(128), Some(0xb0));
        assert_eq!(key_code(129), Some(0xae));
        assert_eq!(key_code(257), None);
        assert_eq!(key_code(264), None);
        assert_eq!(key_code(u16::MAX), None);
    }

    /// **Power, sleep and wake are refused** though the system has codes for
    /// them, or a guest could leave the host unreachable.
    #[test]
    fn power_sleep_and_wake_are_not_mapped() {
        for usage in REFUSED {
            assert_ne!(SET1.get(usize::from(usage)), Some(&0), "usage {usage}");
            assert_eq!(key_code(usage), None, "usage {usage}");
        }
    }

    #[test]
    fn the_reserved_and_error_codes_name_no_key() {
        for usage in 0..=3 {
            assert_eq!(key_code(usage), None, "usage {usage}");
        }
    }

    /// A count, so a table that loses or gains entries in a regeneration
    /// fails here rather than in whichever key stopped working: the system's
    /// 135, less the four refused, with the four added and the six above the
    /// range.
    #[test]
    fn the_table_maps_the_number_of_codes_it_did() {
        let mapped = (0..=u16::MAX).filter(|&u| key_code(u).is_some()).count();
        assert_eq!(SET1.iter().filter(|&&c| c != 0).count(), 135);
        assert_eq!(mapped, 141);
    }

    /// **A make-only code and an E0 code share bit 7**, so none may be another
    /// key's as well, or the sink could not tell which it was handed.
    #[test]
    fn a_make_only_code_is_no_other_keys() {
        for code in MAKE_ONLY {
            let keys = (0..=u16::MAX)
                .filter(|&u| key_code(u) == Some(u16::from(code)))
                .count();
            assert_eq!(keys, 1, "{code:#x}");
        }
    }

    #[test]
    fn a_scanned_code_is_qualified_by_the_page_it_belongs_to() {
        assert_eq!(scan_code(4), Some(0x0007_0004));
        assert_eq!(scan_code(258), None);
    }
}
