# Project Memory

> **Humans:** you can skip this file. The lab CLI, web console, and RGB demos do **not** require Redis.  
> **Agents / AI:** this is the full contract for the optional discovery cache. Also follow [M2M.md](./M2M.md) §3 and [AGENTS.md](../AGENTS.md).

Project Memory v2 is an optional local source-discovery index. Repository files are always the authoritative source of truth. Redis is a deterministic, derived, disposable retrieval cache; a hit is only a path and line-range pointer, and the current file must be opened before any claim or edit.

## CLI contract

Agents use the portable root `project-memory.py`; the raw Redis representation is private, opaque, and unstable. `scripts/project_memory.py` remains a compatibility wrapper.

```bash
python3 project-memory.py index --incremental
python3 project-memory.py index --incremental --repair-deep
python3 project-memory.py --version
python3 project-memory.py status
python3 project-memory.py validate
python3 project-memory.py validate --deep
python3 project-memory.py search "health readiness boundary" --limit 5
python3 project-memory.py symbols "qualified.name" --limit 20
python3 project-memory.py impact "symbol_name" --limit 20
python3 project-memory.py path "source.qualified.name" "target.qualified.name" --edge-kind calls
python3 project-memory.py evaluate --limit 10
python3 project-memory.py clear
```

All output is JSON.

| Command | Success behavior | Exit codes |
|---------|------------------|------------|
| `status` | Prints machine-parseable manifest and freshness data | `0` fresh; `2` missing/stale/invalid; `1` connection/protocol/command error |
| `index` | Atomically activates a staged generation; `--repair-deep` regenerates owners of semantically invalid reused chunks | `0` success; `1` error |
| `validate` | Validates the active manifest and current corpus; `--deep` also verifies every chunk/vector | `0` fresh; `2` invalid/stale; `1` error |
| `search QUERY [--limit N]` | Ranked path, line range, score, and text pointers | `0` success; `1` error (rejects missing/stale by default) |
| `symbols QUERY [--limit N]` | Symbol definition pointers with parser confidence | `0` success; `1` error |
| `impact QUERY [--limit N]` | Distinct incoming typed edges with resolution provenance | `0` success; `1` error |
| `path SOURCE TARGET [options]` | Read-only minimum-hop dependency path with source pointers | `0` success; `1` error |
| `evaluate [--limit N]` | Configured retrieval recall benchmark | `0` all pass; `2` misses; `1` error |
| `clear` | Deletes only this namespace's recorded keys | `0` success; `1` error |

`clear` never runs `FLUSHDB` or `FLUSHALL`. Connection and protocol failures are reported clearly with `cache_consulted: false`.
Indexing and namespace clearing share the same ownership lock and cannot run concurrently.

## Connection configuration

- Default endpoint: `redis://127.0.0.1:6379/0` (no authentication).
- Override: `--url redis://host:port/db`, `PROJECT_MEMORY_URL`, or legacy `RGBMVP_PROJECT_MEMORY_URL`.
- Authentication, TLS, query parameters, and non-`redis` schemes are intentionally unsupported.
- If Redis is unavailable, continue from repository files and explicitly report that the optional cache was not consulted or refreshed.

## Namespace, schema, and freshness

The lowercase project directory name produces the isolated namespace:

```text
rgbmvp:project-memory:v2:*
```

Schema id: `project-memory:v2`. Embedding id: `feature-hash-sha256-unigram-bigram-v1` (384 dimensions).

Each generation manifest records bundle version, schema, namespace, generation, embedding identifier,
dimensions, the exact ordered file list, per-file hashes and chunk maps, chunk count, chunk keys, and a
SHA-256 corpus fingerprint computed from every included relative path and its exact bytes. Any
indexed-file byte change makes the active generation stale.

Source is divided into deterministic 80-line chunks with 16 lines of overlap. Retrieval uses deterministic, locally computed feature-hashed unigrams and bigrams (SHA-256, signed accumulation, L2 normalization) with cosine ranking and a small exact-token lexical component. It uses only Python's standard library: no model download, external embedding API, `redis-py`, NumPy, Redis Stack, RediSearch, RedisJSON, or `redis-cli`.

Project Memory v2.1 records the chunk keys owned by each file. Re-indexing hashes the admitted corpus,
then parses, chunks, and embeds only new, changed, or damaged-cache files. Unchanged files reuse their
recorded chunk keys. v2.1.2 bounds batched `MGET` calls, gives every build a unique staging key, and
uses a namespaced renewable Redis ownership lock with token-fenced activation. Every attempt carries
the existing registry forward, so a successful retry collects leftovers from interrupted writes or
garbage collection before reducing the registry to the active generation. A final repository
fingerprint check prevents activation if source files changed during the build. Operational
file/chunk/timing metrics are returned beside the immutable manifest.

