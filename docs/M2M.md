# Machine-to-machine protocol (agents / AI assistants)

**Audience:** automated agents, coding assistants, CI bots, multi-agent pipelines.  
**Humans:** prefer [PURPOSE_AND_USAGE.md](./PURPOSE_AND_USAGE.md) and [README.md](../README.md).  
**Authority:** repository files on disk always beat caches, summaries, and prior chat turns.

This document is structured for **machine parsing**: goals, invariants, workflows, exit codes, and prohibited actions.

---

## 0. Identity

| Field | Value |
|-------|--------|
| `project_id` | `rgbmvp` |
| `product` | Public lab: RGB on Liquid Testnet + BTC testnet twins + labd `/v1` + CLI + static web |
| `networks_public` | `liquid-testnet`, `bitcoin-testnet` |
| `networks_ci` | Elements regtest (Simplicity), optional bitcoind regtest |
| `mainnet` | forbidden unless explicit human flag + review |
| `phase_status` | P0 done · P1 closed · P2 closed · P3 closed (see `docs/*_CLOSED.md`) |
| `next_strategy` | Protocol completeness on localhost/testnet first; public demo only after U4 (`docs/ROADMAP_NEXT.md`) |
| `next_protocol` | U4/U5 + proof-first UI implemented; optional public deploy still admin-gated; S3+C2+C4 closed |
| `public_hosting` | Blocked until U4 acceptance gate |
| `binary` | `cargo build -p lab-cli` → `./target/debug/rgbmvp` |
| `api_prefix` | `/v1` |
| `data_dir` | `.rgbmvp/` (gitignored runtime state) |

---

## 1. Goals (what success means)

### 1.1 Product goals

1. Demonstrate **RGB NIA** issue → transfer → client-side verify on **Liquid Testnet** (`rgb_ready`, seal + tapret + anchor valid).  
2. Demonstrate **atomic twin swap** BTC↔Liquid via dual HTLC (fund both → claim LQ → claim BTC; refunds after CSV).  
3. Demonstrate **P2 differentiators**: Simplicity seal covenants (C0/C1) and **BFA** full-history audit without oracle (C3) on regtest.  
4. Expose the same logic via **CLI and browser** over **shared `/v1` JSON**; browser holds **no seeds / no preimage**.

### 1.2 Non-goals (do not implement as “success”)

- Teleport one `rgb:` contract id across chains.  
- Call LWK `issue_asset` “RGB”.  
- Require Core Lightning for P0–P3.  
- Commit secrets; write mainnet defaults; `FLUSHDB`/`FLUSHALL` on Redis.  
- Duplicate RGB validation in JavaScript.

### 1.3 Invariants

```text
INVARIANT_1: Files in git (and local uncommitted work) > Redis project memory > chat memory
INVARIANT_2: Native Liquid asset ≠ RGB contract
INVARIANT_3: Cross-chain = atomic swap of twins, separate contract ids
INVARIANT_4: CLI and web share /v1 validation; UI is thin client
INVARIANT_5: .env and .rgbmvp never committed
INVARIANT_6: GET /v1/swap/* must not return preimage_hex (always redacted)
INVARIANT_7: Project memory is discovery-only; not asset or consignment storage
```

---

## 2. Map scenario IDs before coding

Before large changes, open:

1. `docs/ARCHITECTURE.md`  
2. `docs/SCENARIOS.md` (scenario IDs: `R*`, `S*`, `C*`, `U*`)  
3. `docs/STACK.md`  
4. Relevant `docs/*_CLOSED.md` if touching a closed phase  

Map every feature to a scenario id (e.g. `R4`, `S3`, `C0`, `U2`).

---

## 3. Project memory (Redis vector discovery cache)

### 3.1 Role (agents only — humans can ignore)

**Project Memory** is an **optional, disposable, namespaced retrieval index** so agents can find relevant **source paths and line ranges** faster.

| Property | Value |
|----------|--------|
| Authoritative truth | **Never** Redis — always open the file |
| Hit meaning | Pointer only: `path` + line range + score + snippet |
| Storage | Local Redis, default `redis://127.0.0.1:6379/0` |
| Tool | **Canonical:** `project-memory.py`; `scripts/project_memory.py` is compatibility-only |
| Namespace | `rgbmvp:project-memory:v2:*` (configured project slug) |
| Schema | `project-memory:v2` |
| Embedding | Deterministic feature-hash unigram+bigram, 384-d, cosine + lexical |
| Human requirement | **None** — Redis may be down; lab product does not depend on it |

