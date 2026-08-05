//! The append-only storage engine.
//!
//! # Commit protocol
//!
//! A commit never overwrites a byte the live generation depends on:
//!
//! 1. Records are appended at `log_end` and the payload is flushed.
//! 2. The master index is appended *as an ordinary record*, then flushed.
//! 3. A header with `generation + 1` is written to the older slot and flushed.
//!
//! Step 3 is the commit point. A crash before it leaves the previous header
//! untouched, still pointing at the previous master index, with every new byte
//! sitting beyond the old `log_end` where the next append overwrites it. A
//! crash *during* it tears at most one header slot, and the other survives.
//!
//! Writing the master index into the log rather than into reserved space is
//! what makes this work. Appends always start past the live master record, so
//! it stays readable until its successor is committed, and scans skip it by
//! kind. Superseded master records become dead space that compaction reclaims
//! (M5). This is also why issue #14's optional write-ahead log is absent: the
//! ordering above already makes a commit atomic.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use crate::error::{CndbError, Result};
use crate::storage::header::{self, Header, HEADER_SIZE, HEADER_SLOTS, LOG_START};
use crate::storage::index::MasterIndex;
use crate::storage::pos_io::{read_exact_at, write_all_at};
use crate::storage::record::{
    decode_frame, encode_frame, verify_payload, DocId, Frame, Record, RecordKind, MAX_RECORD_LEN,
    RECORD_HEADER_SIZE,
};

/// A single `.cndb` file, opened for reading and writing.
///
/// One writer at a time. Reads take `&self` and use positioned I/O, so the
/// multiple-reader half of the roadmap's concurrency model needs no API change.
#[derive(Debug)]
pub struct StorageEngine {
    file: File,
    path: PathBuf,
    /// The live header, i.e. the last committed state.
    header: Header,
    /// Both decoded slots, so a commit can pick the one safe to overwrite.
    slots: [Option<Header>; HEADER_SLOTS],
    /// Which slot `header` came from.
    live_slot: usize,
    index: MasterIndex,
    /// First free byte. Runs ahead of `header.log_end` between commits.
    end: u64,
    /// Whether anything has been appended since the last commit.
    dirty: bool,
}

/// A snapshot of engine state, for diagnostics and tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub generation: u64,
    pub live_docs: usize,
    pub dead_docs: usize,
    pub committed_len: u64,
    pub pending_len: u64,
}

impl StorageEngine {
    /// Open `path`, creating an empty database if it does not exist.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;

        let file_len = file.metadata()?.len();
        if file_len > 0 {
            return Self::load(file, path, file_len);
        }

