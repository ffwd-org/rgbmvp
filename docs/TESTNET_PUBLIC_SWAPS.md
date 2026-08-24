# Testnet public swaps — live demo plan (bounded public trigger)

**Status:** Implemented in tree, OFF by default; Turnstile browser pass and
public deployment pending · 2026-08-11
**Goal:** Let public visitors trigger **real Liquid/Bitcoin testnet HTLC swaps**
from the hosted site, run that live for **~2 weeks**, then use the evidence to
open a separate mainnet-readiness program.
**Execution model (decided):** *Bounded public trigger* — visitors click
“run a demo swap”; the server executes both sides with **hard caps, per-IP
quotas, global rate limits, a capped faucet float, and bot protection**.

> This document reverses part of ADR-U4 on purpose. The current public freeze is
> **read-only, no wallets, no token** (`deploy/cloudrun.yaml`). Running swaps
> means the server now **holds spendable testnet keys and broadcasts
> transactions**. That is a categorically larger attack surface; the controls
> below exist to make it acceptable **on testnet only**. Mainnet remains refused
> at config load (`lab-core/src/lib.rs`, `lab-btc/src/lib.rs`) throughout.

---

## 1. Threat model (what changes vs. the read-only freeze)

| New capability on the public box | Abuse it enables | Primary control |
|---|---|---|
| Server signs + broadcasts testnet txs | Faucet drain; broadcast spam; mempool/DoS | Amount caps · faucet float cap · per-IP + global rate limit · concurrency cap |
| Public can trigger mutations | Automated abuse, cost blow-up | Turnstile (bot check) · single constrained endpoint · no arbitrary params |
| Hot testnet keys in container | Key exfiltration if RCE | Secret Manager (not env) · dedicated low-float wallets · rotation · kill switch |
| Long-lived swap state | Stuck/leaked funds on restart | Persistent volume · refund watcher · min-instances=1 |

**Non-goals for the 2 weeks:** real value, mainnet, non-custodial browser
signing, and exposing the raw `/v1/swap/{id}/action` endpoint publicly (it takes
arbitrary `amount_sats`, `fee_sats`, `csv_delay`, wallet names — keep it
token-gated).

---

## 1a. Wallet inventory & budget (measured 2026-08-10)

Swaps run **only between our known lab wallets** (visitor triggers, never
supplies keys or amounts). Live balances at planning time:

| Chain | Wallet | Balance (sats) | Role in demo |
|---|---|---|---|
| Liquid | maker | 983,764 | reserve |
| Liquid | carol | 600,950 | observer / reserve |
| Liquid | **bob** | 146,633 | **swap counterparty (LQ leg)** |
| Liquid | alice | 103,946 | actor |
| Liquid | lab0 | 94,433 | misc |
| **Liquid total** | | **1,929,726** | plentiful; faucet easier |
| BTC | btc-funder | 104,861 | deep reserve / refill source |
| BTC | **btc-alice** | 33,607 | **active swap wallet (BTC leg)** |
| **BTC total** | | **138,468** | **scarce; faucets hard** |

**Binding constraint: BTC testnet — specifically btc-alice (~33.6k sats).** BTC
testnet faucets are slow/dry, so the demo is sized around **BTC fee burn**, not
Liquid. With the W5 sweep in place a swap permanently loses only *fees + any
dust*; without it the leg value strands at a demo exit address (see below).

### Where HTLC value actually goes (security-corrected 2026-08-13)

An earlier draft of this plan claimed refunds "return value to the original
funder". **That is wrong.** Every HTLC exit path — claim *and* refund, on both
chains — pays a P2WPKH child key derived by hardened BIP32 from a secret T1
root seed and a public role label, never the funding wallet. The label selects
a role; it is not private-key material:

| Path | Destination | Chain |
|---|---|---|
| BTC claim | secret-seed child `bob-claimer` | BTC |
| BTC refund | secret-seed child `alice-refund` | BTC |
| Liquid claim | secret-seed child `alice-claimer` | Liquid |
| Liquid refund | secret-seed child `bob-refund` | Liquid |

Consequences:
- **`btc-alice` drains on every swap regardless of outcome.** A completed swap
  costs it `leg + fund_fee`; the leg value lands at `bob-claimer`.
