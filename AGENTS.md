# Repository agent instructions

This repository is **rgbmvp** — a **public lab** for **RGB on Liquid Testnet**
(CLI + browser lab console + `/v1` API). **Repository files are always the
authoritative source of truth.**

## Machine entry (preferred)

1. **[docs/M2M.md](docs/M2M.md)** — goals, invariants, `/v1` map, project-memory protocol  
2. **[docs/PURPOSE_AND_USAGE.md](docs/PURPOSE_AND_USAGE.md)** — human purpose · **[docs/MANIFESTO.md](docs/MANIFESTO.md)** — community vision  
3. [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) · [docs/SCENARIOS.md](docs/SCENARIOS.md) · [docs/STACK.md](docs/STACK.md)

Phase closures: [P1_CLOSED](docs/P1_CLOSED.md) · [P2_CLOSED](docs/P2_CLOSED.md) · [P3_CLOSED](docs/P3_CLOSED.md).  
**Next:** [docs/ROADMAP_NEXT.md](docs/ROADMAP_NEXT.md) — S3+C2+C4+U4 code done; optional **public deploy** (Vercel/Cloud Run) after soak — [U4_PUBLIC_HOSTING](docs/U4_PUBLIC_HOSTING.md).  
Headless protocol kit: [docs/HEADLESS.md](docs/HEADLESS.md).

## Do not confuse

- **Native Liquid issued assets** (LWK `issue_asset`) ≠ **RGB contracts** (`rgb:` + consignments + seal + commitment).  
- **Cross-chain** = atomic swap of **twins**, not moving one contract id.  
- **Lightning / CLN:** not required for P0–P3; do not block on `lightningd`.  
- **Wallet name** (`btc-alice`, `bob`) ≠ payment **address** (`tb1…`, `tlq1…`).

## Project Memory (optional Redis discovery cache)

**Humans may ignore this.** Agents: use the portable root entrypoint; the script path is compatibility-only.

```bash
python3 project-memory.py status   # 0=fresh, 2=stale/missing, 1=error
python3 project-memory.py index --incremental    # if status ≠ 0
python3 project-memory.py validate
python3 project-memory.py search "QUERY" --limit 5
```

- Hits are **pointers only** — open the file before claiming or editing; cite the file, not Redis.  
- After changing indexed files, re-`index` and confirm `status` exit `0`.  
- If Redis is down: continue from files; set/disclose `cache_consulted: false`.  
- **Never** `FLUSHDB` / `FLUSHALL`, other namespaces, secrets in the cache, or raw Redis keys.  
- Default: `redis://localhost:6379/0` · override: `--url`, `PROJECT_MEMORY_URL`, or legacy `RGBMVP_PROJECT_MEMORY_URL`.

Full contract: [docs/PROJECT_MEMORY.md](docs/PROJECT_MEMORY.md) · protocol: [docs/M2M.md](docs/M2M.md) §3.

## Implementation rules

- Prefer shared **`/v1` JSON** for CLI and web; no duplicate validation in the UI.  
- Public demos: **Liquid Testnet** + **Bitcoin testnet** for P1/P3 swap. Mainnet only with explicit human flag.  
- Never commit `.env`, `.rgbmvp/`, WIF, real seeds, or private consignments.  
- Map features to scenario ids in `docs/SCENARIOS.md` (`R*`, `S*`, `C*`, `U*`, `T*`).  
- `GET /v1/swap/*` must keep **preimage redacted**.

## T1 demo swaps — invariants (do not regress)

`POST /v1/demo/swap` and `POST /v1/demo/rgb/run` are the **only** endpoints an
unauthenticated visitor may use to cause a state change, and only when their
independent flags are enabled (both off by default).
Contract: [docs/TESTNET_PUBLIC_SWAPS.md](docs/TESTNET_PUBLIC_SWAPS.md).