        let mut engine = StorageEngine {
            file,
            path,
            header: Header::initial(),
            slots: [None, None],
            live_slot: 0,
            index: MasterIndex::default(),
            end: LOG_START,
            dirty: false,
        };
        engine.initialize()?;
        Ok(engine)
    }

    /// Lay down the header region of a brand-new database.
    fn initialize(&mut self) -> Result<()> {
        let mut region = [0u8; HEADER_SIZE * HEADER_SLOTS];
        // Slot 0 holds generation 0; slot 1 stays zeroed and so decodes as
        // invalid, which makes it the first commit's victim.
        region[..HEADER_SIZE].copy_from_slice(&self.header.encode());
        write_all_at(&self.file, &region, 0)?;
        self.file.sync_all()?;
        self.slots = [Some(self.header), None];
        self.live_slot = 0;
        Ok(())
    }

    /// Recover an existing database from its headers.
    fn load(file: File, path: PathBuf, file_len: u64) -> Result<Self> {
        if file_len < LOG_START {
            return Err(CndbError::UnexpectedEof {
                offset: 0,
                needed: LOG_START,
            });
        }

        let mut region = [0u8; HEADER_SIZE * HEADER_SLOTS];
        read_exact_at(&file, &mut region, 0)?;

        let mut slots: [Option<Header>; HEADER_SLOTS] = [None; HEADER_SLOTS];
        let mut first_error = None;
        for (slot, out) in slots.iter_mut().enumerate() {
            let bytes: [u8; HEADER_SIZE] = region[slot * HEADER_SIZE..(slot + 1) * HEADER_SIZE]
                .try_into()
                .expect("slot sized");
            match Header::decode(&bytes, slot) {
                Ok(h) => *out = Some(h),
                Err(e) => {
                    first_error.get_or_insert(e);
                }
            }
        }

        // When both slots fail, report why the first one did: "wrong magic" or
        // "unsupported version" is far more useful than "no valid header".
        let (header, live_slot) =
            header::select_live(&slots).map_err(|fallback| first_error.unwrap_or(fallback))?;

        if header.log_end > file_len {
            return Err(CndbError::UnexpectedEof {
                offset: file_len,
                needed: header.log_end,
            });
        }

        let index = if header.master_len == 0 {
            MasterIndex::default()
        } else {
            let record = read_record_at(&file, header.master_ptr)?;
            if record.kind != RecordKind::MasterIndex {
                return Err(CndbError::CorruptIndex(format!(
                    "record at {} is {:?}, not a master index",
                    header.master_ptr, record.kind
                )));
            }
            MasterIndex::decode(&record.payload)?
        };

        Ok(StorageEngine {
            file,
            path,
            header,
            slots,
            live_slot,
            index,
            end: header.log_end,
            dirty: false,
        })
    }

    /// Append a document and return its id.
    ///
    /// Durable only after [`commit`](Self::commit).
    pub fn append(&mut self, kind: RecordKind, payload: &[u8]) -> Result<DocId> {
        let offset = self.append_raw(kind, payload)?;
        let id = self.index.allocate_id();
        self.index.insert(id, offset);
        Ok(id)
    }

    /// Append a framed record and return where it landed, leaving the index
    /// alone. Used for documents and for the master index itself.
    fn append_raw(&mut self, kind: RecordKind, payload: &[u8]) -> Result<u64> {
        let len = payload.len() as u64;
        if len > MAX_RECORD_LEN {
            return Err(CndbError::RecordTooLarge {
                offset: self.end,
                len,
                max: MAX_RECORD_LEN,
            });
        }

        let offset = self.end;
        write_all_at(&self.file, &encode_frame(kind, payload), offset)?;
        write_all_at(&self.file, payload, offset + RECORD_HEADER_SIZE)?;
        self.end = offset + RECORD_HEADER_SIZE + len;
        self.dirty = true;
        Ok(offset)
    }

    /// Read a document's payload.
    pub fn read(&self, id: DocId) -> Result<Vec<u8>> {
        Ok(self.read_record(id)?.payload)
    }

    /// Read a document with its record kind.
    pub fn read_record(&self, id: DocId) -> Result<Record> {
        if self.index.dead.contains(&id) {
            return Err(CndbError::Deleted(id));
        }
        let offset = self
            .index
            .offsets
            .get(&id)
            .copied()
            .ok_or(CndbError::NotFound(id))?;
        read_record_at(&self.file, offset)
    }

    /// Mark a document deleted.
    ///
    /// Appends a tombstone so the deletion is itself recoverable, then hides
    /// the document. The original record stays on disk until compaction.
    pub fn tombstone(&mut self, id: DocId) -> Result<()> {
        if self.index.dead.contains(&id) {
            return Err(CndbError::Deleted(id));
        }
        if !self.index.offsets.contains_key(&id) {
            return Err(CndbError::NotFound(id));
        }
        self.append_raw(RecordKind::Tombstone, &id.0.to_le_bytes())?;
        self.index.dead.insert(id);
        Ok(())
    }

    /// Make every append since the last commit durable.
    ///
    /// A no-op when nothing has changed.
    pub fn commit(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }

        // 1. Records reach disk before anything points at them.
        self.file.sync_data()?;

        // 2. The master index goes into the log like any other record.
        let payload = self.index.encode()?;
        let master_ptr = self.append_raw(RecordKind::MasterIndex, &payload)?;
        self.file.sync_data()?;

        // 3. The commit point.
        let next = Header {
            generation: self.header.generation + 1,
            master_ptr,
            master_len: payload.len() as u64,
            log_end: self.end,
        };
        let slot = header::victim_slot(&self.slots);
        debug_assert_ne!(slot, self.live_slot, "commit would clobber the live header");
        write_all_at(&self.file, &next.encode(), Header::slot_offset(slot))?;
        self.file.sync_data()?;

        self.header = next;
        self.slots[slot] = Some(next);
        self.live_slot = slot;
        self.dirty = false;
        Ok(())
    }

    /// Walk every record from the start of the log, checking all checksums.
    ///
    /// Returns how many records were read. Only committed bytes are scanned.
    pub fn verify(&self) -> Result<usize> {
        let mut offset = LOG_START;
        let mut count = 0;
        while offset < self.header.log_end {
            let frame = read_frame_at(&self.file, offset)?;
            let mut payload = vec![0u8; frame.len as usize];
            read_exact_at(&self.file, &mut payload, offset + RECORD_HEADER_SIZE)?;
            verify_payload(&frame, &payload, offset)?;
            offset += frame.total_len();
            count += 1;
        }
        if offset != self.header.log_end {
            return Err(CndbError::CorruptIndex(format!(
                "log ends at {offset}, header says {}",
                self.header.log_end
            )));
        }
        Ok(count)
    }

    /// The committed generation. Increments on every commit that does work.
    pub fn generation(&self) -> u64 {
        self.header.generation
    }

    /// Which header slot currently holds the live generation.
    pub fn live_slot(&self) -> usize {
        self.live_slot
    }

    /// True when appends are waiting for a commit.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Path this database was opened from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read-only view of the master index.
    pub fn index(&self) -> &MasterIndex {
        &self.index
    }

    /// Number of live documents.
    pub fn len(&self) -> usize {
        self.index.live_count()
    }

    /// True when no live documents remain.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Every live document id, in no particular order.
    pub fn live_ids(&self) -> impl Iterator<Item = DocId> + '_ {
        self.index.live_ids()
    }

    /// A snapshot of engine state.
    pub fn stats(&self) -> Stats {
        Stats {
            generation: self.header.generation,
            live_docs: self.index.live_count(),
            dead_docs: self.index.dead_count(),
            committed_len: self.header.log_end,
            pending_len: self.end,
        }
    }
}

