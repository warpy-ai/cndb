# cndb — Architecture

**Version:** 0.3.0
**Status:** Draft
**Supersedes:** the v0.2.0 technical design in issue #1

---

## 1. What cndb is

cndb is a **single-file, embedded graph knowledge base for codebases**.

It reads a repository, extracts its structure into a graph of symbols and
relationships, and stores that graph in one portable `.cndb` file. Coding
agents and applications query it to answer structural questions about code —
*who calls this*, *what breaks if I change it*, *what do I need to read to
understand this subsystem* — without crawling the filesystem.

cndb is a storage engine in its own right. It is an alternative to SQLite, not
a layer on top of one. There is no server, no daemon, no network protocol, and
no third-party database underneath it.

### 1.1 What changed from v0.2.0

v0.2.0 described a hybrid **document + vector** database validated against an
e-commerce workload (order history, 10M synthetic orders, concurrent HTTP load).
v0.3.0 keeps the storage thesis and replaces the data model and the workload.

| | v0.2.0 | v0.3.0 |
|---|---|---|
| Primary model | BSON documents | Graph of nodes + edges (stored as documents) |
| Primary retrieval | Vector similarity (HNSW) | Graph traversal + full-text |
| Target workload | E-commerce OLTP, 100 concurrent users | Code intelligence, single local process |
| Primary consumer | Application code | Coding agents and application code |
| Vectors | Core, day one | Deferred to v1, behind a feature flag |

Unchanged, and still the point of the project:

- Everything in one `.cndb` file you can copy between machines
- Local-first, in-process, zero runtime dependencies
- Append-only log with an atomically committed index block
- Custom binary format, no external database

### 1.2 Non-goals for v0

- Distributed or networked operation
- Multi-writer concurrency (single writer, multiple readers)
- Vector search and embeddings (v1)
- LLM-driven extraction (v1 — see §5.4)
- MCP server (deliberately never — see §6)

---

## 2. The three engines

```
        ┌────────────────────────────────────────────────┐
        │  cndb-query   — the bridge to LLMs / callers   │
        │                                                │
        │  explore · callers · callees · impact · path   │
        │  Rust API  ·  CLI (JSON on stdout)  ·  FFI     │
        └───────────────────────┬────────────────────────┘
                                │  in-process function calls
        ┌───────────────────────▼────────────────────────┐
        │  cndb-graph   — the graph knowledge base       │
        │                                                │
        │  node/edge model · adjacency index · FTS index │
        │  document collections · offset map · filters   │
        └───────────────────────┬────────────────────────┘
                                │
        ┌───────────────────────▼────────────────────────┐
        │  cndb-store   — the storage engine             │
        │                                                │
        │  single file · append-only log · master index  │
        │  atomic commit · compaction · crash recovery   │
        └───────────────────────┬────────────────────────┘
                                │
                        ┌───────▼────────┐
                        │  graph.cndb    │
                        └────────────────┘

        ┌────────────────────────────────────────────────┐
        │  cndb-extract — reads and understands code     │
        │                                                │
        │  file walk · tree-sitter parse · resolution    │
        │  writes nodes + edges ─────────────────────────┼──▶ cndb-graph
        └────────────────────────────────────────────────┘
```

`cndb-extract` is a **producer**: it runs at `init`/`sync` time and writes into
the graph. `cndb-query` is a **consumer**: it reads and never writes. They are
separate binaries' worth of concerns but ship in one binary.

---

## 3. Storage engine (`cndb-store`)

### 3.1 File layout