- The addresses are deterministic only for an operator holding the 256-bit
  root seed. Public labels and source code are insufficient to sign. Recovery
  requires an explicit sweep, which is implemented
  (`lab_btc::sweep_all_demo_exits`, CLI `rgbmvp btc sweep-demo`, and
  automatically in the W5 watcher).

### Mandatory migration from public-label exit keys

Releases before the 2026-08-13 remediation used `sha256(public_label)` as a
private key. Treat all four legacy exit keys as public and compromised. Do not
deploy the new derivation over active legacy sessions:

1. Keep `LABD_DEMO_SWAPS=0` and stop the watcher from creating new work.
2. With the pre-remediation binary and wallet state, finish or refund every
   active `demo-<epoch>-<seq>` session.
3. Run `rgbmvp btc sweep-demo --to btc-alice --include-liquid --lq-to bob` and
   verify all four legacy exit balances are zero.
4. Provision the new `rgbmvp-demo-exit-seed` secret as described in
   `deploy/README.md` §5.3, then deploy the remediated binary.
5. Prove one operator-triggered testnet swap, its watcher pass, and its exit
   sweep before enabling the public trigger.

New session records include `exit_key_scheme` and a non-secret `exit_key_id`.
Signing fails closed for legacy sessions or if the mounted seed does not match.
Never rotate the seed while sessions or exit outputs remain; drain first and
retain the old secret version until zero balances are independently verified.

### Cost model per completed swap (repo-proven fees)

| Flow | Sats |
|---|---|
| `btc-alice` pays | **1,800** (1,000 leg + 800 fund fee) |
| Driver fees paid | 1,300 (800 fund + 500 claim/refund) |
| Exit value before sweep | 500 |
| Conservative exit-sweep allowance | up to 500 |
| **Budget charge per admission** | **1,800** |

**Cost-minimization defaults:**
- **Value-only HTLC path (`rgb_wrap=false`)** — avoids tapret commitment dust
  (~330 sats/leg) and extra RGB transactions. RGB-wrapped stays operator-only.
- **Minimal leg size:** 1,000 sats/leg, server-fixed.
- **Fees are the repo's proven values** (fund 800 / claim 500 / LQ 300), not the
  ~200-sat vbyte estimate an earlier draft used. Now **measured live** at 5.25
  and 3.63 sat/vB — see [T1_FIRST_SWAP.md](./T1_FIRST_SWAP.md). There is room to
  lower them (~450/300 ≈ 3.0/2.2 sat/vB) but one sample is not a fee policy;
  gather 2–3 more swaps first.
- **Recycle:** the W5 watcher sweeps the demo exit addresses back to `btc-alice`
  every cycle, so only fees leave the pool.

### Budget-grounded quota table (starting values — tune during soak)

| Control | Value | Rationale |
|---|---|---|
| Leg size (BTC & LQ) | 1,000 sats | Just above dust; minimal footprint |
| BTC budget/admission | 1,800 sats | 800 fund + 500 claim/refund + 500 sweep |
| **BTC fee budget** | **28,000 sats** | of btc-alice's 33.6k, leaves buffer |
| **Total admissions** | **15** | floor(28,000 / 1,800), with 1,000 sats headroom |
| Daily cap | 6 swaps | budget binds within the bounded run |
| Global rate | 1 concurrent · 1 new / 10 min | Prevents bursts / mempool spam |
| Per-IP quota | 1 / hour · max 2 / day | One visitor can't hog the budget |
| Pause floor — btc-alice | < 5,000 sats | Refill from btc-funder, else pause |
| Pause floor — bob (LQ) | < 20,000 sats | Comfortable; LQ is plentiful |

The budget never credits batching savings in advance: every admission consumes
the full 1,800-sat capacity. If the soak establishes lower safe fees, all three
fee inputs and the maximum reservation can be reviewed together. If BTC runs low the demo
**pauses gracefully** (503), never drains or crashes.

Under-reservation fails closed at startup, and unexpected actual fees are
recorded without clamping; see
[T1_FEE_UNDER_RESERVATION_REMEDIATION.md](./T1_FEE_UNDER_RESERVATION_REMEDIATION.md).

---

## 2. Architecture — the constrained trigger

Do **not** expose the granular action endpoint. Add one new public endpoint that
runs a **fully server-parameterized** swap:

