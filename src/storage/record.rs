//! Record framing for the append-only log.
//!
//! Every record is `[len: u32][kind: u8][crc32: u32][payload]`, little-endian,
//! with the checksum taken over the payload alone.
//!
//! Issue #1 framed records as `[len][BSON]` and issue #2 as
//! `[len][collection][BSON]`. Neither carries a checksum, so a partially
//! written or bit-rotted record deserializes into plausible garbage instead of
//! failing. The kind tag replaces the inline collection name: it is one byte
//! rather than a length-prefixed string, and it lets the master index be
//! written into the same log as a well-framed record that scans skip over.

use crate::error::{CndbError, Result};
use serde::{Deserialize, Serialize};

/// Bytes of framing preceding every payload.
pub const RECORD_HEADER_SIZE: u64 = 9;

/// Largest payload accepted, a guard against absurd allocations when framing is
/// corrupt.
pub const MAX_RECORD_LEN: u64 = 64 * 1024 * 1024;

/// Identifier of a stored document, unique for the life of the database.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
pub struct DocId(pub u64);

impl std::fmt::Display for DocId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// What a record holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum RecordKind {
    /// A graph node. Written from M2 onward.
    Node = 0,
    /// A graph edge. Written from M2 onward.
    Edge = 1,
    /// An unstructured document.
    Blob = 2,
    /// Marks an earlier record dead. Payload is the target `DocId`.
    Tombstone = 3,
    /// A serialized master index, written by every commit.
    MasterIndex = 4,
}

impl RecordKind {
    /// Parse a kind tag. `offset` only labels errors.
    pub fn from_tag(tag: u8, offset: u64) -> Result<Self> {
        match tag {
            0 => Ok(RecordKind::Node),
            1 => Ok(RecordKind::Edge),
            2 => Ok(RecordKind::Blob),
            3 => Ok(RecordKind::Tombstone),
            4 => Ok(RecordKind::MasterIndex),
            kind => Err(CndbError::UnknownRecordKind { kind, offset }),
        }
    }

    /// True for records that carry user data rather than engine bookkeeping.
    pub fn is_document(self) -> bool {
        matches!(self, RecordKind::Node | RecordKind::Edge | RecordKind::Blob)
    }
}

/// A record read back out of the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub kind: RecordKind,
    pub payload: Vec<u8>,
}

/// Build the 9-byte frame preceding `payload`.
pub fn encode_frame(kind: RecordKind, payload: &[u8]) -> [u8; RECORD_HEADER_SIZE as usize] {
    let mut frame = [0u8; RECORD_HEADER_SIZE as usize];
    frame[0..4].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    frame[4] = kind as u8;
    frame[5..9].copy_from_slice(&crc32fast::hash(payload).to_le_bytes());
    frame
}

/// A parsed frame, before its payload is read.
#[derive(Debug, Clone, Copy)]
pub struct Frame {
    pub len: u64,
    pub kind: RecordKind,
    pub crc: u32,
}

impl Frame {
    /// Total on-disk size of the record, framing included.
    pub fn total_len(&self) -> u64 {
        RECORD_HEADER_SIZE + self.len
    }
}

/// Parse a frame read from `offset`.
pub fn decode_frame(buf: &[u8; RECORD_HEADER_SIZE as usize], offset: u64) -> Result<Frame> {
    let len = u32::from_le_bytes(buf[0..4].try_into().expect("4 bytes")) as u64;
    if len > MAX_RECORD_LEN {
        return Err(CndbError::RecordTooLarge {
            offset,
            len,
            max: MAX_RECORD_LEN,
        });
    }
    Ok(Frame {
        len,
        kind: RecordKind::from_tag(buf[4], offset)?,
        crc: u32::from_le_bytes(buf[5..9].try_into().expect("4 bytes")),
    })
}

/// Verify a payload against the checksum in its frame.
pub fn verify_payload(frame: &Frame, payload: &[u8], offset: u64) -> Result<()> {
    if crc32fast::hash(payload) != frame.crc {
        return Err(CndbError::RecordChecksum { offset });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrips() {
        let payload = b"the quick brown fox";
        let frame = encode_frame(RecordKind::Blob, payload);
        let decoded = decode_frame(&frame, 128).unwrap();

        assert_eq!(decoded.len, payload.len() as u64);
        assert_eq!(decoded.kind, RecordKind::Blob);
        assert_eq!(
            decoded.total_len(),
            RECORD_HEADER_SIZE + payload.len() as u64
        );
        verify_payload(&decoded, payload, 128).unwrap();
    }

    #[test]
    fn empty_payload_is_legal() {
        let frame = encode_frame(RecordKind::Tombstone, &[]);
        let decoded = decode_frame(&frame, 0).unwrap();
        assert_eq!(decoded.len, 0);
        verify_payload(&decoded, &[], 0).unwrap();
    }

    #[test]
    fn detects_a_single_flipped_bit_in_the_payload() {
        let payload = b"cndb storage core".to_vec();
        let frame = decode_frame(&encode_frame(RecordKind::Blob, &payload), 0).unwrap();

        for i in 0..payload.len() {
            let mut corrupt = payload.clone();
            corrupt[i] ^= 0x01;
            assert!(
                matches!(
                    verify_payload(&frame, &corrupt, 0),
                    Err(CndbError::RecordChecksum { .. })
                ),
                "flipping a bit in byte {i} went undetected"
            );
        }
    }

    #[test]
    fn every_kind_survives_its_tag() {
        for kind in [
            RecordKind::Node,
            RecordKind::Edge,
            RecordKind::Blob,
            RecordKind::Tombstone,
            RecordKind::MasterIndex,
        ] {
            let frame = decode_frame(&encode_frame(kind, b"x"), 0).unwrap();
            assert_eq!(frame.kind, kind);
        }
    }

    #[test]
    fn rejects_unknown_kinds() {
        // 5..=255 are unallocated and reserved for later record types.
        let mut frame = encode_frame(RecordKind::Blob, b"x");
        frame[4] = 5;
        assert!(matches!(
            decode_frame(&frame, 64),
            Err(CndbError::UnknownRecordKind {
                kind: 5,
                offset: 64
            })
        ));
    }

    #[test]
    fn rejects_absurd_lengths_before_allocating() {
        let mut frame = encode_frame(RecordKind::Blob, b"x");
        frame[0..4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(
            decode_frame(&frame, 0),
            Err(CndbError::RecordTooLarge { .. })
        ));
    }

    #[test]
    fn only_user_data_counts_as_a_document() {
        assert!(RecordKind::Node.is_document());
        assert!(RecordKind::Edge.is_document());
        assert!(RecordKind::Blob.is_document());
        assert!(!RecordKind::Tombstone.is_document());
        assert!(!RecordKind::MasterIndex.is_document());
    }
}
