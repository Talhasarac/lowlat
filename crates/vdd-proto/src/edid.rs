//! A monitor's EDID: one block per slot (docs/11-drivers.md section 1.3).
//!
//! **Nothing in it changes but the serial, which carries the slot.** The system
//! knows a monitor by its EDID, so one that changed across a replug -- a new
//! preferred timing for a new mode, say -- would be a new monitor and lose its
//! arrangement; the modes travel in the lists the driver answers with instead.
//! The serial is also how the driver finds a monitor's slot when the system
//! asks for the modes of a description and names no monitor.
//!
//! The block is the driver's own, or a real monitor's a person dumped and
//! named in the file (section 1.4); either way its serial is patched per slot.
//! A different block is a different monitor to the system, with nothing of
//! the old one's arrangement.

use crate::MAX_MONITORS;

/// One block.
pub const LEN: usize = 128;
/// The most an EDID here carries: the base block and one extension.
pub const MAX_LEN: usize = 2 * LEN;

const HEADER: [u8; 8] = [0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00];
/// The manufacturer, as the three letters the block packs: "LWL".
const VENDOR: [u8; 2] = [0x32, 0xec];
const PRODUCT: [u8; 2] = [0x01, 0x00];
/// The serial's upper half; the slot is the lower.
const SERIAL_BASE: u32 = 0x4c57_0000;
/// Where the serial sits, and how far a description must match the block
/// before it: the header, the manufacturer and the product.
const SERIAL: core::ops::Range<usize> = 12..16;
/// The extension count, in the base block.
const EXTENSIONS: usize = 126;

const TEMPLATE: [u8; LEN] = [
    // Header.
    0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00, //
    // Manufacturer, product, serial (patched per slot).
    VENDOR[0], VENDOR[1], PRODUCT[0], PRODUCT[1], 0x00, 0x00, 0x00, 0x00, //
    // Week 1 of 2026; version 1.4.
    0x01, 36, 0x01, 0x04, //
    // Digital input, eight bits a component, over DisplayPort; size unknown,
    // so no physical size scales the desktop; gamma 2.2; sRGB, the preferred
    // timing native.
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

fn sum(block: &[u8]) -> u8 {
    block.iter().fold(0u8, |sum, b| sum.wrapping_add(*b))
}

/// An EDID of one block or two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Edid {
    bytes: [u8; MAX_LEN],
    len: usize,
}

impl Default for Edid {
    fn default() -> Self {
        Self::generated()
    }
}

impl Edid {
    /// The driver's own block.
    pub fn generated() -> Self {
        let mut bytes = [0u8; MAX_LEN];
        if let Some(base) = bytes.get_mut(..LEN) {
            base.copy_from_slice(&TEMPLATE);
        }
        Self { bytes, len: LEN }
    }