```
POST /v1/demo/swap            (public, Turnstile-gated, rate-limited)
  body: { turnstile_token }   ← no amounts, no wallets, no csv from the client
  server fixes: amount, fee, csv_delay, wallets (faucet pool), rgb_wrap
  returns: { swap_id }        ← then the UI polls GET /v1/swap/{id}

GET  /v1/swap/{id}            (already public-safe; preimage always redacted)
GET  /v1/demo/quota           (optional: remaining global/per-IP budget)
```

Server-side orchestrator walks the existing phases
(`init → fund_btc → fund_lq → claim_lq → claim_btc → done`) via the internal
`SwapService`, never via the public HTTP action path. Every arbitrary mutating
step stays behind the existing `MutationPolicy` (token/loopback); the exact
public path carries no attacker-controlled protocol parameters.

**ADR-T1 (new):** public mutation is allowed **only** through `/v1/demo/swap`,
with server-fixed parameters, on testnet, behind Turnstile + quotas. All other
mutating endpoints remain denied in public mode. Reverting = unset the demo flag.

The later fixed RGB lab adds one separate exact exception,
`POST /v1/demo/rgb/run`. It accepts only `turnstile_token`, binds action
`rgbmvp_rgb_lab`, runs predefined `bob → alice` Issue → Transfer → Verify on
Liquid Testnet, and uses `/data/rgb_demo_budget.json`; it neither broadens
`/v1/rgb/*` nor shares T1's quota ledger.

---

## 3. Workstreams

### W1 — Constrained execution endpoint
- New `POST /v1/demo/swap` gated by a new env flag (e.g. `LABD_DEMO_SWAPS=1`)
  that is **independent** of `LABD_PUBLIC_READ_ONLY` and **off by default**.
- Server-side orchestrator: run the scripted swap end-to-end with fixed params;
  persist `swap_id`; return immediately and let the UI poll.
- Reject if global/per-IP/concurrency budgets are exceeded (see W2).
- Keep `/v1/swap/{id}/action` token-gated and undocumented on the public UI.

### W2 — Abuse controls (the core of “bounded”)
All starting values are in the **§1a quota table** (budget-grounded on the
measured BTC scarcity). Implement each as config so they tune without a redeploy.
- **Amount cap:** demo leg size server-fixed at ~1,000 sats; reject any request
  or config that raises it in demo mode. Value-only path by default.
- **Faucet float floor:** pause new swaps when btc-alice < 5,000 sats (auto-refill
  from btc-funder if available) or bob (LQ) < 20,000; alert on low float.
- **Per-IP quota + global rate limit + concurrency cap + daily/total budget:**
  extend the existing `RateLimiter` (`lab-core/src/security.rs`) to cover
  `/v1/demo/swap`; add an in-flight counter, a per-day swap counter, and a
  running 2-week fee-budget counter that hard-stops at ~28,000 sats BTC.
- **Proxy-aware identity:** configure the exact trusted right-edge XFF suffix
  with `LABD_XFF_TRUSTED_HOPS`. For the documented Google load-balancer chain,
  `1` selects the next-to-last client IP. Invalid, duplicate, oversized, or
  underspecified chains fall back to the socket-peer bucket.
- **Bot protection:** Cloudflare **Turnstile** in front of the trigger
  (server-side Siteverify). The widget requests the fixed action
  `rgbmvp_demo_swap`; the server requires `success=true`, that exact action, and
  a hostname in `LABD_DEMO_TURNSTILE_HOSTNAMES`. Missing or malformed hostname
  configuration refuses T1 startup. Use the `turnstile-spin` workflow.
- **Body/label limits:** already enforced (`DefaultBodyLimit`, `is_safe_path_id`).

### W3 — Custody & key management
- Dedicated **demo faucet wallets** (Alice/Bob roles), low float, **not** the
  issuer keys; separate from any operator wallet.
- Inject mnemonics via **GCP Secret Manager** mounted at runtime — **never** as
  plain Cloud Run env vars, never baked into the image (gitleaks + Trivy already
  in CI guard the image).
- **Rotation** procedure + a **kill switch** (unset `LABD_DEMO_SWAPS`, redeploy)
  that instantly returns the box to the read-only freeze.
- Runtime SA keeps **no** project IAM roles beyond Secret Manager accessor.

