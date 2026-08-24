# Roadmap next — protocol completeness first (localhost / testnet)

**Strategy (locked 2026-07-22):**  
**Deepen protocol on testnet + localhost first.** Public Internet demo only after **U4** security gate.

Historical phase closures (P1–P3) remain valid evidence of what was proven. This roadmap is **extension work**, not a rewrite of those claims.

| Priority | Track | When |
|----------|--------|------|
| 0 | Doc honesty + ADRs | Now |
| 1 | **S3** RGB-wrapped claim (CLI + live proof) | Done — [S3_RGB_WRAP.md](./S3_RGB_WRAP.md) |
| 2 | **C2** burn mint-gate (regtest) | **Done** — [C2_CLOSED.md](./C2_CLOSED.md) |
| 3 | **C4** staking (regtest) | **Done** — [C4_CLOSED.md](./C4_CLOSED.md) |
| 4 | **U4** public-hosting security foundation | **Implemented** — [U4_PUBLIC_HOSTING.md](./U4_PUBLIC_HOSTING.md); deploy still operator-run |
| 5 | Independent review + public **read-only** demo | After U4 soak + deploy |
| UI | Proof-first web refresh + rollback plan | **Done locally** — [UI_REFRESH.md](./UI_REFRESH.md); no protocol change |

```text
Localhost / public testnet (operator)
   │
   ├─► S3 RGB-wrapped claim     ◄── done (CLI + live)
   ├─► C2 burn mint-gate        ◄── done (regtest)
   ├─► C4 stake                 ◄── done (regtest)
   │
   └─► U4 security (code done; deploy optional)
              │
              ▼
         Internet demo (read-only; Vercel + Cloud Run sketches)
```

---

## Phase 0 — Extension contract (docs)

### Claim reconciliation

| ID | Correct status |
|----|----------------|
| **S2** | Script HTLC fund/claim/refund **live** (value path) |
| **S3** | **CLI implemented** — see [S3_RGB_WRAP.md](./S3_RGB_WRAP.md). Live testnet happy-path still operator-run. Value claim alone is **not** S3. |
| **P1 closed** | Still correct for **value** HTLC lab path; RGB-wrap was always deferred in [P1_CLOSED.md](./P1_CLOSED.md) |
| **U4** | **New** — public hosting security gate (not a silent expansion of closed P3) |

### ADR stubs (fill before/while implementing)

#### ADR-S3 — RGB-wrapped claim

| Topic | Working default |
|-------|-----------------|
| Scope | CLI-first; both BTC and Liquid legs |
| Seal | Funding transfer assigns RGB allocation to **HTLC outpoint** |
| Claim tx | Preimage reveal + spend HTLC + **successor seal** + **commitment** (tapret preferred on both; opret only if required by a specific covenant demo) |
| Leg 2 preimage | Prefer extract from **confirmed claim witness**, not only local session file |
| Session | Versioned per-leg RGB fields (contract, plan ids, seals, consignment ref, verify status) |
| Done | Value claims **and** both RGB `anchor_verify` valid |
| Regression | Keep `rgb_wrap=false` value-only path |

#### ADR-C2 — Burn mint-gate (**accepted · implemented**)

| Topic | Decision |
|-------|----------|
| Burn | Explicit asset + exact tranche to **empty SPK** (SHA256∅ baked as `VAULT_SPK_HASH`) |
| Anchor | **Separate** opret vout0 (not dual-role OP_RETURN) |
| Gate | Recreate gate (same C1 program + recursion) |
| BFA | `mode=burn` + empty `vault=` in `elements-backing:v1` terms |
| Evidence | [C2_CLOSED.md](./C2_CLOSED.md) · `./scripts/demo_c2_mint_gate_burn.sh` |

#### ADR-C4 — Time-locked staking (**accepted · implemented**)

| Topic | Decision |
|-------|----------|
| Time | Absolute block height via `jet::check_lock_height` + `nLockTime` |
| Principal | Full stake input → `STAKER_SPK_HASH` (explicit asset+amount) |
| Trigger | Keyless; anyone after maturity |
| Fees | Separate P2WPKH input |
| Rewards / partial / rollover | **Out of MVP** |
| RGB | Deferred; MVP is seal-value only |
| Evidence | [C4_CLOSED.md](./C4_CLOSED.md) · `./scripts/demo_c4_stake.sh` |

#### ADR-U4 — Public vs operator surface (**accepted · implemented**)

| Topic | Decision |
|-------|----------|
| Public | GET static + `/v1` read surface; see `GET /v1/security` |
| Mutations | Bearer `LABD_API_TOKEN` (constant-time); required if `LABD_PUBLIC_READ_ONLY` or non-loopback |
| CORS | Allowlist `LABD_CORS_ORIGINS` (no `*` in public mode) |
| labd bind | Operator `127.0.0.1`; Cloud Run `0.0.0.0:$PORT` + read-only |
| Docker RPC | Host bind **127.0.0.1** only |
| Mainnet | Forbidden at config load |
| Evidence | [U4_PUBLIC_HOSTING.md](./U4_PUBLIC_HOSTING.md) · `Dockerfile.public` · `deploy/` |

