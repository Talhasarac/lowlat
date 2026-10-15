//! A monitor's EDID: one fixed block per slot (docs/11-drivers.md section 1.3).
//!
//! **Nothing in it changes but the serial, which carries the slot.** The system
//! knows a monitor by its EDID, so one that changed across a replug -- a new
//! preferred timing for a new mode, say -- would be a new monitor and lose its
//! arrangement; the modes travel in the lists the driver answers with instead.
//! The serial is also how the driver finds a monitor's slot when the system
//! asks for the modes of a description and names no monitor.

use crate::MAX_MONITORS;

pub const LEN: usize = 128;

/// The manufacturer, as the three letters the block packs: "LWL".
const VENDOR: [u8; 2] = [0x32, 0xec];
const PRODUCT: [u8; 2] = [0x01, 0x00];
/// The serial's upper half; the slot is the lower.
const SERIAL_BASE: u32 = 0x4c57_0000;

const TEMPLATE: [u8; LEN] = [
    // Header.
    0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00, //
    // Manufacturer, product, serial (patched per slot).
    VENDOR[0], VENDOR[1], PRODUCT[0], PRODUCT[1], 0x00, 0x00, 0x00, 0x00, //
    // Week 1 of 2026; version 1.4.
    0x01, 36, 0x01, 0x04, //
    // Digital input, eight bits a component; size unknown, so no physical
    // size scales the desktop; gamma 2.2; sRGB, the preferred timing native.
    0xa5, 0x00, 0x00, 0x78, 0x06, //
    // The sRGB primaries and white point.
    0xee, 0x91, 0xa3, 0x54, 0x4c, 0x99, 0x26, 0x0f, 0x50, 0x54, //
    // No established timings; the eight standard timings unused.
    0x00, 0x00, 0x00, //
    0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, //
    0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, //
    // Detailed timing: 1920x1080 at 60, 148.5 MHz, positive syncs.
    0x02, 0x3a, 0x80, 0x18, 0x71, 0x38, 0x2d, 0x40, 0x58, 0x2c, 0x45, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x1e, //
    // The name a person sees.
    0x00, 0x00, 0x00, 0xfc, 0x00, b'l', b'o', b'w', b'l', b'a', b't', 0x0a, 0x20, 0x20, 0x20, 0x20,
    0x20, 0x20, //
    // Two descriptors left unused.
    0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, //
    0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, //
    // No extension; the checksum (patched).
    0x00, 0x00,
];

/// The block for a slot. A slot past the last is clamped to it rather than
/// refused, since the driver never asks for one.
pub fn edid(slot: u8) -> [u8; LEN] {
    let slot = slot.min(u8::try_from(MAX_MONITORS - 1).unwrap_or(u8::MAX));
    let mut block = TEMPLATE;
    let serial = SERIAL_BASE | u32::from(slot);
    if let Some(field) = block.get_mut(12..16) {
        field.copy_from_slice(&serial.to_le_bytes());
    }
    let sum = block
        .get(..LEN - 1)
        .unwrap_or(&[])
        .iter()
        .fold(0u8, |sum, b| sum.wrapping_add(*b));
    if let Some(checksum) = block.last_mut() {
        *checksum = 0u8.wrapping_sub(sum);
    }
    block
}

/// The slot a block of this driver's names, or `None` for any other block.
pub fn slot_of(block: &[u8]) -> Option<u8> {
    if block.len() < LEN {
        return None;
    }
    let ours = block.get(..12)? == TEMPLATE.get(..12)?;
    if !ours {
        return None;
    }
    let serial = u32::from_le_bytes(block.get(12..16)?.try_into().ok()?);
    if serial & 0xffff_0000 != SERIAL_BASE {
        return None;
    }
    let slot = usize::try_from(serial & 0xffff).ok()?;
    if slot >= MAX_MONITORS {
        return None;
    }
    u8::try_from(slot).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_slots_block_is_valid_and_names_its_slot() {
        for slot in 0..u8::try_from(MAX_MONITORS).expect("fits") {
            let block = edid(slot);
            assert_eq!(&block[..8], &[0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0]);
            assert_eq!(
                block.iter().fold(0u8, |s, b| s.wrapping_add(*b)),
                0,
                "slot {slot}: the checksum"
            );
            assert_eq!(slot_of(&block), Some(slot));
        }
        assert_eq!(slot_of(&edid(200)), Some(15), "past the last, clamped");
    }

    #[test]
    fn slots_differ_in_the_serial_and_checksum_alone() {
        let a = edid(0);
        let b = edid(9);
        for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
            if !(12..16).contains(&i) && i != LEN - 1 {
                assert_eq!(x, y, "byte {i} differs between slots");
            }
        }
        assert_ne!(a[12..16], b[12..16]);
    }

    #[test]
    fn the_manufacturer_packs_three_letters() {
        let packed = u16::from_be_bytes(VENDOR);
        let letter = |shift: u16| {
            char::from(b'A' - 1 + u8::try_from((packed >> shift) & 31).expect("5 bits"))
        };
        assert_eq!((letter(10), letter(5), letter(0)), ('L', 'W', 'L'));
        assert_eq!(packed >> 15, 0, "the top bit is clear");
    }

    #[test]
    fn a_foreign_or_short_block_names_no_slot() {
        let mut block = edid(3);
        assert_eq!(slot_of(&block[..LEN - 1]), None);
        block[9] ^= 1;
        assert_eq!(slot_of(&block), None);
        let mut far = edid(3);
        far[12] = 0x40;
        assert_eq!(slot_of(&far), None, "a slot past the last");
        let mut other = edid(3);
        other[15] ^= 0x80;
        assert_eq!(slot_of(&other), None, "another serial range");
    }
}