### W4 — State persistence (swaps outlive restarts)
- Move `RGBMVP_DATA_DIR` off `/tmp` to a **persistent** store: Cloud Run volume
  (GCS/Filestore) or a small persistent disk; set **`min-instances=1`** so the
  demo is always warm and refund timers keep running.
- Ensure swap sessions + consignments survive revision changes; verify a swap
  mid-flight during a deploy is not orphaned.
- Admission is now write-ahead and fail-closed: no session or broadcast before
  a durable reservation; recovered/unknown reservations remain charged; corrupt
  state refuses startup. See
  [T1_FEE_BUDGET_REMEDIATION.md](./T1_FEE_BUDGET_REMEDIATION.md).

### W5 — Refund/liveness safety
- **Refund watcher:** background task that, after the CSV window, auto-refunds
  funded-but-unclaimed HTLCs, **then sweeps the demo exit addresses back to the
  funding wallet** (see the corrected value-flow table in §1a — a refund alone
  does not return funds to `btc-alice`).
- Bound the demo to the value path + optional `rgb_wrap`; make CSV delay short
  enough to recover funds within the demo window but valid on testnet.
- Idempotency: never double-fund (the action path already guards this); make the
  orchestrator resumable from persisted phase.

### W6 — Deployment infra
- New Cloud Run profile (or a `deploy/cloudrun-demo.yaml`) diffed from the freeze:
  `LABD_DEMO_SWAPS=1`, Secret Manager mounts, persistent volume, `min-instances=1`,
  egress allowed to public **Esplora + Electrum** testnet endpoints.
- Keep `deploy/cloudrun-demo-freeze.yaml` as the **same-service rollback**
  target for `rgbmvp-demo`. `deploy/cloudrun.yaml` targets the independent
  `rgbmvp-public` service and is not a T1 rollback.
- Budget alerts + max-instances cap to bound cost.

### W7 — Observability & ops

**Metrics source:** `GET /v1/demo/quota` is the single public scrape target. Its
field contract is pinned by a test (`quota_json_exposes_the_fields_ops_alerts_depend_on`)
so a refactor cannot silently blind monitoring. Unknown balances report `floats:
null` rather than `0`, so a monitoring gap never reads as an empty wallet.

**Alert thresholds** (Cloud Monitoring / any poller against `/v1/demo/quota`):

| Alert | Condition | Why it matters |
|---|---|---|
| Budget nearly spent | `budget.swaps_remaining_est < 10` | Run is ending; decide on refill or stop |
| BTC float low | `floats.btc_sats < floats.btc_floor_sats * 1.5` | Faucet refill needed *before* it pauses |
| Liquid float low | `floats.lq_sats < floats.lq_floor_sats * 1.5` | Same, Liquid side |
| Demo paused | any `503` from `/v1/demo/swap` | Visitors are being turned away |
| Stuck swap | `usage.in_flight == 1` for > 90 min | Driver wedged; check the refund watcher |
| Instance down | `/v1/health` failing | `min-instances=1`, so this is real downtime |

**Log queries** (`gcloud run services logs read rgbmvp-demo`):

| Grep | Meaning |
|---|---|
| `T1 custody` | Startup custody verdict — `OK` or a refusal |
| `T1 demo budget restored` | Confirms W4 persistence is actually working |
| `demo sweep:` | Refund watcher activity (only logs when it acts or errors) |
| `demo: swap .* failed` | Driver failures worth investigating |
| `turnstile` | Bot-check misconfiguration |

**Secret hygiene:** no log line carries key material. Denial messages are
public-facing and are asserted secret-free by
`denial_messages_leak_nothing_sensitive`; the quota endpoint is asserted
secret-free by `demo_quota_is_public_and_safe`; the public swap view already had
a preimage-redaction regression test.

**Daily soak checklist:** budget remaining · both floats vs floors · error rate ·
one end-to-end swap spot-checked on the explorer · cost vs budget alert.

**Incident response:** kill switch is `LABD_DEMO_SWAPS=0` on `rgbmvp-demo`,
followed by `update-traffic --to-latest` and an `enabled == false` check. After
active sessions are resolved and exits swept, full rollback replaces that same
service with `deploy/cloudrun-demo-freeze.yaml`; `deploy/cloudrun.yaml` must not
be used because it names `rgbmvp-public`. See
[`deploy/README.md` §5.6](../deploy/README.md).

