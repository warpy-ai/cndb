//! The master index: every in-memory lookup structure, in one committed blob.
//!
//! M1 carries the document offset map (issue #7), the tombstone set and the id
//! allocator. M2 adds the adjacency, name and full-text indexes described in
//! `docs/ARCHITECTURE.md` §3.3 as further fields here.
//!
//! bincode is not self-describing, so adding a field breaks compatibility with
//! older files. `FORMAT_VERSION` in the header is what guards that: a file
//! written by an older layout is rejected at open rather than misread.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::error::{CndbError, Result};
use crate::storage::record::DocId;

/// All indexes, serialized as one record on each commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MasterIndex {
    /// Document id to its byte offset in the log.
    pub offsets: HashMap<DocId, u64>,
    /// Documents tombstoned but not yet compacted away.
    pub dead: HashSet<DocId>,
    /// Next id to hand out. Never reused, even after a delete.
    pub next_doc_id: u64,
}

impl Default for MasterIndex {
    fn default() -> Self {
        MasterIndex {
            offsets: HashMap::new(),
            dead: HashSet::new(),
            // Ids start at 1 so 0 stays available as a niche/sentinel.
            next_doc_id: 1,
        }
    }
}

impl MasterIndex {
    /// Reserve the next document id.
    pub fn allocate_id(&mut self) -> DocId {
        let id = DocId(self.next_doc_id);
        self.next_doc_id += 1;
        id
    }

    /// Record where a document landed.
    pub fn insert(&mut self, id: DocId, offset: u64) {
        self.offsets.insert(id, offset);
    }

    /// Offset of a live document, or `None` if unknown or deleted.
    pub fn offset_of(&self, id: DocId) -> Option<u64> {
        if self.dead.contains(&id) {
            return None;
        }
        self.offsets.get(&id).copied()
    }

    /// True if the id exists and has not been deleted.
    pub fn is_live(&self, id: DocId) -> bool {
        self.offsets.contains_key(&id) && !self.dead.contains(&id)
    }

    /// Number of live documents.
    pub fn live_count(&self) -> usize {
        self.offsets.len() - self.dead.len()
    }

    /// Number of tombstoned documents awaiting compaction.
    pub fn dead_count(&self) -> usize {
        self.dead.len()
    }

    /// Share of stored documents that are tombstones, 0.0 when empty.
    ///
    /// M5 compacts once this crosses its threshold.
    pub fn dead_ratio(&self) -> f64 {
        if self.offsets.is_empty() {
            return 0.0;
        }
        self.dead.len() as f64 / self.offsets.len() as f64
    }

    /// Every live document id, in no particular order.
    pub fn live_ids(&self) -> impl Iterator<Item = DocId> + '_ {
        self.offsets
            .keys()
            .copied()
            .filter(|id| !self.dead.contains(id))
    }

    /// Serialize for the master index record.
    pub fn encode(&self) -> Result<Vec<u8>> {
        bincode::serialize(self).map_err(|e| CndbError::CorruptIndex(e.to_string()))
    }

    /// Deserialize from a master index record payload.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        bincode::deserialize(bytes).map_err(|e| CndbError::CorruptIndex(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_start_at_one_and_never_repeat() {
        let mut idx = MasterIndex::default();
        assert_eq!(idx.allocate_id(), DocId(1));
        assert_eq!(idx.allocate_id(), DocId(2));

        idx.insert(DocId(1), 128);
        idx.dead.insert(DocId(1));
        // Deleting does not free the id for reuse.
        assert_eq!(idx.allocate_id(), DocId(3));
    }

    #[test]
    fn tombstones_hide_documents_without_losing_the_offset() {
        let mut idx = MasterIndex::default();
        idx.insert(DocId(1), 128);
        assert_eq!(idx.offset_of(DocId(1)), Some(128));
        assert!(idx.is_live(DocId(1)));

        idx.dead.insert(DocId(1));
        assert_eq!(idx.offset_of(DocId(1)), None);
        assert!(!idx.is_live(DocId(1)));
        // The offset survives so compaction can still find the record.
        assert_eq!(idx.offsets.get(&DocId(1)), Some(&128));
    }

    #[test]
    fn counts_and_ratio_track_tombstones() {
        let mut idx = MasterIndex::default();
        assert_eq!(idx.dead_ratio(), 0.0, "empty index must not divide by zero");

        for i in 1..=4u64 {
            idx.insert(DocId(i), i * 100);
        }
        assert_eq!((idx.live_count(), idx.dead_count()), (4, 0));

        idx.dead.insert(DocId(1));
        assert_eq!((idx.live_count(), idx.dead_count()), (3, 1));
        assert_eq!(idx.dead_ratio(), 0.25);

        let mut live: Vec<_> = idx.live_ids().map(|d| d.0).collect();
        live.sort_unstable();
        assert_eq!(live, vec![2, 3, 4]);
    }

    #[test]
    fn roundtrips_through_bincode() {
        let mut idx = MasterIndex::default();
        for i in 1..=32u64 {
            let id = idx.allocate_id();
            idx.insert(id, i * 137);
        }
        idx.dead.insert(DocId(3));

        assert_eq!(MasterIndex::decode(&idx.encode().unwrap()).unwrap(), idx);
    }

    #[test]
    fn rejects_a_truncated_blob() {
        let idx = MasterIndex::default();
        let bytes = idx.encode().unwrap();
        assert!(matches!(
            MasterIndex::decode(&bytes[..bytes.len() / 2]),
            Err(CndbError::CorruptIndex(_))
        ));
    }
}