- The RGB demo accepts only `turnstile_token`, requires action
  `rgbmvp_rgb_lab`, fixes `bob → alice` and every chain/asset/amount/broadcast
  parameter server-side, and uses a quota ledger separate from T1.

- The demo endpoint accepts **only** a bot-check token. Amounts, fees, CSV delay,
  wallet names, and `rgb_wrap=false` are server-fixed — never read them from the
  request body.
- Its exemption from the U4 mutation-token check is **exact-path and flag-gated**.
  `/v1/swap/*`, `/v1/rgb/*`, `/v1/audit/*` stay token-gated at all times.
- Spending rules live in `lab_core::demo` and must stay **pure** (no clock, no
  network, no filesystem) so they remain deterministically testable.
- Admission **reserves** the worst-case fee; failures release the slot but keep
  day/IP quota (anti retry-spam). Unknown balances **fail closed**.
- The full reservation must be durably committed before a demo session is
  created. Recovered reservations and unknown execution outcomes remain charged;
  corrupt/unreadable budget state blocks startup. Only a proven pre-execution
  failure may release a reservation. The maximum must include the BTC funding,
  claim/refund, and exit-sweep fees. Startup refuses under-reservation, and
  runtime settlement records actual fees without clamping them to the reserve.
- Custody (`lab_core::custody`) resolves keys from `RGBMVP_SECRET_DIR`
  (colon-separated) before local wallet dirs, and labd refuses to start on a
  public bind without it, or with a group/world-readable key.
- The refund watcher may only touch ids it minted (`demo-<epoch>-<seq>`) — never
  an operator's swap session.
- Refund completion is per leg: a first-chain refund leaves the session
  `refunding`; the watcher must keep retrying the other funded leg after its
  independent CSV maturity. Only both resolved legs may become `refunded`.
- **HTLC exits do NOT pay the funding wallet.** Claim and refund both pay one
  of four P2WPKH addresses whose private keys are hardened children of the
  custody-backed demo-exit seed. Public labels must never determine a signing
  key. Recovering that value requires the sweep —
  `lab_btc::sweep_all_demo_exits` (BTC) and
  `lab_chain::sweep_all_demo_exits_lq` (Liquid, explicit L-BTC only). Without it
  both `btc-alice` and `bob` drain every swap. Never claim "refunds return value
  to the funder".
- Never rotate the demo-exit seed while its sessions or exit outputs remain;
  disable T1, finish/refund legacy sessions, sweep all exits, then rotate.
- `RGBMVP_DATA_DIR` must be persistent in deployment. Follow
  `docs/T1_FEE_BUDGET_REMEDIATION.md`; never bypass a budget-recovery refusal.
- T1 rollback must target the live `rgbmvp-demo` service with
  `deploy/cloudrun-demo-freeze.yaml`. `deploy/cloudrun.yaml` names the separate
  `rgbmvp-public` service and cannot disable or replace T1.
- Per-IP quotas use `LABD_XFF_TRUSTED_HOPS`, the exact trusted right-edge XFF
  suffix length. Never restore the Boolean `LABD_TRUST_XFF`, select the
  rightmost entry, accept invalid IP tokens, or trust XFF on an unverified path.
- `/v1/demo/wallets` may publish aggregate testnet L-BTC only from the dedicated
  watch-only bundle (`RGBMVP_LIQUID_WATCH_BUNDLE`). Keep that mount outside
  `RGBMVP_SECRET_DIR`, reject spending private keys and address mismatches, and
  never feed its display/stale cache into T1 admission or signing paths.

## Local development

- Python 3.11+ for glue and project memory: `pip install -e ".[dev]"`.  
- Rust: `cargo build -p lab-cli` → `./target/debug/rgbmvp`.  
- Redis optional (project memory only).  
- P2 Simplicity demos: Docker Elements via `./scripts/regtest_simplicity.sh up`.

## Privacy

Do not commit `.env`, credentials, keys, customer data, or production payloads.  
Prefer `.env.example` for non-secret templates only.