### W8 — Testing (before public exposure)
- Abuse simulation: hammer `/v1/demo/swap` past per-IP and global limits;
  confirm rejects, not faucet drain.
- Faucet-drain simulation: run to float exhaustion; confirm graceful “demo
  paused” state, not crashes or stuck funds.
- Chaos: kill the instance mid-swap; confirm resume + refund watcher recovers.
- E2E refund path on testnet (fund → wait CSV → auto-refund).
- Keep the existing required CI (`cargo test`, gitleaks, Trivy, `cargo audit`).

### W9 — Public UX  *(implemented 2026-08-11, except the Turnstile key)*

**No endpoint changes are required.** The API is already shaped for this UI:
`GET /v1/swap/{id}` returns a ready-made `steps[]` array (`created → funded_btc →
funded_lq → claimed_lq → claimed_btc → done`, each with `done` + `label`) plus a
`links{}` map of explorer URLs; `GET /v1/demo/quota` exposes limits, budget,
floats and usage; denials return typed codes with `Retry-After`. W9 is a
presentation layer over what already exists.

**Decision: a new self-contained `web/swap.html` at `/swap`.** The operator
console in `index.html` drives `/v1/swap/init` and `/v1/swap/{id}/action` — the
arbitrary-parameter endpoints, which are token-gated and return 403 in public
mode. The public flow must not reuse it. A separate page keeps the two audiences
apart and means nothing already shipping in the read-only freeze is edited.

#### W9.1 — CSP (blocking prerequisite, and a U4 decision)

Before W9, the header (`labd_axum.rs`, `apply_security_headers`) was:

```
default-src 'self'; script-src 'self' 'unsafe-inline'; connect-src 'self';
frame-ancestors 'none'; …
```

Turnstile loads a script from `challenges.cloudflare.com` **and renders inside an
iframe from that origin**. With no `frame-src` directive, it fell back to
`default-src 'self'`, blocking both the widget script and iframe. W9 therefore
required the following narrow CSP change.

Implemented change:
- `script-src`: add `https://challenges.cloudflare.com`
- add `frame-src https://challenges.cloudflare.com`
- **`connect-src` stays `'self'`** — siteverify is server-side, so the browser
  never talks to Cloudflare directly.

`security_headers_on_v1_get` now pins the exact allowed origin, so a later edit
cannot quietly widen the policy.

#### W9.2–W9.7 — the page

| Item | Work |
|---|---|
| W9.2 | Turnstile widget (`action=rgbmvp_demo_swap`) → `POST /v1/demo/swap` with `{turnstile_token}` → Siteverify success + exact action + exact hostname → `{swap_id}` |
| W9.3 | Poll `GET /v1/swap/{id}`; render `steps[]` as a checklist with per-tx explorer links; stop on `done`/`refunded`; surface the wait honestly (“waiting for a Bitcoin block — this can take a while on testnet”) |
| W9.4 | Quota banner from `/v1/demo/quota`: swaps left today, “paused — awaiting faucet refill” when a float floor is hit, disable the button with a countdown on `Retry-After` |
| W9.5 | Typed denial rendering — friendly copy per `code` (`turnstile_required`, `demo_cooldown`, `demo_daily_cap`, `demo_busy`, `demo_budget_exhausted`, `demo_low_float`, `demo_float_unknown`); never show raw JSON |
| W9.6 | Disclaimers: testnet only · no real value · capped · may pause · **the swap runs between the lab's own wallets, not yours** |
| W9.7 | Route `/swap` + `/swap.html` in labd; nav entry on all pages; hide the operator wizard when `public_read_only`; test asserting the public page never renders a preimage |

**Constraints to preserve:** single self-contained HTML file, one inline
`<script>`, **no build step and no external JS dependency** beyond the Turnstile
widget itself — matching every other page in `web/`.

**Out of scope:** endpoint changes (none needed), bundlers/frameworks, and
Turnstile key provisioning (a Cloudflare account step, tracked separately).

#### Status

| Item | State |
|---|---|
| W9.1 CSP + pinned test | **Done** — verified live on the served header |
| W9.2 Turnstile widget → trigger | **Wired, unproven** — needs a sitekey |
| W9.3 phase tracker | **Done** — binds `steps[]` + `links{}` |
| W9.4 quota banner / pause / cooldown | **Done** |
| W9.5 typed denial copy | **Done** — one message per code |
| W9.6 disclaimers | **Done** |
| W9.7 route, nav, leak test | **Done** — `/swap`, `/swap.html` |

