//! The file header and its two-slot commit protocol.
//!
//! A `.cndb` file opens with two fixed 64-byte header slots. A commit writes a
//! header with an incremented generation into whichever slot is *older*, so the
//! previous generation's header stays intact for the entire write. On open both
//! slots are read and the valid one with the highest generation wins.
//!
//! This replaces overwriting a single header in place (issue #1 §4.3). A 64-byte
//! write inside one sector is atomic on real hardware, but that is a hardware
//! assumption rather than a guarantee; ping-pong makes a torn header a
//! recoverable event instead of a corrupt database.

use crate::error::{CndbError, Result};

/// Magic bytes opening every cndb file.
pub const MAGIC: [u8; 4] = *b"CNDB";

/// On-disk format version, `0.3.0`.
pub const FORMAT_VERSION: u32 = 0x0003_0000;

/// Size of a single header slot.
pub const HEADER_SIZE: usize = 64;

/// Number of header slots.
pub const HEADER_SLOTS: usize = 2;

/// Bytes reserved for headers; also the offset of the first log record.
pub const LOG_START: u64 = (HEADER_SIZE * HEADER_SLOTS) as u64;

/// Byte range of the checksum field, zeroed while the checksum is computed.
const CRC_RANGE: std::ops::Range<usize> = 40..44;

/// A decoded header slot.
///
/// Layout, little-endian:
///
/// | range   | field        |
/// |---------|--------------|
/// | `0..4`  | magic        |
/// | `4..8`  | version      |
/// | `8..16` | generation   |
/// | `16..24`| master_ptr   |
/// | `24..32`| master_len   |
/// | `32..40`| log_end      |
/// | `40..44`| crc32        |
/// | `44..64`| reserved     |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// Monotonic commit counter. The highest valid one is the live database.
    pub generation: u64,
    /// Offset of the master index record, or 0 when none has been written.
    pub master_ptr: u64,
    /// Length of the master index payload, or 0 when none has been written.
    pub master_len: u64,
    /// First free byte of the log. The next append starts here.
    pub log_end: u64,
}

impl Header {
    /// The header of a freshly created, empty database.
    pub fn initial() -> Self {
        Header {
            generation: 0,
            master_ptr: 0,
            master_len: 0,
            log_end: LOG_START,
        }
    }

    /// Serialize to its fixed 64-byte on-disk form, checksum included.
    pub fn encode(&self) -> [u8; HEADER_SIZE] {
        let mut buf = [0u8; HEADER_SIZE];
        buf[0..4].copy_from_slice(&MAGIC);
        buf[4..8].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        buf[8..16].copy_from_slice(&self.generation.to_le_bytes());
        buf[16..24].copy_from_slice(&self.master_ptr.to_le_bytes());
        buf[24..32].copy_from_slice(&self.master_len.to_le_bytes());
        buf[32..40].copy_from_slice(&self.log_end.to_le_bytes());
        // CRC field stays zero while the checksum is computed over the whole
        // slot, so reserved bytes are covered too.
        let crc = crc32fast::hash(&buf);
        buf[CRC_RANGE].copy_from_slice(&crc.to_le_bytes());
        buf
    }

    /// Parse a header slot, validating magic, version and checksum.
    ///
    /// `slot` only labels errors.
    pub fn decode(buf: &[u8; HEADER_SIZE], slot: usize) -> Result<Self> {
        let magic: [u8; 4] = buf[0..4].try_into().expect("4 bytes");
        if magic != MAGIC {
            return Err(CndbError::BadMagic {
                expected: MAGIC,
                found: magic,
            });
        }

        let stored_crc = u32::from_le_bytes(buf[CRC_RANGE].try_into().expect("4 bytes"));
        let mut probe = *buf;
        probe[CRC_RANGE].fill(0);
        if crc32fast::hash(&probe) != stored_crc {
            return Err(CndbError::HeaderChecksum { slot });
        }

        let version = u32::from_le_bytes(buf[4..8].try_into().expect("4 bytes"));
        if version != FORMAT_VERSION {
            return Err(CndbError::UnsupportedVersion {
                found: version,
                supported: FORMAT_VERSION,
            });
        }

        Ok(Header {
            generation: u64::from_le_bytes(buf[8..16].try_into().expect("8 bytes")),
            master_ptr: u64::from_le_bytes(buf[16..24].try_into().expect("8 bytes")),
            master_len: u64::from_le_bytes(buf[24..32].try_into().expect("8 bytes")),
            log_end: u64::from_le_bytes(buf[32..40].try_into().expect("8 bytes")),
        })
    }