`status` is intentionally lightweight: it checks manifest structure and repository freshness without
loading chunk payloads. `validate --deep` additionally loads every active chunk and recomputes its
owner, content-derived identifier, tokens, and embedding vector while checking line bounds. Unknown
schema, malformed or semantically inconsistent data, decoding errors, or missing chunks are cache
misses requiring re-indexing.

Normal incremental indexing performs inexpensive structural checks on reused chunks. After a deep
validation failure, `index --incremental --repair-deep` performs semantic checks and regenerates only
the files owning invalid chunks. Duplicate chunk keys are invalid manifest metadata.

## v2.2 syntax and code graph

Each file owns a deterministic graph record containing module-qualified symbols, typed edges,
parser provenance, extraction confidence, resolution provenance, and diagnostics. Unchanged files
reuse their graph records; only new and changed files
are re-extracted. Deep validation recomputes the graph from current source in addition to validating
chunks.

- Python uses the standard-library AST. Definitions, imports, calls, inheritance, decorators, and
  decorator-call forms are marked `python-ast` with authoritative extraction confidence. Module
  prefixes, explicit aliases, unaliased dotted imports, lexical binding scopes, and package-relative
  imports are retained. Resolution confidence is reported separately.
- Rust uses `rust-syntax-v1`, a deterministic lightweight syntax extractor for definitions, `use`
  statements, and call-shaped expressions. Its records are always marked `heuristic`, and its
  diagnostic explicitly says it is not an AST parser.
- Other admitted text remains searchable by chunks and has an empty `parser: none` graph record.

`search` includes score-component explanations and exact symbol overlap. `symbols` provides
definition pointers. `impact` reports uniquely resolved incoming edges plus exact target-name
matches; ambiguous names remain labeled as name matches instead of being presented as resolved.
Results are deduplicated by source symbol. `.project-memory.json` may define `evaluation_queries` with
`mode`, `query`, and `expected_paths` to measure recall at a chosen result limit.

Resolution status is one of `exact_qualified`, `lexical_scope`, `import_binding`,
`heuristic_unique_short_name`, `ambiguous`, or `unresolved`. Only the first three carry `strong`
confidence. The manifest and indexing metrics report counts for every status; a unique short-name
link remains `probable`, never authoritative. Attribute calls without an explicit binding remain
unresolved rather than being guessed from a globally unique method name.

v2.2 embedded per-file graph records in the active manifest. That kept activation transactional but
made metadata reads proportional to graph size.

The rgbmvp RC measurement at 623 symbols and 6,168 edges was 2,102,555 manifest bytes, of which
97.1% was the embedded graph. That measurement motivated the v2.3 storage migration before broad
replication.
The configured repository benchmark contains 25 definition, impact, and semantic-search cases;
recall is a regression signal, not proof of general resolution accuracy.

## v2.3 external graph storage

v2.3 stores deterministic extraction graphs as content-addressed Redis records keyed by extractor
schema, record schema, relative owner path, and file content hash. Path ownership is part of the
identity because module-qualified names and symbol ids are path-dependent. The active manifest
contains only the per-file graph references plus symbol, edge, and resolution summaries.
Corpus-dependent cross-file resolution is derived after records are loaded; it is never written back
into a shared content-addressed record.

- `status` validates compact manifest references without loading graph records.
- `search` and `symbols` load extraction graphs only when symbol data is requested.
- `impact` loads the records and derives cross-file resolution in memory.
- `validate --deep` loads every graph, validates record ownership and schema, recomputes extraction
  from current source, and verifies the manifest summaries after resolution.
- `index --incremental --repair-deep` semantically compares reusable graphs with current-source
  extraction and rewrites only corrupted owner records; graph-only repair does not regenerate chunks.
- Incremental indexing reuses unchanged graph keys, writes only missing or invalid records, and
  registers staged graph keys before writes.
- Activation remains a single fenced manifest-pointer update. Obsolete graph records and manifests
  are collected only after activation; an interrupted cleanup leaves their exact keys in the scoped
  registry for the next index run.
- Embedded v2.2 graphs migrate automatically to external v2.3 records without regenerating unchanged
  chunk embeddings.

The key shape is an implementation detail: `<namespace>:graph:<identity>`, where `identity` hashes
the graph schema, record schema, relative path, and file hash. Never scan or delete graph keys by
wildcard; `clear` and garbage collection operate only on exact keys recorded by this namespace.

The initial rgbmvp v2.3 migration produced a compact 80,434-byte manifest and 100 external graph
records totaling 2,044,322 bytes. Compared with the 2,102,555-byte v2.2 embedded manifest, manifest
transfer fell by 96.17% while preserving the same retrieval contract.

The graph remains a disposable discovery aid. It does not replace compiler name resolution, type
checking, or opening the returned current source file.

## v2.4 dependency paths