`LABD_DEMO_TURNSTILE_SITEKEY` (public, non-secret) is surfaced through
`/v1/demo/quota`. **Without it the page renders a "bot check is not configured"
state and keeps the trigger disabled** rather than showing a broken widget — so
the Turnstile-shaped hole fails safe, and closing it later is config-only, not
a code change. Public T1 also requires a comma-separated exact DNS allowlist in
`LABD_DEMO_TURNSTILE_HOSTNAMES`; URLs, ports, paths, wildcards, and malformed
labels are rejected. A successful token from another widget action or hostname
is rejected before chain reads or fee admission.

**Still unverifiable until a key exists:** the Turnstile *pass* path, end to end
in a browser — the same gap noted in [T1_FIRST_SWAP.md](./T1_FIRST_SWAP.md) §10.
Everything else was exercised with `LABD_DEMO_TURNSTILE_REQUIRED=0`.

---

## 4. The 2-week soak

**Entry gates (all green before public launch):**
- W1–W8 complete; abuse + chaos + refund tests pass.
- Secrets via Secret Manager; image scans clean; `cargo audit` clean.
- Kill switch verified: one redeploy returns to read-only freeze.
- `GET /v1/security` shows expected posture; only enabled exact demo paths are
  publicly mutating, and all arbitrary `/v1/rgb/*` and `/v1/swap/*` remain locked.

**Daily during soak:**
- Check faucet float, error rates, quota saturation, cost vs. budget.
- Spot-check a live swap end-to-end on the explorer.
- Watch for abuse patterns; tighten quotas if needed (config-only, no redeploy
  where possible).

**Success criteria (what the 2 weeks must prove):**
- N successful public swaps with zero custody incidents and zero stuck funds.
- Abuse controls held (no faucet drain, no runaway cost, no DoS outage).
- Refund watcher recovered every unclaimed HTLC.
- Clean logs (no secret leakage), stable uptime.

---

## 5. Mainnet-readiness gate (separate program — NOT in these 2 weeks)

A successful testnet soak is **necessary but far from sufficient** for mainnet.
Mainnet is a distinct, higher-bar program. Minimum deltas before it is even
scoped:

- **Independent security audit** of the HTLC scripts, the vendored
  `rgb-consensus-patched` seal/DBC verification, and the custody path.
- **Custody rethink:** custodial hot-key server is acceptable on testnet, **not**
  for real value. Move to non-custodial browser signing, MPC, or a hardware-backed
  signer + strict spend policy before mainnet.
- **Real-value controls:** withdrawal limits, monitoring/alerting SLOs, formal
  incident response, and a tested disaster-recovery plan.
- **Legal/compliance review** for operating a live swap service.
- Remove the mainnet config refusal only behind an explicit, reviewed ADR — it is
  the current safety backstop and stays until this gate is passed.

---

## 6. Risk register (top items)

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| Faucet drain / cost blow-up | Med | Med | Amount + float caps, quotas, budget alerts, kill switch |
| Stuck/orphaned HTLC funds | Med | Med | Persistent state, refund watcher, min-instances=1 |
| Hot-key exfiltration via RCE | Low | High (testnet) | Secret Manager, low float, rotation, minimal SA, dep audit |
| Broadcast spam / mempool abuse | Med | Low | Global rate limit, concurrency cap, Turnstile |
| Secret leakage in logs | Low | Med | Scrubber + test, redaction already on public views |
| Posture creep toward mainnet | Low | High | Mainnet refused at config load; separate gated program |

---

## 7. Indicative schedule (~2 weeks prep + 2 weeks soak)

- **Days 1–3:** W1 endpoint + W2 caps/quotas + Turnstile.
- **Days 4–6:** W3 secrets/custody + W4 persistence + W5 refund watcher.
- **Days 7–9:** W6 deploy profile + W7 observability/alerts + runbook.
- **Days 10–12:** W8 abuse/chaos/refund tests; fix; re-test.
- **Day 13:** entry-gate review; kill-switch drill.
- **Day 14:** public launch → **2-week soak** with daily ops.
- **Post-soak:** write results; open the mainnet-readiness program (§5).
