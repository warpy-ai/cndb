//! Error types for cndb.

use crate::storage::DocId;

/// Result alias used throughout cndb.
pub type Result<T> = std::result::Result<T, CndbError>;

/// Every way a cndb operation can fail.
#[derive(Debug, thiserror::Error)]
pub enum CndbError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("not a cndb file: expected magic {expected:?}, found {found:?}")]
    BadMagic { expected: [u8; 4], found: [u8; 4] },

    #[error("unsupported format version {found:#010x}; this build supports {supported:#010x}")]
    UnsupportedVersion { found: u32, supported: u32 },

    #[error("no valid header: both slots are corrupt or this is not a cndb database")]
    NoValidHeader,

    #[error("header slot {slot} failed its checksum")]
    HeaderChecksum { slot: usize },

    #[error("record at offset {offset} failed its checksum")]
    RecordChecksum { offset: u64 },

    #[error("record at offset {offset} declares {len} bytes, over the {max} byte limit")]
    RecordTooLarge { offset: u64, len: u64, max: u64 },

    #[error("unknown record kind {kind} at offset {offset}")]
    UnknownRecordKind { kind: u8, offset: u64 },

    #[error("unexpected end of file at offset {offset}: needed {needed} bytes")]
    UnexpectedEof { offset: u64, needed: u64 },

    #[error("document {0} not found")]
    NotFound(DocId),

    #[error("document {0} was deleted")]
    Deleted(DocId),

    #[error("bson: {0}")]
    Bson(String),

    #[error("only JSON objects can be stored as documents, found {found}")]
    NotAnObject { found: &'static str },

    #[error("master index is corrupt: {0}")]
    CorruptIndex(String),
}
