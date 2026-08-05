# cndb — ContextDB

A single-file, embedded **graph knowledge base for codebases**.

cndb reads a repository, extracts its structure into a graph of symbols and
relationships, and stores it in one portable `.cndb` file. Coding agents and
applications query that graph to answer structural questions about code —
*who calls this*, *what breaks if I change it*, *what do I need to read to
understand this subsystem* — without crawling the filesystem.

cndb is a storage engine in its own right, an alternative to SQLite rather than
a layer on top of one. No server, no daemon, no network protocol, no third-party
database underneath.

## Design

- **One file.** The entire graph lives in a single `.cndb` file you can copy
  between machines.
- **Local-first.** Runs in-process with zero runtime dependencies. Source code
  never leaves the machine.
- **Deterministic.** Structure is extracted with tree-sitter, so the same commit
  produces the same graph on every machine. Heuristically resolved edges are
  tagged as such rather than reported as fact.
- **No MCP.** Applications get real in-process function calls. Agents shell out
  to one static binary that prints JSON — no server to register, no port, no
  editor restart.

## Planned interface

```bash
cndb init                      # build the graph for this repo
cndb sync                      # incremental re-index of changed files
cndb explore "auth flow"       # relevant symbols, with call paths and snippets
cndb callers parse_header      # who calls this
cndb impact parse_header       # blast radius of a change
cndb path handle_request write_record
```

```rust
let db = Cndb::open(".cndb/graph.cndb")?;
let callers = db.callers("cndb::storage::append_record", Depth(2))?;
```

## Status

Early development. See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the
full design, the graph and file-format specifications, and the roadmap.

## License

Apache-2.0. See [LICENSE](LICENSE).