### 3.2 CLI (machine contract)

All output is **JSON**. Prefer `python3` if `python` is missing.

```bash
python3 project-memory.py status
# exit 0 = fresh; exit 2 = missing/stale/invalid; exit 1 = error

python3 project-memory.py index --incremental    # atomic generation switch
python3 project-memory.py index --incremental --repair-deep  # semantic cache repair
python3 project-memory.py validate
python3 project-memory.py validate --deep    # full active chunk/vector verification
python3 project-memory.py search "QUERY" --limit 5
python3 project-memory.py symbols "SYMBOL" --limit 20
python3 project-memory.py impact "SYMBOL" --limit 20
python3 project-memory.py path "SOURCE" "TARGET" --edge-kind calls
python3 project-memory.py evaluate --limit 10
python3 project-memory.py clear    # this namespace only — never FLUSHDB
```

v2.4 keeps the v2 schema and v3 graph format unchanged, stores per-file extraction graphs as
content-addressed records, and adds read-only minimum-hop dependency paths. Path traversal uses
strong resolutions by default; repository files remain authoritative.

| Env / flag | Purpose |
|------------|---------|
| `PROJECT_MEMORY_URL` | Portable Redis URL override |
| `RGBMVP_PROJECT_MEMORY_URL` | Legacy compatible Redis URL override |
| `--url redis://host:port/db` | Same |

Unsupported: auth, TLS, non-`redis` schemes, raw Redis key protocols, Redis Stack modules.

### 3.3 Mandatory agent workflow (broad exploration)

```text
PROCEDURE explore_codebase:
  1. RUN status
  2. IF exit 2 OR stale: RUN index; RUN status (expect exit 0)
  3. IF exit 1 (Redis down): SET cache_consulted=false; CONTINUE from files; DISCLOSE to user
  4. RUN 2–3 focused search queries (component OR protocol OR failure mode)
  5. FOR each hit: OPEN file at returned lines; CITE file path in claims; NEVER cite Redis as authority
  6. AFTER edits to indexed files: RUN index; RUN status (expect exit 0)
  7. NEVER: FLUSHDB, FLUSHALL, other namespaces, secrets in index, depend on raw Redis encoding
```

### 3.4 Validation after code changes

```text
PROCEDURE validate:
  python3 -m compileall -q src scripts   # when Python changed
  pytest -q                              # when tests apply
  cargo build -p lab-cli                 # when Rust changed
  cargo test -p lab-core -p lab-rgb ...  # when relevant
  REBUILD project memory if indexed files changed
```

### 3.5 What is indexed vs excluded

**Indexed (source-oriented):** `README.md`, `AGENTS.md`, `docs/**/*.md`, `src/**`, `tests/**`, selected `scripts/**`, etc.  
**Excluded:** `.env`, `.rgbmvp/`, secrets, `target/`, `.venv/`, artifacts, vendor code, large binaries, and the memory implementation itself.

Full contract: [PROJECT_MEMORY.md](./PROJECT_MEMORY.md).  
**Raw Redis key layout is private and unstable** — do not document or hard-code keys outside the tool.

---

## 4. Runtime surfaces

### 4.1 CLI map

```text
rgbmvp net status
rgbmvp wallet bootstrap-testnet|address|balance|…
rgbmvp rgb issue|transfer|verify|…
rgbmvp btc import-env|balance|…
rgbmvp swap init|fund-btc|fund-lq|claim-lq|claim-btc|refund-*|status
rgbmvp bfa issue|mint-plan|audit|demo
rgbmvp covenant address|demo|demo-c1
rgbmvp serve --bind 127.0.0.1:8080
```

### 4.2 HTTP `/v1` (labd)