    /// Byte offset of a header slot.
    pub fn slot_offset(slot: usize) -> u64 {
        debug_assert!(slot < HEADER_SLOTS);
        (slot * HEADER_SIZE) as u64
    }
}

/// Pick the live header from the decoded slots.
///
/// Takes the valid slot with the highest generation. Returns `NoValidHeader`
/// only when every slot failed to decode, which means the file is not a cndb
/// database or both headers are damaged.
pub fn select_live(slots: &[Option<Header>; HEADER_SLOTS]) -> Result<(Header, usize)> {
    slots
        .iter()
        .enumerate()
        .filter_map(|(i, h)| h.map(|h| (h, i)))
        .max_by_key(|(h, _)| h.generation)
        .ok_or(CndbError::NoValidHeader)
}

/// The slot a commit should overwrite: any invalid slot, else the oldest.
///
/// Never returns the live slot, so the live header survives the write.
pub fn victim_slot(slots: &[Option<Header>; HEADER_SLOTS]) -> usize {
    if let Some(i) = slots.iter().position(|h| h.is_none()) {
        return i;
    }
    slots
        .iter()
        .enumerate()
        .min_by_key(|(_, h)| h.expect("all slots valid").generation)
        .map(|(i, _)| i)
        .expect("at least one slot")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Header {
        Header {
            generation: 7,
            master_ptr: 4096,
            master_len: 512,
            log_end: 4608,
        }
    }

    #[test]
    fn layout_is_exactly_one_slot() {
        assert_eq!(sample().encode().len(), HEADER_SIZE);
        assert_eq!(LOG_START, 128);
    }

    #[test]
    fn roundtrips() {
        let h = sample();
        assert_eq!(Header::decode(&h.encode(), 0).unwrap(), h);
    }

    #[test]
    fn initial_header_points_past_the_header_region() {
        let h = Header::initial();
        assert_eq!(h.log_end, LOG_START);
        assert_eq!(h.master_len, 0);
        assert_eq!(Header::decode(&h.encode(), 0).unwrap(), h);
    }

    #[test]
    fn rejects_bad_magic() {
        let mut buf = sample().encode();
        buf[0] = b'X';
        assert!(matches!(
            Header::decode(&buf, 0),
            Err(CndbError::BadMagic { .. })
        ));
    }

    #[test]
    fn rejects_wrong_version() {
        let mut h = sample().encode();
        h[4..8].copy_from_slice(&0x0002_0000u32.to_le_bytes());
        // Re-checksum so version, not corruption, is what fails.
        h[CRC_RANGE].fill(0);
        let crc = crc32fast::hash(&h);
        h[CRC_RANGE].copy_from_slice(&crc.to_le_bytes());
        assert!(matches!(
            Header::decode(&h, 0),
            Err(CndbError::UnsupportedVersion { .. })
        ));
    }

    #[test]
    fn detects_corruption_in_every_field() {
        // Byte 40..44 is the checksum itself; corrupting it must also fail.
        for i in 0..HEADER_SIZE {
            let mut buf = sample().encode();
            buf[i] ^= 0xFF;
            let decoded = Header::decode(&buf, 0);
            assert!(
                decoded.is_err(),
                "corrupting byte {i} went undetected: {decoded:?}"
            );
        }
    }

    #[test]
    fn live_slot_is_the_highest_generation() {
        let old = Header {
            generation: 3,
            ..sample()
        };
        let new = Header {
            generation: 4,
            ..sample()
        };
        let (live, slot) = select_live(&[Some(old), Some(new)]).unwrap();
        assert_eq!((live.generation, slot), (4, 1));

        let (live, slot) = select_live(&[Some(new), Some(old)]).unwrap();
        assert_eq!((live.generation, slot), (4, 0));
    }

    #[test]
    fn live_slot_ignores_a_damaged_partner() {
        let h = sample();
        assert_eq!(select_live(&[None, Some(h)]).unwrap().1, 1);
        assert_eq!(select_live(&[Some(h), None]).unwrap().1, 0);
        assert!(matches!(
            select_live(&[None, None]),
            Err(CndbError::NoValidHeader)
        ));
    }

    #[test]
    fn victim_is_never_the_live_slot() {
        let old = Header {
            generation: 3,
            ..sample()
        };
        let new = Header {
            generation: 4,
            ..sample()
        };
        // Oldest loses.
        assert_eq!(victim_slot(&[Some(old), Some(new)]), 0);
        assert_eq!(victim_slot(&[Some(new), Some(old)]), 1);
        // An empty slot is claimed before a valid one is recycled.
        assert_eq!(victim_slot(&[None, Some(new)]), 0);
        assert_eq!(victim_slot(&[Some(new), None]), 1);
    }
}
