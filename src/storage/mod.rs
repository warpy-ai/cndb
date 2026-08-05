//! The storage engine: a single file, an append-only log, and an atomically
//! committed index.
//!
//! See `docs/ARCHITECTURE.md` §3 for the file layout and commit protocol.

pub mod engine;
pub mod header;
pub mod index;
mod pos_io;
pub mod record;

pub use engine::{Stats, StorageEngine};
pub use header::{Header, FORMAT_VERSION, HEADER_SIZE, HEADER_SLOTS, LOG_START, MAGIC};
pub use index::MasterIndex;
pub use record::{DocId, Record, RecordKind, MAX_RECORD_LEN, RECORD_HEADER_SIZE};