---

## Phase S3 — RGB-wrapped claim (primary)

**Goal:** one transaction per leg reveals preimage, closes HTLC-bound RGB seal, creates receiver seal, re-anchors, passes verify.

**Surfaces:** CLI first; `/v1` status only after invariants stable.

**Exit:** documented runbook + negative tests (missing/wrong commitment, wrong seal, bad consignment, failed verify after broadcast, preimage extract failures).

**Estimate:** 2–4 weeks.

---

## Phase C2 — Burn mint-gate

Reuse C1 tooling. Program + demo + BFA burn mode + negatives.

**Estimate:** 1–2 weeks after ADR-C2.

---

## Phase C4 — Staking

**Closed** — [C4_CLOSED.md](./C4_CLOSED.md). Absolute height + principal-home; no rewards/partial.

---

## Phase U4 — Security gate (before Internet)

**Implemented in-tree** — [U4_PUBLIC_HOSTING.md](./U4_PUBLIC_HOSTING.md).  
Still **required** before marketing a public URL: deploy soak + no secrets in image.

MVP delivered: loopback RPC ports, `LABD_PUBLIC_READ_ONLY`, Bearer on POST, id regex, CORS allowlist, body limit, public Dockerfile + Vercel/Cloud Run sketches.

---

## Explicit non-goals (near term)

- Public Internet labd without U4  
- Browser-side RGB transition construction  
- Reopening P1/P2/P3 as “failed” — they closed the scopes they claimed  
- Mainnet  

---

## Engineering ladder (approved sequence)

**Completed order:** S3 negatives → application services → **U5** Axum → S3 HTTP/browser; **C5** docs in parallel.
**S5** is intentionally moved to a post-freeze extension milestone.
Do **not** add S3 mutation business logic to the handwritten HTTP server and then rewrite it.

| Phase | Work | Scenario | Status |
|-------|------|----------|--------|
| 0a | Docker Trivy `load: true` | CI | **Done** (`.github/workflows/docker-public.yml`) |
| 0b | Services + public swap view shared | prep for U5 | **Done** — `SwapService` + `lab_api::s3` fund-wrap/claim |
| 1 | S3 offline negative matrix + witness extract tests | S3 harden | **Done** — real verifier mutations + fake broadcaster/verifier cases in required CI |
| 2 | Axum/Hyper labd | **U5** — [U5_AXUM.md](./U5_AXUM.md) | **Done** (default); `LABD_HTTP=legacy` fallback |
| 3 | Authenticated S3 HTTP + browser (preserve U2/U4) | S3 surfaces | **Done** — live HTTP `s3-browser-20260724-0112` → phase=done; console mode |
| 4 | Round-trip twin swaps | **S5** | **Deferred** — post-freeze extension; not required by P1/S3 claims |
| ∥ | LiquiDEX comparison writeup | **C5** — [C5_LIQUIDEX_COMPARISON.md](./C5_LIQUIDEX_COMPARISON.md) | **Complete** — sourced positioning; no implementation claim |

**U5** is a new ops/platform scenario. It must not silently reopen **U4** or **P3**.  
**Mainnet** remains out of scope throughout.

### Service boundary (current)

```text
lab-cli     → Clap; calls services
lab-api     → SwapService, public_swap_view, /v1 JSON helpers, lab_api::s3
lab-rgb     → session phase, S3 gates, HTLC, RGB domain
labd/Axum   → HTTP routing + U4 middleware; same services (U5)
```

### S3 negatives (CI)

**Done:** offline domain/extract tests, real claim-plan/witness mutations, and
FakeBroadcaster/FakeRgbVerifier application-service cases run in the required
`lab-rgb` + `lab-api` CI job. Optional live testnet mutation remains operator-run
or `workflow_dispatch` only; public faucet state is not a deterministic CI gate.

### Priority recommendation (status as of 2026-07-27)

| Priority | Item | Status |
|----------|------|--------|
| 1 | S3 negative automation | **Closed** — offline verifier + application-service matrix required in CI |
| 2 | Service extraction + Axum (U5) | **Closed** |
| 3 | S3 browser/API workflow | **Closed** — live evidence in `artifacts/public/s3-browser-20260724.json` |
| 4 | S5 round-trip | **Deferred** — named post-freeze extension; non-blocking for current closure |
| 5 | C5 LiquiDEX writeup | **Closed** — documentation positioning complete |

### T1 — bounded public demo swaps (new track, 2026-08-10)