v2.4 keeps the cache, graph, graph-record, and namespace schemas unchanged while adding a read-only
`path` query. Deterministic breadth-first search finds a minimum-hop route across resolved `calls`,
`decorated_by`, `imports`, and `inherits` edges. Strong resolutions are traversed by default;
`--include-probable` explicitly permits heuristic unique-short-name links. Ambiguous and unresolved
edges are never traversed.

Use `--direction reverse` for caller/importer routes, repeat `--edge-kind` to restrict traversal,
and use `--max-depth` to bound exploration. Results contain ordered symbol and edge pointers, but
remain discovery aids that must be checked against current repository source. v2.4 also adds exact
repository-relative `exclude_paths`, `--version`, package `__version__`, and a boundary-safe strict
UTF-8 probe. Activating the v2.4 manifest requires an incremental index; unchanged chunks and graph
records remain reusable.

**Raw Redis layout, hash fields, vector encoding, and stored text formatting are private implementation details, not a stable API.** Agents must not depend on them.

## Corpus and privacy

Included content is deliberately source-oriented:

- root `README.md`, `AGENTS.md`, `pyproject.toml`, `.gitignore`, and non-secret `.env.example`;
- Markdown in `docs/`;
- Python application source and tests;
- Rust workspace crates and project-owned programs;
- web HTML/JavaScript/CSS and deployment/container configuration;
- CI workflow YAML under `.github/workflows/` when present;
- agent instruction markdown under `.agents/`, `.claude/`, or `.codex/` when present;
- other `scripts/**/*.py` and `scripts/**/*.sh` (except the memory tool itself).

Excluded content includes:

- generated builds, dependencies, virtual environments, package caches;
- binaries, archives, editor state, logs, coverage, temporary files;
- environment files with secrets (`.env`), credentials, private keys, tokens, passkeys;
- production/customer data, personal data, operational payloads;
- databases and local `data/` trees;
- symlinks and files larger than 1 MB;
- the portable memory implementation and compatibility wrapper themselves.

Portable bundle v2.0.1 makes sensitive filename/path and private-key suffix
filters mandatory and non-overridable. Known text-source types must also pass a
UTF-8/binary probe, so broad test/schema globs cannot admit images, databases,
archives, or invalid text payloads.

v2.1.2 also rejects generic token, refresh/auth/OAuth token, and client-secret
filenames when they use data/configuration suffixes, without excluding legitimate
source modules such as `token.py`.

Never cache credentials, tokens, passkeys, device keys, personal data, production payloads, or uncommitted content copied from external systems. Never write application/runtime state into the project-memory namespace.

Inspect actual coverage after indexing:

```bash
python3 project-memory.py status | python3 -m json.tool
```

Confirm the manifest includes representative `src/`, `tests/`, `docs/`, configuration, and agent files, and does not include secrets, data dumps, or the memory tool.

## Machine workflow

1. Run `status` before broad exploration; run `index` when absent or stale.
2. Issue two or three focused intent queries (component, behavior, boundary, protocol, or failure terms) instead of dumping the corpus.
3. Open every returned **current** source location before relying on it; cite the file, never Redis.
4. Make and validate changes with the repository's normal checks (`pytest -q`, syntax checks).
5. Re-index after indexed-file edits and require a final fresh `status`.

Redis may be shared across projects. Never use `FLUSHDB`, `FLUSHALL`, wildcard deletion outside this namespace, or raw-key automation. A failed Redis operation does not prevent direct repository inspection.

## Operator examples

```bash
# Build / refresh
python3 project-memory.py index --incremental

# Freshness check (exit 0 only when fresh)
python3 project-memory.py status; echo exit:$?

# Focused discovery
python3 project-memory.py search "readiness health config boundary" --limit 5
python3 project-memory.py search "project memory namespace fingerprint" --limit 5

# Remove only this project's keys
python3 project-memory.py clear

# Custom endpoint
python3 project-memory.py --url redis://127.0.0.1:6379/0 status
PROJECT_MEMORY_URL=redis://127.0.0.1:6379/0 python3 project-memory.py index --incremental
```

## Failures

| Situation | Behavior |
|-----------|----------|
| Redis down / refused | Exit `1`, JSON error on stderr, `cache_consulted: false` |
| Bad URL / auth present | Exit `1`, clear validation error |
| Missing or stale index | `status` exit `2`; `search` fails with re-index instruction |
| Unknown schema / malformed chunks | Treated as cache miss / invalid; re-index required |
| Shared Redis | Only this namespace's recorded keys are written or deleted |

## Unstable representation

The bytes stored under `rgbmvp:project-memory:v2:*` may change without notice within a future schema revision. Do not document, scrape, or hard-code key names, vector formats, or chunk payloads outside this tool.

## Portable bundle

The reusable unit is `project-memory.py`, `project_memory/`, and `.project-memory.json`. The package discovers the repository root from its own location and has no dependency on rgbmvp runtime code or installation. The configuration preserves this repository's corpus and privacy boundaries; when copying elsewhere, set a unique `project_slug` and review its include patterns and additive exclusions.