/// Read and validate a record frame at `offset`.
fn read_frame_at(file: &File, offset: u64) -> Result<Frame> {
    let mut buf = [0u8; RECORD_HEADER_SIZE as usize];
    read_exact_at(file, &mut buf, offset)?;
    decode_frame(&buf, offset)
}

/// Read a whole record at `offset`, verifying its checksum.
fn read_record_at(file: &File, offset: u64) -> Result<Record> {
    let frame = read_frame_at(file, offset)?;
    let mut payload = vec![0u8; frame.len as usize];
    read_exact_at(file, &mut payload, offset + RECORD_HEADER_SIZE)?;
    verify_payload(&frame, &payload, offset)?;
    Ok(Record {
        kind: frame.kind,
        payload,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn scratch() -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.cndb");
        (dir, path)
    }

    fn blob(engine: &mut StorageEngine, body: &str) -> DocId {
        engine.append(RecordKind::Blob, body.as_bytes()).unwrap()
    }

    /// Overwrite bytes on disk behind the engine's back, to simulate damage.
    fn damage(path: &Path, offset: u64, bytes: &[u8]) {
        let file = OpenOptions::new().write(true).open(path).unwrap();
        write_all_at(&file, bytes, offset).unwrap();
        file.sync_all().unwrap();
    }

    #[test]
    fn a_new_database_starts_empty_and_valid() {
        let (_dir, path) = scratch();
        let engine = StorageEngine::open(&path).unwrap();

        assert_eq!(engine.generation(), 0);
        assert!(engine.is_empty());
        assert!(!engine.is_dirty());
        assert_eq!(engine.verify().unwrap(), 0);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), LOG_START);
    }

    #[test]
    fn appends_are_readable_before_they_are_committed() {
        let (_dir, path) = scratch();
        let mut engine = StorageEngine::open(&path).unwrap();

        let id = blob(&mut engine, "hello");
        assert!(engine.is_dirty());
        assert_eq!(engine.read(id).unwrap(), b"hello");
        assert_eq!(engine.len(), 1);
    }

    #[test]
    fn ids_are_unique_and_documents_do_not_collide() {
        let (_dir, path) = scratch();
        let mut engine = StorageEngine::open(&path).unwrap();

        let ids: Vec<_> = (0..64)
            .map(|i| blob(&mut engine, &format!("doc-{i}")))
            .collect();
        engine.commit().unwrap();

        let unique: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len());
        for (i, id) in ids.iter().enumerate() {
            assert_eq!(engine.read(*id).unwrap(), format!("doc-{i}").as_bytes());
        }
    }

    #[test]
    fn data_survives_a_reopen() {
        let (_dir, path) = scratch();
        let id = {
            let mut engine = StorageEngine::open(&path).unwrap();
            let id = blob(&mut engine, "durable");
            engine.commit().unwrap();
            id
        };

        let engine = StorageEngine::open(&path).unwrap();
        assert_eq!(engine.generation(), 1);
        assert_eq!(engine.read(id).unwrap(), b"durable");
        assert_eq!(engine.len(), 1);
    }

    #[test]
    fn uncommitted_appends_are_lost_on_reopen() {
        let (_dir, path) = scratch();
        let (kept, dropped) = {
            let mut engine = StorageEngine::open(&path).unwrap();
            let kept = blob(&mut engine, "committed");
            engine.commit().unwrap();
            // Simulates a crash: written and flushed, but never committed.
            let dropped = blob(&mut engine, "in flight");
            (kept, dropped)
        };

        let engine = StorageEngine::open(&path).unwrap();
        assert_eq!(engine.read(kept).unwrap(), b"committed");
        assert!(matches!(engine.read(dropped), Err(CndbError::NotFound(_))));
        assert_eq!(engine.len(), 1);
    }

    #[test]
    fn an_uncommitted_append_is_overwritten_not_orphaned() {
        let (_dir, path) = scratch();
        {
            let mut engine = StorageEngine::open(&path).unwrap();
            blob(&mut engine, "committed");
            engine.commit().unwrap();
            blob(&mut engine, "this text must not survive");
        }

        let mut engine = StorageEngine::open(&path).unwrap();
        let replacement = blob(&mut engine, "second");
        engine.commit().unwrap();

        // The lost record's space was reused, and the log still scans clean.
        assert_eq!(engine.read(replacement).unwrap(), b"second");
        engine.verify().unwrap();
    }

    #[test]
    fn ids_do_not_restart_after_a_reopen() {
        let (_dir, path) = scratch();
        let first = {
            let mut engine = StorageEngine::open(&path).unwrap();
            let id = blob(&mut engine, "a");
            engine.commit().unwrap();
            id
        };

        let mut engine = StorageEngine::open(&path).unwrap();
        let second = blob(&mut engine, "b");
        assert!(second > first, "{second} should follow {first}");
        assert_eq!(engine.read(first).unwrap(), b"a");
    }

    #[test]
    fn commits_alternate_header_slots() {
        let (_dir, path) = scratch();
        let mut engine = StorageEngine::open(&path).unwrap();
        assert_eq!(engine.live_slot(), 0, "generation 0 is written to slot 0");

        let mut seen = Vec::new();
        for i in 0..6 {
            blob(&mut engine, &format!("v{i}"));
            engine.commit().unwrap();
            seen.push(engine.live_slot());
            assert_eq!(engine.generation(), i + 1);
        }
        assert_eq!(seen, vec![1, 0, 1, 0, 1, 0]);
    }

    #[test]
    fn committing_nothing_does_nothing() {
        let (_dir, path) = scratch();
        let mut engine = StorageEngine::open(&path).unwrap();
        blob(&mut engine, "x");
        engine.commit().unwrap();

        let before = engine.stats();
        engine.commit().unwrap();
        engine.commit().unwrap();
        assert_eq!(
            engine.stats(),
            before,
            "an empty commit must not burn a generation"
        );
    }

    #[test]
    fn a_torn_header_falls_back_to_the_previous_generation() {
        let (_dir, path) = scratch();
        let (first, second) = {
            let mut engine = StorageEngine::open(&path).unwrap();
            let first = blob(&mut engine, "generation one");
            engine.commit().unwrap();
            let second = blob(&mut engine, "generation two");
            engine.commit().unwrap();
            assert_eq!(engine.generation(), 2);
            (first, second)
        };

        // Tear the live slot, exactly as a crash mid-header-write would.
        let live = StorageEngine::open(&path).unwrap().live_slot();
        damage(&path, Header::slot_offset(live), &[0xAB; HEADER_SIZE]);

        // The older slot still describes a complete, consistent database.
        let engine = StorageEngine::open(&path).unwrap();
        assert_eq!(engine.generation(), 1);
        assert_eq!(engine.read(first).unwrap(), b"generation one");
        assert!(matches!(engine.read(second), Err(CndbError::NotFound(_))));
        engine.verify().unwrap();
    }

    #[test]
    fn recovery_still_works_after_writing_over_the_torn_slot() {
        let (_dir, path) = scratch();
        {
            let mut engine = StorageEngine::open(&path).unwrap();
            blob(&mut engine, "one");
            engine.commit().unwrap();
            blob(&mut engine, "two");
            engine.commit().unwrap();
        }

        let live = StorageEngine::open(&path).unwrap().live_slot();
        damage(&path, Header::slot_offset(live), &[0u8; HEADER_SIZE]);

        // Recover, then commit again: the freed slot is reclaimed first.
        let mut engine = StorageEngine::open(&path).unwrap();
        assert_eq!(engine.generation(), 1);
        let id = blob(&mut engine, "three");
        engine.commit().unwrap();
        assert_eq!(engine.generation(), 2);
        assert_eq!(engine.live_slot(), live);

        let engine = StorageEngine::open(&path).unwrap();
        assert_eq!(engine.read(id).unwrap(), b"three");
    }

    #[test]
    fn both_headers_destroyed_is_reported_clearly() {
        let (_dir, path) = scratch();
        {
            let mut engine = StorageEngine::open(&path).unwrap();
            blob(&mut engine, "x");
            engine.commit().unwrap();
        }
        damage(&path, 0, &[0x7Fu8; HEADER_SIZE * HEADER_SLOTS]);
        assert!(matches!(
            StorageEngine::open(&path),
            Err(CndbError::BadMagic { .. })
        ));
    }

    #[test]
    fn a_foreign_file_is_rejected_as_not_a_database() {
        let (_dir, path) = scratch();
        std::fs::write(&path, vec![b'z'; 4096]).unwrap();
        assert!(matches!(
            StorageEngine::open(&path),
            Err(CndbError::BadMagic { .. })
        ));
    }

    #[test]
    fn a_file_too_short_to_hold_headers_is_rejected() {
        let (_dir, path) = scratch();
        std::fs::write(&path, b"CNDB").unwrap();
        assert!(matches!(
            StorageEngine::open(&path),
            Err(CndbError::UnexpectedEof { .. })
        ));
    }

    #[test]
    fn a_truncated_file_is_rejected_rather_than_half_read() {
        let (_dir, path) = scratch();
        {
            let mut engine = StorageEngine::open(&path).unwrap();
            blob(&mut engine, "a document long enough to matter");
            engine.commit().unwrap();
        }

        let len = std::fs::metadata(&path).unwrap().len();
        let file = OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(len - 16).unwrap();
        drop(file);

        assert!(matches!(
            StorageEngine::open(&path),
            Err(CndbError::UnexpectedEof { .. })
        ));
    }

    #[test]
    fn corruption_in_the_payload_is_caught_on_read() {
        let (_dir, path) = scratch();
        let mut engine = StorageEngine::open(&path).unwrap();
        let id = blob(&mut engine, "the payload we will damage");
        engine.commit().unwrap();
        let offset = engine.index().offset_of(id).unwrap();
        drop(engine);

        damage(&path, offset + RECORD_HEADER_SIZE + 4, b"X");

        let engine = StorageEngine::open(&path).unwrap();
        assert!(matches!(
            engine.read(id),
            Err(CndbError::RecordChecksum { .. })
        ));
        assert!(matches!(
            engine.verify(),
            Err(CndbError::RecordChecksum { .. })
        ));
    }

    #[test]
    fn tombstones_hide_documents_and_survive_a_reopen() {
        let (_dir, path) = scratch();
        let (kept, gone) = {
            let mut engine = StorageEngine::open(&path).unwrap();
            let kept = blob(&mut engine, "keep me");
            let gone = blob(&mut engine, "delete me");
            engine.tombstone(gone).unwrap();
            engine.commit().unwrap();

            assert_eq!(engine.len(), 1);
            assert!(matches!(engine.read(gone), Err(CndbError::Deleted(_))));
            (kept, gone)
        };

        let engine = StorageEngine::open(&path).unwrap();
        assert_eq!(engine.len(), 1);
        assert_eq!(engine.read(kept).unwrap(), b"keep me");
        assert!(matches!(engine.read(gone), Err(CndbError::Deleted(_))));
    }

    #[test]
    fn deleting_twice_or_deleting_nothing_is_an_error() {
        let (_dir, path) = scratch();
        let mut engine = StorageEngine::open(&path).unwrap();
        let id = blob(&mut engine, "x");

        engine.tombstone(id).unwrap();
        assert!(matches!(engine.tombstone(id), Err(CndbError::Deleted(_))));
        assert!(matches!(
            engine.tombstone(DocId(9999)),
            Err(CndbError::NotFound(_))
        ));
    }

    #[test]
    fn the_master_index_does_not_disturb_the_record_stream() {
        let (_dir, path) = scratch();
        let mut engine = StorageEngine::open(&path).unwrap();

        // Commit repeatedly so several superseded master records end up sitting
        // between the document records.
        let mut ids = Vec::new();
        for i in 0..5 {
            ids.push(blob(&mut engine, &format!("round-{i}")));
            engine.commit().unwrap();
        }

        // 5 documents + 5 master records, all framed and checksummed.
        assert_eq!(engine.verify().unwrap(), 10);
        for (i, id) in ids.iter().enumerate() {
            assert_eq!(engine.read(*id).unwrap(), format!("round-{i}").as_bytes());
        }

        let engine = StorageEngine::open(&path).unwrap();
        assert_eq!(engine.generation(), 5);
        assert_eq!(engine.len(), 5);
    }

    #[test]
    fn every_record_kind_roundtrips() {
        let (_dir, path) = scratch();
        let mut engine = StorageEngine::open(&path).unwrap();

        let node = engine.append(RecordKind::Node, b"a node").unwrap();
        let edge = engine.append(RecordKind::Edge, b"an edge").unwrap();
        let empty = engine.append(RecordKind::Blob, b"").unwrap();
        engine.commit().unwrap();

        let engine = StorageEngine::open(&path).unwrap();
        assert_eq!(engine.read_record(node).unwrap().kind, RecordKind::Node);
        assert_eq!(engine.read_record(edge).unwrap().kind, RecordKind::Edge);
        assert_eq!(engine.read(empty).unwrap(), b"");
    }

    #[test]
    fn large_payloads_roundtrip() {
        let (_dir, path) = scratch();
        let mut engine = StorageEngine::open(&path).unwrap();

        let big: Vec<u8> = (0..4 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
        let id = engine.append(RecordKind::Blob, &big).unwrap();
        engine.commit().unwrap();

        let engine = StorageEngine::open(&path).unwrap();
        assert_eq!(engine.read(id).unwrap(), big);
    }

    #[test]
    fn oversized_payloads_are_refused() {
        let (_dir, path) = scratch();
        let mut engine = StorageEngine::open(&path).unwrap();
        let too_big = vec![0u8; (MAX_RECORD_LEN + 1) as usize];
        assert!(matches!(
            engine.append(RecordKind::Blob, &too_big),
            Err(CndbError::RecordTooLarge { .. })
        ));
    }

    #[test]
    fn many_commits_keep_the_database_consistent() {
        let (_dir, path) = scratch();
        let mut ids = Vec::new();

        for round in 0..25 {
            let mut engine = StorageEngine::open(&path).unwrap();
            ids.push(blob(&mut engine, &format!("round-{round}")));
            if round % 4 == 0 {
                engine.tombstone(ids[0]).ok();
            }
            engine.commit().unwrap();
            engine.verify().unwrap();
        }

        let engine = StorageEngine::open(&path).unwrap();
        assert_eq!(engine.generation(), 25);
        assert_eq!(engine.len(), 24);
        assert_eq!(engine.live_ids().count(), 24);
    }
}
