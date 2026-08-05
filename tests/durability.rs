//! End-to-end durability and portability tests.
//!
//! These cover the storage-engine half of the success criteria in
//! `docs/ARCHITECTURE.md` §7.2 — C8 (a crash during commit is always
//! recoverable) and C9 (the file copies between machines).

use cndb::{Cndb, CndbError, DocId, StorageEngine, FORMAT_VERSION};
use serde_json::json;
use tempfile::TempDir;

/// Build a database with several committed generations and some deletions.
/// Returns the ids expected to be readable afterwards.
fn populate(path: &std::path::Path) -> Vec<DocId> {
    let mut db = Cndb::open(path).unwrap();
    let mut live = Vec::new();

    for round in 0..6 {
        for i in 0..4 {
            let id = db
                .insert(&json!({
                    "round": round,
                    "kind": "Function",
                    "qualified_name": format!("cndb::round{round}::symbol_{i}"),
                    "span": { "start_line": round * 10 + i, "end_line": round * 10 + i + 5 },
                }))
                .unwrap();
            live.push(id);
        }
        // Delete one document from an earlier round, so tombstones are part of
        // the committed state rather than an untested edge case.
        if round > 0 {
            let victim = live.remove(0);
            db.delete(victim).unwrap();
        }
        db.commit().unwrap();
    }

    live
}

#[test]
fn c9_the_file_is_self_contained_and_portable() {
    let dir = TempDir::new().unwrap();
    let original = dir.path().join("graph.cndb");
    let live = populate(&original);

    // Copying the raw bytes is what moving between machines amounts to.
    let copy = dir.path().join("copied.cndb");
    std::fs::copy(&original, &copy).unwrap();

    let db = Cndb::open(&copy).unwrap();
    assert_eq!(db.len(), live.len());
    db.verify().unwrap();
    for id in &live {
        let doc = db.get(*id).unwrap();
        assert_eq!(doc["kind"], "Function");
        assert!(doc["qualified_name"]
            .as_str()
            .unwrap()
            .starts_with("cndb::"));
    }
}

#[test]
fn c8_a_crash_at_any_byte_never_yields_a_corrupt_database() {
    let dir = TempDir::new().unwrap();
    let source = dir.path().join("graph.cndb");
    populate(&source);

    let bytes = std::fs::read(&source).unwrap();
    let scratch = dir.path().join("prefix.cndb");

    // A crash leaves some prefix of the intended writes on disk. Every prefix
    // must either be rejected outright or open into a fully consistent
    // database. What must never happen is opening and then handing back
    // damaged data.
    let mut opened = 0;
    for len in 0..=bytes.len() {
        std::fs::write(&scratch, &bytes[..len]).unwrap();

        let db = match Cndb::open(&scratch) {
            Ok(db) => db,
            Err(_) => continue,
        };
        opened += 1;

        // Whatever it claims to hold must actually be readable and intact.
        db.verify()
            .unwrap_or_else(|e| panic!("prefix of {len} bytes opened but failed verify: {e}"));
        for id in db.ids() {
            db.get(id)
                .unwrap_or_else(|e| panic!("prefix of {len} bytes: {id} unreadable: {e}"));
        }
    }

    assert!(
        opened > 1,
        "expected several prefixes to be recoverable, only {opened} were"
    );
}

#[test]
fn a_zero_length_prefix_is_treated_as_a_new_database_not_a_corrupt_one() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("empty.cndb");
    std::fs::write(&path, b"").unwrap();

    let db = Cndb::open(&path).unwrap();
    assert!(db.is_empty());
    assert_eq!(db.stats().generation, 0);
}

#[test]
fn the_on_disk_layout_is_stable() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("layout.cndb");
    {
        let mut db = Cndb::open(&path).unwrap();
        db.insert(&json!({ "x": 1 })).unwrap();
        db.commit().unwrap();
    }

    let bytes = std::fs::read(&path).unwrap();

    // Guards against silent format drift: a change here must be a deliberate
    // version bump, not a side effect of refactoring.
    assert_eq!(&bytes[0..4], b"CNDB", "magic bytes moved");
    assert_eq!(
        u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
        FORMAT_VERSION,
        "version field moved or changed"
    );
    // Slot B carries its own magic once the first commit lands there.
    assert_eq!(&bytes[64..68], b"CNDB", "second header slot moved");
    assert!(
        bytes.len() > 128,
        "records must start after both header slots"
    );
}

#[test]
fn a_database_written_by_a_future_version_is_refused() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("future.cndb");
    {
        let mut db = Cndb::open(&path).unwrap();
        db.insert(&json!({ "x": 1 })).unwrap();
        db.commit().unwrap();
    }

    // Rewrite both version fields, re-checksumming so the version is what
    // fails rather than corruption.
    let mut bytes = std::fs::read(&path).unwrap();
    for slot in 0..2 {
        let base = slot * 64;
        bytes[base + 4..base + 8].copy_from_slice(&0x0009_0000u32.to_le_bytes());
        bytes[base + 40..base + 44].fill(0);
        let crc = crc32fast::hash(&bytes[base..base + 64]);
        bytes[base + 40..base + 44].copy_from_slice(&crc.to_le_bytes());
    }
    std::fs::write(&path, &bytes).unwrap();

    assert!(matches!(
        Cndb::open(&path),
        Err(CndbError::UnsupportedVersion { .. })
    ));
}

#[test]
fn reopening_repeatedly_does_not_grow_the_file() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("stable.cndb");
    populate(&path);

    let size = std::fs::metadata(&path).unwrap().len();
    for _ in 0..10 {
        let db = StorageEngine::open(&path).unwrap();
        db.verify().unwrap();
    }
    assert_eq!(
        std::fs::metadata(&path).unwrap().len(),
        size,
        "opening without writing must not touch the file"
    );
}

/// Abandoned writes are reclaimed *logically* — the next commit overwrites
/// them — but the engine never shortens the file, so its length stays at the
/// high-water mark. Physically returning that space is compaction's job (M5).
///
/// What matters here is that repeated crashes do not leak: the committed
/// region must stay small, and the file must not grow once per crash.
#[test]
fn abandoned_writes_are_overwritten_rather_than_leaked() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("reuse.cndb");

    let junk = json!({ "discard": "x".repeat(200) });

    // Five separate crash-and-recover cycles, each abandoning the same volume.
    let mut high_water = 0;
    for cycle in 0..5 {
        let mut db = Cndb::open(&path).unwrap();
        db.insert(&json!({ "cycle": cycle })).unwrap();
        db.commit().unwrap();

        for _ in 0..50 {
            db.insert(&junk).unwrap();
        }
        drop(db); // crash: flushed, never committed

        let size = std::fs::metadata(&path).unwrap().len();
        if cycle == 0 {
            high_water = size;
        }
    }

    let db = Cndb::open(&path).unwrap();
    db.verify().unwrap();
    assert_eq!(db.len(), 5, "one surviving document per cycle");

    // Each cycle reuses the previous cycle's abandoned bytes, so the file sits
    // near a single cycle's high-water mark rather than five times it.
    let final_size = std::fs::metadata(&path).unwrap().len();
    assert!(
        final_size < high_water * 2,
        "file grew to {final_size} against a one-cycle mark of {high_water}: \
         abandoned bytes are leaking, not being reused"
    );

    // The committed region is a small fraction of the physical file — the
    // abandoned tail is beyond it and is never read back.
    let committed = db.stats().committed_len;
    assert!(
        committed * 4 < final_size,
        "committed region {committed} should be far smaller than the file {final_size}"
    );
}