| Method | Path | Notes |
|--------|------|--------|
| GET | `/v1` | Catalog JSON |
| GET | `/v1/health` | Network + `rgb_ready` |
| GET | `/v1/phases` | Ladder chip statuses |
| GET | `/v1/demo/wallets` · `/v1/demo/activity` | Board; BTC via Esplora, confidential L-BTC via server-side watch-only LWK sync with explicit live/stale metadata |
| GET | `/v1/demo/rgb/quota` | Fixed RGB lab parameters, limits, and usage |
| POST | `/v1/demo/rgb/run` | Anonymous Turnstile-gated fixed `bob → alice` Issue → Transfer → Verify |
| POST | `/v1/rgb/issue` · `/v1/rgb/transfer` · `/v1/rgb/verify` | Server-side lab wallets |
| GET | `/v1/rgb/contracts` · `/v1/rgb/plans/{id}` | Stored artifacts |
| POST | `/v1/swap/init` | Create session |
| GET | `/v1/swap/{id}` | Public view; **preimage null** |
| POST | `/v1/swap/{id}/action` | `fund_btc\|fund_lq\|claim_lq\|claim_btc\|refund_*\|set_contracts` |
| POST | `/v1/audit/bfa` | BFA history body; public compute-only mode requires embedded witness hex |

Static: `/`, `/status`, `/demo`, `/audit`.

UI state (2026-07-24): proof-first landing + `/status` evidence explorer +
read-only `/demo` + guided `/audit`; implementation and rollback evidence:
[`UI_REFRESH.md`](./UI_REFRESH.md) ·
[`UI_ROLLBACK_PLAN.md`](./UI_ROLLBACK_PLAN.md).

### 4.3 Wallet names vs addresses

| Role | Name | Wrong |
|------|------|--------|
| Alice BTC | `btc-alice` | `tb1…` address as wallet id |
| Bob Liquid | `bob` | `tlq1…` address as wallet id |

API may map common address pastes to names; still prefer names.

---

## 5. Crates (touch map)

| Crate | Responsibility |
|-------|----------------|
| `lab-cli` | Binary: CLI + embedded labd HTTP |
| `lab-rgb` | NIA, BFA, HTLC, swap session, verify |
| `lab-chain` | LWK Liquid wallet, broadcast, seals |
| `lab-btc` | BTC testnet WIF + Esplora |
| `lab-simplicity` | C0/C1 SimplicityHL driver |
| `lab-api` | `/v1` catalog helpers |
| `lab-core` | Config, health types |
| `vendor/rgb-consensus-patched` | WitnessTx patch — pin carefully |

---

## 6. Safety protocol

```text
FORBIDDEN:
  - Commit .env, .rgbmvp/, WIF, mnemonics of real value, private consignments
  - Mainnet operations without explicit human approval
  - FLUSHDB / FLUSHALL
  - Writing secrets into project memory
  - Returning preimage on public GET swap endpoints
  - Inventing dual validation in web/

REQUIRED:
  - Prefer shared /v1 for CLI and web
  - Map features to SCENARIOS.md ids
  - Disclose when project memory was not consulted
```

---

## 7. Decision shortcuts

| Question | Answer |
|----------|--------|
| Native asset or RGB? | Check claim: `rgb:` id vs Elements asset id |
| Need Lightning? | No for P0–P3 core |
| Need Docker? | Only P2 Simplicity regtest demos |
| Need Redis? | Only agent discovery cache |
| Change closed phase? | Update `*_CLOSED.md` + SCENARIOS; do not silently reopen claims |
| Split P2 to other repo? | Deferred; monorepo + HEADLESS.md |

---

## 8. Suggested agent entry sequence (new session)

```text
1. READ docs/M2M.md (this file) and AGENTS.md
2. READ docs/PURPOSE_AND_USAGE.md OR README status table
3. RUN `python3 project-memory.py status` → index if needed
4. SEARCH 2–3 queries for the task domain
5. OPEN files; implement; validate; re-index
6. IF user-facing docs change, keep PURPOSE_AND_USAGE / README consistent
```

---

## 9. Related paths

| Path | Role |
|------|------|
| `AGENTS.md` | Short always-on agent rules |
| `docs/PROJECT_MEMORY.md` | Full memory contract |
| `docs/PURPOSE_AND_USAGE.md` | Human purpose + usage |
| `docs/ARCHITECTURE.md` | System design |
| `docs/SCENARIOS.md` | Scenario ladder |
| `project-memory.py` | Canonical portable memory interface |
| `scripts/project_memory.py` | Compatibility wrapper |