    /// A dumped EDID: one block or two, the header first, each block's
    /// checksum right, and the extension count saying how many follow.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != LEN && bytes.len() != MAX_LEN {
            return None;
        }
        if bytes.get(..HEADER.len())? != HEADER {
            return None;
        }
        if bytes.chunks(LEN).any(|block| sum(block) != 0) {
            return None;
        }
        if usize::from(*bytes.get(EXTENSIONS)?) != bytes.len() / LEN - 1 {
            return None;
        }
        let mut edid = Self {
            bytes: [0u8; MAX_LEN],
            len: bytes.len(),
        };
        edid.bytes.get_mut(..bytes.len())?.copy_from_slice(bytes);
        Some(edid)
    }

    pub fn as_slice(&self) -> &[u8] {
        self.bytes.get(..self.len).unwrap_or(&[])
    }

    /// This EDID for a slot: its serial carries the slot, and the base block's
    /// checksum follows. A slot past the last is clamped to it rather than
    /// refused, since the driver never asks for one.
    pub fn for_slot(&self, slot: u8) -> Self {
        let slot = slot.min(u8::try_from(MAX_MONITORS - 1).unwrap_or(u8::MAX));
        let mut edid = *self;
        let serial = SERIAL_BASE | u32::from(slot);
        if let Some(field) = edid.bytes.get_mut(SERIAL) {
            field.copy_from_slice(&serial.to_le_bytes());
        }
        let total = sum(edid.bytes.get(..LEN - 1).unwrap_or(&[]));
        if let Some(checksum) = edid.bytes.get_mut(LEN - 1) {
            *checksum = 0u8.wrapping_sub(total);
        }
        edid
    }

    /// The slot a description of this EDID's names, or `None` for any other.
    pub fn slot_of(&self, description: &[u8]) -> Option<u8> {
        if description.len() < LEN {
            return None;
        }
        if description.get(..SERIAL.start)? != self.bytes.get(..SERIAL.start)? {
            return None;
        }
        let serial = u32::from_le_bytes(description.get(SERIAL)?.try_into().ok()?);
        if serial & 0xffff_0000 != SERIAL_BASE {
            return None;
        }
        let slot = usize::try_from(serial & 0xffff).ok()?;
        if slot >= MAX_MONITORS {
            return None;
        }
        u8::try_from(slot).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid(edid: &Edid) -> bool {
        edid.as_slice().chunks(LEN).all(|block| sum(block) == 0)
    }

    #[test]
    fn every_slots_block_is_valid_and_names_its_slot() {
        let own = Edid::generated();
        for slot in 0..u8::try_from(MAX_MONITORS).expect("fits") {
            let block = own.for_slot(slot);
            assert_eq!(&block.as_slice()[..8], &HEADER);
            assert!(valid(&block), "slot {slot}: the checksum");
            assert_eq!(own.slot_of(block.as_slice()), Some(slot));
        }
        assert_eq!(
            own.slot_of(own.for_slot(200).as_slice()),
            Some(15),
            "past the last, clamped"
        );
    }

    #[test]
    fn slots_differ_in_the_serial_and_checksum_alone() {
        let a = Edid::generated().for_slot(0);
        let b = Edid::generated().for_slot(9);
        for (i, (x, y)) in a.as_slice().iter().zip(b.as_slice()).enumerate() {
            if !SERIAL.contains(&i) && i != LEN - 1 {
                assert_eq!(x, y, "byte {i} differs between slots");
            }
        }
        assert_ne!(a.as_slice()[SERIAL], b.as_slice()[SERIAL]);
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
    fn a_foreign_or_short_description_names_no_slot() {
        let own = Edid::generated();
        let mut block = *own
            .for_slot(3)
            .as_slice()
            .first_chunk::<LEN>()
            .expect("a block");
        assert_eq!(own.slot_of(&block[..LEN - 1]), None);
        block[9] ^= 1;
        assert_eq!(own.slot_of(&block), None);
        let mut far = *own
            .for_slot(3)
            .as_slice()
            .first_chunk::<LEN>()
            .expect("a block");
        far[12] = 0x40;
        assert_eq!(own.slot_of(&far), None, "a slot past the last");
        let mut other = *own
            .for_slot(3)
            .as_slice()
            .first_chunk::<LEN>()
            .expect("a block");
        other[15] ^= 0x80;
        assert_eq!(own.slot_of(&other), None, "another serial range");
    }

    /// A real monitor's two blocks, as a person would dump them: another
    /// maker, one extension, both checksums right.
    fn dumped() -> [u8; MAX_LEN] {
        let mut bytes = [0u8; MAX_LEN];
        bytes[..LEN].copy_from_slice(&TEMPLATE);
        bytes[8..12].copy_from_slice(&[0x10, 0xac, 0x34, 0x12]);
        bytes[12..16].copy_from_slice(&[0xaa, 0xbb, 0xcc, 0xdd]);
        bytes[EXTENSIONS] = 1;
        bytes[LEN - 1] = 0u8.wrapping_sub(sum(&bytes[..LEN - 1]));
        bytes[LEN] = 0x02;
        bytes[LEN + 1] = 0x03;
        bytes[MAX_LEN - 1] = 0u8.wrapping_sub(sum(&bytes[LEN..MAX_LEN - 1]));
        bytes
    }

    #[test]
    fn a_dumped_edid_is_taken_whole_and_patched_per_slot() {
        let bytes = dumped();
        let edid = Edid::parse(&bytes).expect("valid");
        assert_eq!(edid.as_slice(), &bytes[..]);
        let five = edid.for_slot(5);
        assert!(valid(&five), "both checksums after the patch");
        assert_eq!(edid.slot_of(five.as_slice()), Some(5));
        assert_eq!(
            &five.as_slice()[8..12],
            &[0x10, 0xac, 0x34, 0x12],
            "its maker kept"
        );
        assert_eq!(
            &five.as_slice()[LEN..],
            &bytes[LEN..],
            "the extension untouched"
        );
        assert_eq!(
            Edid::generated().slot_of(five.as_slice()),
            None,
            "not the generated block's"
        );
    }

    #[test]
    fn a_damaged_dump_is_refused() {
        let good = dumped();
        assert!(Edid::parse(&good).is_some());
        // One good block and a zero byte: every checksum holds, the length
        // alone is wrong.
        let mut long = [0u8; LEN + 1];
        long[..LEN].copy_from_slice(Edid::generated().for_slot(0).as_slice());
        assert!(Edid::parse(&long).is_none(), "a length of no block count");
        // A header broken with its checksum kept right.
        let mut header = good;
        header[1] = 0;
        header[LEN - 1] = 0u8.wrapping_sub(sum(&header[..LEN - 1]));
        assert!(Edid::parse(&header).is_none(), "the header");
        let mut base = good;
        base[20] ^= 1;
        assert!(Edid::parse(&base).is_none(), "the base block's checksum");
        let mut extension = good;
        extension[LEN + 5] ^= 1;
        assert!(
            Edid::parse(&extension).is_none(),
            "the extension's checksum"
        );
        let mut count = TEMPLATE;
        count[EXTENSIONS] = 1;
        count[LEN - 1] = 0u8.wrapping_sub(sum(&count[..LEN - 1]));
        assert!(
            Edid::parse(&count).is_none(),
            "one block saying another follows"
        );
        let generated = Edid::generated().for_slot(0);
        assert!(
            Edid::parse(generated.as_slice()).is_some(),
            "the generated one, one block"
        );
    }
}