```
┌──────────────────────────────────────────────────────────┐
│ HEADER SLOT A  (64 bytes)                                │
│ HEADER SLOT B  (64 bytes)                                │
│   magic       [u8; 4]  "CNDB"                            │
│   version     u32      0x0003_0000                       │
│   generation  u64      monotonic commit counter          │
│   master_ptr  u64      byte offset of master index block  │
│   master_len  u64      length of master index block       │
│   log_end     u64      first free byte in the record log  │
│   crc32       u32      checksum over the preceding fields │
│   reserved    [u8; 24]                                    │
├──────────────────────────────────────────────────────────┤
│ RECORD LOG  (append-only, grows forward)                 │
│   [len u32][kind u8][crc32 u32][BSON payload …]          │
│   [len u32][kind u8][crc32 u32][BSON payload …]          │
│   …                                                      │
│   kind: 0 = Node, 1 = Edge, 2 = Blob, 3 = Tombstone      │
├──────────────────────────────────────────────────────────┤
│ MASTER INDEX BLOCK  (bincode, rewritten on each commit)  │
│   see §3.3                                               │
└──────────────────────────────────────────────────────────┘
```

### 3.2 Atomic commit via header ping-pong

v0.2.0 specified overwriting a single header in place. A 64-byte write inside
one sector is atomic on real hardware, but that is a hardware assumption rather
than a guarantee. v0.3.0 uses two header slots instead:

1. Append all new records to the log. `fsync`.
2. Append a fresh master index block after them. `fsync`.
3. Write a header with `generation + 1` into the **older** of the two slots.
   `fsync`.

On open, read both slots, discard any with a bad magic or CRC, and take the
survivor with the highest `generation`. A crash at any point leaves the previous
generation's header fully intact and the partially written bytes beyond
`log_end` are simply overwritten by the next append. No write-ahead log needed,
and no torn-header failure mode.

### 3.3 Master index block

One bincode-serialized struct holding every in-memory index:

```rust
pub struct MasterIndex {
    /// DocId -> byte offset in the record log
    offsets:  HashMap<DocId, u64>,
    /// Outgoing adjacency: NodeId -> edges leaving it
    out_adj:  HashMap<NodeId, Vec<EdgeRef>>,
    /// Incoming adjacency: NodeId -> edges arriving at it
    in_adj:   HashMap<NodeId, Vec<EdgeRef>>,
    /// Fully-qualified symbol name -> candidate nodes
    by_name:  HashMap<String, Vec<NodeId>>,
    /// FileId -> every doc extracted from that file (for incremental sync)
    by_file:  HashMap<FileId, Vec<DocId>>,
    /// Inverted index over names, signatures and doc comments
    fts:      InvertedIndex,
    /// Tombstoned doc ids, excluded from all reads
    dead:     HashSet<DocId>,
    meta:     DbMeta,
}

pub struct EdgeRef {
    kind:    EdgeKind,
    other:   NodeId,
    edge_id: DocId,
}
```

`EdgeRef` vectors are kept sorted by `kind`, so a kind-filtered neighbor lookup
is a binary search plus a contiguous slice — the hot path for every traversal.

Storing adjacency in the index rather than scanning edge documents is the single
decision that makes graph traversal viable on this format. A `callers` query
touches the index and then reads only the documents it actually returns.

### 3.4 Deletes and compaction

The log is append-only, so updates and deletes are tombstones. `sync` on a
changed file tombstones everything in `by_file[file_id]` and appends fresh
records. When `dead.len() / total` crosses a threshold (default 0.3), compaction
rewrites live records into a sibling file and renames it into place.

---

## 4. Graph model (`cndb-graph`)

Nodes and edges are documents in reserved collections. The graph layer is a
typed view over the document engine, not a separate storage path.

### 4.1 Node kinds

`File` · `Module` · `Function` · `Method` · `Class` · `Struct` · `Interface`
(trait/protocol) · `Enum` · `TypeAlias` · `Constant` · `Test`

```rust
pub struct Node {
    id:             NodeId,
    kind:           NodeKind,
    name:           String,        // `parse_header`
    qualified_name: String,        // `cndb::storage::file_format::parse_header`
    language:       Language,
    file:           FileId,
    span:           Span,          // byte + line range, for snippet extraction
    signature:      Option<String>,
    doc:            Option<String>,
    hash:           u64,           // content hash, for change detection
}
```

### 4.2 Edge kinds

