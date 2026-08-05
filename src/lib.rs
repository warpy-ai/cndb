//! # cndb — ContextDB
//!
//! A single-file, embedded graph knowledge base for codebases.
//!
//! cndb is a storage engine in its own right, not a layer over another
//! database. Everything lives in one portable `.cndb` file: an append-only
//! record log plus a master index committed atomically via two alternating
//! header slots. See `docs/ARCHITECTURE.md` for the full design.
//!
//! ## Status
//!
//! M1 — the storage core — is implemented: file format, record log, document
//! offset map, BSON codec, and crash-safe commit and recovery. The graph model,
//! extraction and query engines follow in M2 through M5.
//!
//! ```
//! # fn main() -> cndb::Result<()> {
//! # let dir = tempfile::TempDir::new().unwrap();
//! # let path = dir.path().join("graph.cndb");
//! use serde_json::json;
//!
//! let mut db = cndb::Cndb::open(&path)?;
//! let id = db.insert(&json!({ "kind": "Function", "name": "parse_header" }))?;
//! db.commit()?;
//! assert_eq!(db.get(id)?["kind"], "Function");
//! # Ok(())
//! # }
//! ```

pub mod api;
pub mod document;
pub mod error;
pub mod storage;

pub use api::Cndb;
pub use document::{from_bson_bytes, to_bson_bytes};
pub use error::{CndbError, Result};
pub use storage::{DocId, MasterIndex, Record, RecordKind, Stats, StorageEngine, FORMAT_VERSION};
