//! The public database handle.
//!
//! M1 exposes documents. M2 layers the graph model over this — `insert_node`,
//! `insert_edge`, `neighbors` — reusing the same engine and commit protocol.

use std::path::Path;

use serde_json::Value;

use crate::document::codec::{from_bson_bytes, to_bson_bytes};
use crate::error::Result;
use crate::storage::{DocId, RecordKind, Stats, StorageEngine};

/// An open cndb database.
///
/// Writes are buffered until [`commit`](Self::commit); a handle dropped without
/// committing discards them, which is what makes an interrupted run leave the
/// file at its last consistent state rather than half-updated.
///
/// ```
/// # fn main() -> cndb::Result<()> {
/// # let dir = tempfile::TempDir::new().unwrap();
/// # let path = dir.path().join("graph.cndb");
/// use serde_json::json;
///
/// let mut db = cndb::Cndb::open(&path)?;
/// let id = db.insert(&json!({ "name": "parse_header" }))?;
/// db.commit()?;
///
/// assert_eq!(db.get(id)?["name"], "parse_header");
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct Cndb {
    store: StorageEngine,
}

impl Cndb {
    /// Open `path`, creating an empty database if it does not exist.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        Ok(Cndb {
            store: StorageEngine::open(path)?,
        })
    }

    /// Store a JSON object and return its id.
    ///
    /// Errors if `doc` is not an object. Durable after [`commit`](Self::commit).
    pub fn insert(&mut self, doc: &Value) -> Result<DocId> {
        let bytes = to_bson_bytes(doc)?;
        self.store.append(RecordKind::Blob, &bytes)
    }

    /// Fetch a document by id.
    pub fn get(&self, id: DocId) -> Result<Value> {
        from_bson_bytes(&self.store.read(id)?)
    }

    /// Delete a document.
    pub fn delete(&mut self, id: DocId) -> Result<()> {
        self.store.tombstone(id)
    }

    /// Make every write since the last commit durable.
    pub fn commit(&mut self) -> Result<()> {
        self.store.commit()
    }

    /// Check every committed record's checksum, returning how many were read.
    pub fn verify(&self) -> Result<usize> {
        self.store.verify()
    }

    /// Number of live documents.
    pub fn len(&self) -> usize {
        self.store.len()
    }

    /// True when no live documents remain.
    pub fn is_empty(&self) -> bool {
        self.store.is_empty()
    }

    /// Every live document id, in no particular order.
    pub fn ids(&self) -> impl Iterator<Item = DocId> + '_ {
        self.store.live_ids()
    }

    /// A snapshot of engine state.
    pub fn stats(&self) -> Stats {
        self.store.stats()
    }

    /// The underlying storage engine.
    pub fn store(&self) -> &StorageEngine {
        &self.store
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::CndbError;
    use serde_json::json;
    use tempfile::TempDir;

    #[test]
    fn documents_roundtrip_across_a_reopen() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("api.cndb");

        let (kept, removed) = {
            let mut db = Cndb::open(&path).unwrap();
            let kept = db
                .insert(&json!({ "kind": "Function", "name": "commit" }))
                .unwrap();
            let removed = db
                .insert(&json!({ "kind": "Function", "name": "gone" }))
                .unwrap();
            db.delete(removed).unwrap();
            db.commit().unwrap();
            (kept, removed)
        };

        let db = Cndb::open(&path).unwrap();
        assert_eq!(db.len(), 1);
        assert_eq!(db.get(kept).unwrap()["name"], "commit");
        assert!(matches!(db.get(removed), Err(CndbError::Deleted(_))));
        assert_eq!(db.ids().collect::<Vec<_>>(), vec![kept]);
        db.verify().unwrap();
    }

    #[test]
    fn dropping_without_committing_discards_writes() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("api.cndb");
        {
            let mut db = Cndb::open(&path).unwrap();
            db.insert(&json!({ "name": "never committed" })).unwrap();
        }
        assert!(Cndb::open(&path).unwrap().is_empty());
    }

    #[test]
    fn non_objects_are_refused_before_anything_is_written() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("api.cndb");
        let mut db = Cndb::open(&path).unwrap();

        assert!(matches!(
            db.insert(&json!([1, 2, 3])),
            Err(CndbError::NotAnObject { .. })
        ));
        assert!(db.is_empty(), "a rejected insert must not consume an id");
    }
}