**In tree, OFF by default (`LABD_DEMO_SWAPS`), never deployed.** Plan, budget,
and ADR-T1: [TESTNET_PUBLIC_SWAPS.md](./TESTNET_PUBLIC_SWAPS.md).

T1 deliberately narrows ADR-U4 rather than reopening it: the read-only freeze
remains the default and the only shipped public profile. When the flag is on,
labd gains exactly **one** public mutation (`POST /v1/demo/swap`) which accepts
no protocol parameters, and holds spendable **testnet** keys to run swaps
between the lab's own wallets.

| Workstream | State |
|---|---|
| W1 constrained trigger endpoint | Implemented |
| W2 quotas / caps / fee budget | Implemented |
| W3 Secret Manager custody + startup preflight | Implemented |
| W4 budget persistence across restart | Implemented |
| W5 refund / recycle watcher | Implemented |
| W6 deploy + same-service rollback profiles | `deploy/cloudrun-demo.yaml` + `deploy/cloudrun-demo-freeze.yaml` + runbook |
| W7 observability / alert thresholds | Documented; quota field contract pinned by test |
| W8 abuse + chaos tests | Implemented |
| W9 public UX | **Implemented** — Turnstile browser pass still unproven |

**Two live swaps completed** — [T1_FIRST_SWAP.md](./T1_FIRST_SWAP.md):
run 1 operator/CLI, run 2 via the **automated `POST /v1/demo/swap` driver** with
no human in the loop. Fees measured (5.25 / 3.63 sat/vB); the driver's retry
loop recovered from both an unconfirmed-UTXO wait and an Esplora propagation
race. That run observed W2 reservation→spend and completed-state persistence;
it did **not** prove crash durability. Write-ahead admission and fail-closed
recovery are now implemented in
[T1_FEE_BUDGET_REMEDIATION.md](./T1_FEE_BUDGET_REMEDIATION.md), with the isolated
deployment interruption drill still pending.

Run 3 exercised the **W5 refund watcher** and CSV refund path unattended, and
the Liquid-side exit sweep was implemented and proven live (118,624 sats
recovered from 119,024 stranded).

**Still not proven:** **Turnstile against a real request** — every run used
`LABD_DEMO_TURNSTILE_REQUIRED=0` (no Cloudflare secret locally). That is now the
last functional gate before public exposure. Mainnet remains refused at
config load throughout; §5 of the plan gates it behind a separate program.

### Post-freeze extension milestone

**S5** remains a valid future protocol scenario with its original conservation
criteria, but it is not part of the current testnet proof-of-concept freeze.
Neither the closed P1 value-HTLC claim nor the S3 RGB-wrapped-claim evidence
depends on a reverse swap. Reopening S5 requires a separately approved extension
milestone, test plan, and evidence run.

---

## Next concrete actions

1. ~~Lock strategy: protocol first, localhost.~~  
2. ~~Reconcile S3/U4 in SCENARIOS + this roadmap.~~  
3. ~~Implement **S3** CLI + live proof.~~ → [S3_RGB_WRAP.md](./S3_RGB_WRAP.md)  
4. ~~**C2** burn mint-gate on regtest.~~ → [C2_CLOSED.md](./C2_CLOSED.md)  
5. ~~**C4** staking.~~ → [C4_CLOSED.md](./C4_CLOSED.md)  
6. ~~**U4** security engineering.~~ → [U4_PUBLIC_HOSTING.md](./U4_PUBLIC_HOSTING.md)  
7. ~~Public content + CI + harden.~~ → [PUBLIC_LAUNCH.md](./PUBLIC_LAUNCH.md) · `artifacts/public/` · `.github/workflows/*`  
8. Operator: enable deploy secrets → 24–48h soak → announce (ops; parallel).  
9. ~~**Service extraction** (`lab_api::s3` / `SwapService`).~~  
10. ~~**U5** Axum default labd.~~ → [U5_AXUM.md](./U5_AXUM.md)  
11. ~~**S3 HTTP + browser** + live testnet path.~~ → [S3_RGB_WRAP.md](./S3_RGB_WRAP.md) · `artifacts/public/s3-browser-20260724.json`  
12. ~~**S3 negatives:** fixture/fake-adapter matrix in required CI.~~
13. **S5:** deferred to a separately approved post-freeze extension milestone.
14. ~~**C5** docs polish (positioning).~~ → [C5_LIQUIDEX_COMPARISON.md](./C5_LIQUIDEX_COMPARISON.md)
15. Retain `LABD_HTTP=legacy` through the first successful 24–48h public soak; removal is a post-soak cleanup gate.
16. ~~Proof-first UI/UX refresh + documented rollback.~~ → [UI_REFRESH.md](./UI_REFRESH.md) · [UI_ROLLBACK_PLAN.md](./UI_ROLLBACK_PLAN.md)