| Kind | Meaning |
|---|---|
| `CONTAINS` | file → symbol, class → method, module → module |
| `CALLS` | function → function it invokes |
| `IMPORTS` | file/module → module it imports |
| `IMPLEMENTS` | type → interface/trait it implements |
| `EXTENDS` | type → supertype |
| `REFERENCES` | any symbol → any symbol mentioned but not called |
| `TESTS` | test → symbol under test |

```rust
pub struct Edge {
    id:         EdgeId,
    kind:       EdgeKind,
    from:       NodeId,
    to:         NodeId,
    confidence: Confidence,   // Extracted | Inferred(f32)
    span:       Option<Span>, // the call site itself
}
```

### 4.3 Confidence is not optional

Tree-sitter tells you *"there is a call to the identifier `parse` at this
byte range."* It does not tell you *which* `parse`. Resolution is a separate
pass (§5.3) and it is frequently ambiguous — dynamic dispatch, duck typing,
re-exports, generics.

Every edge therefore carries provenance:

- `Extracted` — syntactically unambiguous. A direct call to a name with exactly
  one visible definition; an import with a resolvable path.
- `Inferred(score)` — resolved heuristically among several candidates.

Query results expose this, and callers can filter on it. Reporting a guessed
call edge as fact is how a code intelligence tool loses trust, and it is why
this field exists in v0 rather than being retrofitted alongside LLM extraction
in v1.

---

## 5. Extraction engine (`cndb-extract`)

### 5.1 Deterministic by design

v0 extraction is **tree-sitter only**. No LLM is involved in building the graph.

This is a deliberate constraint, not a limitation to be lifted at the first
opportunity:

- **Reproducible.** The same commit produces a byte-identical graph on every
  machine. `impact` results are only trustworthy if they are stable.
- **Fast.** Parsing is milliseconds per file, which is what makes save-triggered
  incremental sync possible at all.
- **Free.** No API keys, no per-index cost, no network. Indexing a large
  monorepo costs nothing.
- **Private.** Source code never leaves the machine.

### 5.2 Pipeline

```
walk repo (respect .gitignore)
  └─▶ detect language by extension
       └─▶ parse with tree-sitter grammar
            └─▶ run per-language queries → local symbols + raw references
                 └─▶ global resolution pass
                      └─▶ append Node/Edge records, rebuild indexes, commit
```

Languages for v0: **Rust, TypeScript, JavaScript, Python, Go**. Grammars are
compiled into the binary, so there is no runtime grammar download and no
per-machine variation.

### 5.3 Resolution pass

Parsing is per-file and embarrassingly parallel. Resolution is global and runs
once all files are parsed:

1. Build the qualified-name index from every definition found.
2. Resolve each file's imports to concrete modules, producing `IMPORTS` edges.
3. For each raw reference, compute the candidate set from the referencing file's
   visible scope: locals → file scope → imported names → crate/package root.
4. Exactly one candidate → `CALLS`/`REFERENCES` edge, `Extracted`.
   Several candidates → `Inferred(1/n)`, dropped below a floor (default 0.2).
   Zero candidates → recorded as an external/unresolved reference, not an edge.

### 5.4 Where the LLM fits (v1, not v0)

The interpretation layer is additive and sits strictly on top of the facts
layer. It never rewrites extracted structure — it adds `Concept` nodes,
subsystem summaries, and cross-language edges that no parser can see (a handler
named in a YAML config, a route string matched to a function).

The intended provider is whatever coding agent is **already installed on the
user's machine**, invoked headlessly:

```
~/.claude/  → claude -p …          ~/.codex/ → codex exec …
~/.gemini/  → gemini -p …          ollama    → fully local
```

Behind an `LlmProvider` trait, with a direct-API-key backend as the fallback.
The user already pays for their agent subscription, so enrichment costs them
nothing extra and requires no key management. This is why agent detection is
worth building — but it enriches a graph that is already complete and correct
without it.

---

## 6. Query engine (`cndb-query`)

### 6.1 Two consumers, two surfaces, no MCP

**Library consumers** — applications, and the Bun/Node bindings tracked in
issue #3 — get genuine in-process function calls. The SQLite model:

```rust
let db = Cndb::open(".cndb/graph.cndb")?;
let callers = db.callers("cndb::storage::append_record", Depth(2))?;
```

**Agent consumers** — Claude Code, Cursor, Codex — shell out to a single static
binary that writes JSON to stdout. No daemon, no port, no protocol handshake,
no config file to edit, no editor restart.

MCP is the thing we are choosing not to be. It requires a server process, a
registration step, and a client that speaks the protocol. A CLI that prints JSON
works in every agent that can run a command, which is all of them.

### 6.2 Operations

| Command | Semantics |
|---|---|
| `explore <query>` | FTS seeds, ranked by relevance × degree centrality, expanded 1–2 hops, returned with source snippets |
| `callers <symbol>` | reverse traversal of `CALLS`, bounded depth |
| `callees <symbol>` | forward traversal of `CALLS`, bounded depth |
| `impact <symbol>` | transitive reverse reachability over `CALLS` ∪ `REFERENCES`; returns affected symbols, files, and tests |
| `path <a> <b>` | bidirectional BFS between two symbols |
| `context <symbol>` | the minimal read-set to understand a symbol: definition, callees, types, tests |
| `stats` | node/edge counts by kind, languages, staleness |

Every command takes `--json`. Snippets are read from the source file by span at
query time, so the graph stores locations rather than duplicated source.

### 6.3 CLI shape

```
cndb init            # build the graph for the current repo → .cndb/graph.cndb
cndb sync            # incremental re-index of changed files
cndb explore "auth flow" --json
cndb impact parse_header --depth 3 --json
cndb callers Cndb::open
cndb path handle_request write_record
cndb stats
```

---

## 7. Roadmap

| Milestone | Scope | Existing issues |
|---|---|---|
| **M1 — Storage core** | header ping-pong, record log, offset map, BSON codec, commit/recover | #5, #6, #7, #8, #14 |
| **M2 — Graph model** | node/edge types, adjacency + name indexes, filters, traversal primitives | #9, #15 (reframed) |
| **M3 — Extraction** | file walk, tree-sitter for 5 languages, resolution pass, confidence | new |
| **M4 — Query engine** | FTS index, explore/callers/callees/impact/path/context | new |
| **M5 — CLI + sync** | binary, JSON output, incremental sync, compaction | new |
| **M6 — Bindings** | Bun/Node FFI, C-compatible surface | #3 |
| **v1 — Enrichment** | agent detection, `LlmProvider`, concept nodes, summaries | new |
| **v1 — Vectors** | fastembed/ONNX embeddings, ANN index, hybrid ranking | #10–#13, #16 (deferred) |

### 7.1 Dependency changes

`rust-bert` is removed. It pulls `torch-sys`, whose build script downloads
libtorch — a multi-gigabyte native dependency that **currently fails to build,
leaving CI red on `main`**. It also makes a single portable binary impossible,
which contradicts the project's core portability goal. When vectors land in v1,
the replacement is `fastembed`/ONNX Runtime or Candle, both of which link
statically.

`hnsw` is removed alongside it and returns with v1 vectors.

### 7.2 Success criteria

The v0.2.0 criteria measured an e-commerce OLTP workload and no longer describe
anything cndb does. Replacements, measured against real open-source repositories
rather than synthetic data:

| # | Criterion | Target |
|---|---|---|
| C1 | Cold index, 10k-file repo | < 60 s |
| C2 | Incremental sync, one changed file | < 200 ms |
| C3 | Cold start (open + load index) | < 200 ms |
| C4 | `callers` / `callees` | < 5 ms |
| C5 | `impact`, depth 3 | < 50 ms |
| C6 | `explore` end to end, with snippets | < 100 ms |
| C7 | Graph file size | < 20% of repo source size |
| C8 | Crash during commit | previous generation always recoverable |
| C9 | Portability | graph file copies across macOS/Linux/Windows |

C8 and C9 carry over from v0.2.0 (issues #22, #14) essentially unchanged — they
were always about the storage engine, and the storage engine survived the pivot.
