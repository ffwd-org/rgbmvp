# T1 — live swap evidence

> Historical security note (2026-08-13): these runs predate the secret-backed
> exit-key remediation. Their role labels and addresses describe the legacy
> `sha256(public_label)` scheme and must not be reused as a current runbook.
> Legacy outputs must be swept before enabling the remediated T1 profile.

Two live swaps, both complete:

| # | Session | Path | Result |
|---|---|---|---|
| 1 | `live-20260810-2325` | Operator CLI, step by step | ✅ `done` — §1–§5 |
| 2 | `demo-1786427249-0` | **Automated `POST /v1/demo/swap` driver** | ✅ `done` — §6 |

Run 2 is the more significant: it is the first swap completed by the **W1
orchestrator** with no human in the loop, and it exercised failure paths that a
step-by-step run cannot reach.

---

# Run 1 — first live swap (operator, CLI)

**Date:** 2026-08-10 · **Session:** `live-20260810-2325` · **Operator-run, CLI**
**Result:** ✅ complete — phase `done`, first attempt, no retries, no stuck funds.

Purpose: validate the assumptions behind
[TESTNET_PUBLIC_SWAPS.md](./TESTNET_PUBLIC_SWAPS.md) before any public exposure.
Two things were unproven until this run: **whether the fee defaults confirm**,
and **whether the cross-chain preimage extraction works end to end live**.

Parameters (value-only path, the T1 public default):

| | |
|---|---|
| Legs | 1,000 sats each side |
| `rgb_wrap` | `false` |
| `csv_delay` | 6 blocks |
| Wallets | `btc-alice` (BTC leg) ↔ `bob` (Liquid leg) |

---

## 1. Transactions

| Step | Chain | Txid | Block |
|---|---|---|---|
| Fund HTLC | BTC | [`ecbe50b8…0e97`](https://blockstream.info/testnet/tx/ecbe50b8ac1e20a1fc43c459b70a6f6bca926d3b88ac640ce227ce055c560e97) | 5,105,513 |
| Fund HTLC | Liquid | [`a04ce3b1…1b17`](https://blockstream.info/liquidtestnet/tx/a04ce3b11137171f9107a4f7d13e6a0f922204487b5a5561b6ca77ffdeb41b17) | 2,567,829 |
| Claim (preimage revealed) | Liquid | [`535c6ea9…8125`](https://blockstream.info/liquidtestnet/tx/535c6ea985a81b834e5d31459f29763bc133a00392b63e4fdfb84717485a8125) | 2,567,831 |
| Claim from witness | BTC | [`486d9cc1…b9d7`](https://blockstream.info/testnet/tx/486d9cc1ce9403d70c1f8b55081b999d80bc2d5c690e7d3491ba5361094cb9d7) | 5,105,513 |

`claim-btc --from-witness` recovered the preimage from the **Liquid claim's
witness stack**, not the local session file — the S3 extraction path, proven
against live chain data.

---

## 2. Measured fees (the primary purpose of this run)

| Tx | vsize | Fee | Rate |
|---|---|---|---|
| BTC fund | 152 vB | 800 sats | **5.25 sat/vB** |
| BTC claim | 138 vB | 500 sats | **3.63 sat/vB** |
| Liquid claim | 219 vB | 300 sats | — |
| (BTC sweep, 5-in/1-out) | 380 vB | 700 sats | **1.84 sat/vB** |

**Findings**

- The repo's long-standing defaults (800 fund / 500 claim) are **3.6–5.3× the
  1 sat/vB relay floor**. They work, with room to spare.
- vbyte prediction was accurate to within ~4% on every transaction (predicted
  153/133/382, actual 152/138/380), so the estimation method is sound — what
  was wrong earlier was the *risk appetite*, not the arithmetic.
- The earlier T1 draft's ~200 sats would have been ~1.3–1.5 sat/vB. The sweep
  relayed at 1.84, so 200 likely *would* have worked — but "likely relays" is
  not an acceptable margin for a transaction holding a live HTLC.
- **Not yet retuned.** A plausible target is ~450 fund / ~300 claim (≈3.0 and
  ≈2.2 sat/vB), which would cut a swap from 1,300 → ~750 sats and roughly double
  budget runway. Deferred until 2–3 more swaps of evidence exist; one sample is
  not a fee policy.

---

## 3. Value flow (confirms the §1a correction)

| Wallet / address | Before | After | Δ |
|---|---|---|---|
| `btc-alice` | 169,531 | 167,731 | **−1,800** |
| `bob-claimer` exit | 0 (swept) | 500 | **+500** |

Exactly the documented model: 1,000 leg + 800 fund fee leaves `btc-alice`;
1,300 burns to miners; 500 lands at the `bob-claimer` demo exit address.

**The 500 sats reappeared at the same address swept ~1 hour earlier**
([`53d5f8aa…73a7`](https://blockstream.info/testnet/tx/53d5f8aae6d8980423c86dff5c4a3f76f8436091d67a8fe5357b7cd0995973a7),
which recovered 35,924 sats of historical strandings). This is live confirmation
that the W5 sweep is **required, not optional** — without it every swap silently
bleeds the scarce BTC wallet.

---

## 4. Operational observations

- **Chained spend relayed and confirmed.** The BTC claim spent the funding
  output while the funding tx was still unconfirmed; both landed in the **same
  block (5,105,513)**. Good robustness signal for the W1 driver, which retries
  rather than requiring confirmations between steps.
- **Liquid is fast, Bitcoin is the pacing item.** Liquid funding confirmed in
  ~1 minute; the BTC leg waited on testnet blocks. The driver's 90-minute wall
  ceiling is adequate for a normal block cadence but remains the component most
  exposed to a slow testnet stretch — still untested under one.
- **`pick_largest_utxo` does not filter for confirmed UTXOs.** It selects purely
  by value, so it will happily chain off unconfirmed change. Harmless here, but
  worth knowing before an automated run.
- No refund path was exercised (the swap completed). The CSV refund and the
  automated W5 watcher remain **unproven live**.

---

## 5. What run 1 does and does not prove

**Proven**
- The value-only HTLC swap completes end to end on live testnet.
- Cross-chain preimage extraction from a Liquid witness works against real data.
- The fee defaults confirm comfortably.
- The demo-exit drain is real, and the sweep recovers it.

**Not proven by run 1** — the W1 automated driver (this was CLI, step by step),
Turnstile, the W5 refund watcher, or the CSV refund path. Run 2 closes the first
of those.

---

# Run 2 — automated driver (`POST /v1/demo/swap`)

**Date:** 2026-08-10 · **Session:** `demo-1786427249-0`
**Result:** ✅ complete — phase `done`, **no human in the loop after the POST**.

Local labd with `LABD_DEMO_SWAPS=1`. The request body was `{}` — no amounts, no
wallets, no fees; every protocol parameter came from the server.

## 6. Transactions

| Step | Chain | Txid |
|---|---|---|
| Fund HTLC | BTC | [`446653ca…9944`](https://blockstream.info/testnet/tx/446653cab1198d7a3172e63dd9f2d33a614ea3da5271c906f69d4d7d7f6e9944) |
| Fund HTLC | Liquid | [`ab04251a…6af2`](https://blockstream.info/liquidtestnet/tx/ab04251a61ec3480133af431db109d8ef364dd75fd66c9361ccd4315c5786af2) — block 2,567,850 |
| Claim (preimage) | Liquid | [`d5ad8453…4470`](https://blockstream.info/liquidtestnet/tx/d5ad84533afc9396479210fb15861edbeee4d74ff24c4a277a577d7d02734470) — block 2,567,851 |
| Claim from witness | BTC | [`a11c0880…d24f`](https://blockstream.info/testnet/tx/a11c0880f019f2547ec63040dbeea330052beaa7549e46f95edcb63db691d24f) |

## 7. The retry loop earned its keep — twice

The driver hit two genuine failures and recovered from both unattended:

```
demo: swap demo-1786427249-0 step claim_lq  pending/failed: no UTXO ≥ 999 on tex1q2tun6w…
demo: swap demo-1786427249-0 step claim_btc pending/failed: HTTP 404 for
      .../liquidtestnet/api/tx/d5ad8453…/hex
```

1. **`claim_lq`** — the Liquid funding tx had not confirmed yet.
2. **`claim_btc`** — a *propagation race*: the Liquid claim had broadcast, but was
   not yet fetchable from the Esplora API, so witness extraction 404'd.

The second is the more valuable finding. It is invisible in a step-by-step run
where a human naturally waits between commands, and it is exactly the class of
failure the retry loop exists for. Both resolved on the next 60 s poll.

## 8. Governor behaviour observed live

> Historical evidence: this run used the earlier 1,300-sat reservation and
> proved persistence only after successful completion. It did not exercise a
> crash while a reservation was in flight. The later 1,800-sat write-ahead
> model and its pending deployment drill are documented in
> [T1_FEE_BUDGET_REMEDIATION.md](./T1_FEE_BUDGET_REMEDIATION.md).

| Moment | `in_flight` | `reserved` | `spent` | `remaining_est` |
|---|---|---|---|---|
| Mid-swap | 1 | 1,300 | 0 | 20 |
| After completion | 0 | 0 | 1,300 | 20 |

The W2 worst-case reservation converted cleanly into actual spend, and the
in-flight slot released. **Completed-state W4 persistence was observed live**:
after completion
`.rgbmvp/demo_budget.json` held `fee_spent_sats: 1300`, so completed spend
survived a restart. This did not establish crash durability for an in-flight
reservation.

**Preimage redaction held throughout.** While the swap was mid-flight the public
`GET /v1/swap/{id}` returned `preimage_hex: null`, `preimage_redacted: true`.

## 9. Drain pattern, third confirmation

After run 2, `bob-claimer` held **1,000 sats** — 500 from each swap — and
`btc-alice` was down to 165,931. The value-flow model in
[TESTNET_PUBLIC_SWAPS.md §1a](./TESTNET_PUBLIC_SWAPS.md) has now reproduced on
three independent occasions (historical strandings, run 1, run 2). The W5 sweep
is load-bearing, not a nicety.

---

---

# Run 3 — refund path and W5 watcher (deliberate timeout)

**Date:** 2026-08-10 · **Session:** `demo-1786427775-99`
**Result:** ✅ watcher refunded and recycled **unattended**.

Designed to be the cheapest test that exercises the refund path: the BTC leg was
funded and the **Liquid leg deliberately left unfunded**, so the swap could only
ever exit via `refund_btc`. Funding
[`bdfc4033…dc65`](https://blockstream.info/testnet/tx/bdfc4033ba36d72b5502bc130340061e8f017a4198d4e5a41af63d42fda9dc65)
confirmed at block 5,105,516; `OP_CHECKSEQUENCEVERIFY` gates the refund on 6
confirmations, reached at 5,105,521.

labd was then started with `LABD_DEMO_SWEEP_INTERVAL_SECS=60`. The watcher fired
on its first cycle:

```
demo sweep: refunded bitcoin leg of demo-1786427775-99
demo recycle: swept 500 sats from bob-claimer -> btc-alice (33ac073e…2438)
demo sweep: SweepReport { scanned: 2, eligible: 1, refunded_btc: 1,
                          refunded_lq: 0, skipped_young: 0, errors: 0,
                          recycled_sats: 500 }
```

Phase `funded_btc` → **`refunded`** via
[`82240323…04ac`](https://blockstream.info/testnet/tx/822403239ab65b7755a3ecfe22c6b3309d250c17fa7b9f92105d450d833a04ac).

What that report line establishes:

| Field | Meaning |
|---|---|
| `scanned: 2, eligible: 1` | Examined both demo swaps; picked only the stuck one, left the completed one alone |
| `refunded_lq: 0` | Did not attempt a Liquid refund on a leg that was never funded |
| `errors: 0` | CSV maturity respected — no rejected broadcast |
| `recycled_sats: 500` | Refund **and** sweep chained in one cycle: value freed from the HTLC, then returned to `btc-alice` |

The id filter also held: only `demo-<epoch>-<seq>` sessions were touched.
`T1 demo budget restored: spent=1300sats` on startup confirmed W4 persistence
across a second real restart.

---

# Liquid-side exit sweep (added after run 3)

Extending exit inspection to Liquid revealed the same defect, larger:

```
liquid   alice-claimer   119,024 sats  (7 utxos)   <- stranded
liquid   bob-refund            0 sats
bitcoin  bob-claimer       1,000 sats              <- 500 per swap
bitcoin  alice-refund          0 sats
```

**119,024 L-BTC sats** had accumulated at `alice-claimer` across the repo's
entire P0/P1/S3 history — ~81% of `bob`'s working balance. Inspection was built
before the sweep deliberately, so the (more involved) Elements signing work was
justified by a measured number rather than a hypothesis.

`lab_chain::sweep_demo_exit_lq` consolidates confirmed explicit policy-asset
UTXOs into one output plus the Elements fee output, signing each input with the
BIP143 P2WPKH script code. Proven live:
[`4f91fa14…24a4`](https://blockstream.info/liquidtestnet/tx/4f91fa1457a176a5e5b42122ad0dd29854da1a5e324cedd4ea9ea2c306c124a4)
— **7 inputs → 2 outputs, 602 vB, 118,624 sats recovered** for a 400 sat fee.

Both chains are now wired into the W5 watcher (`recycled_sats` +
`recycled_lq_sats`) and available manually:

```bash
rgbmvp btc demo-exits                       # inspect both chains
rgbmvp btc sweep-demo --to btc-alice --include-liquid --lq-to bob
```

**Dust guard observed working:** the 500 sats the refund left at `alice-refund`
were *not* swept — `balance 500 does not cover fee 500 plus dust threshold`.
Sweeping would have destroyed value, so it correctly declined.

---

## 10. Status after three runs

**Proven live**
- Value-only HTLC swap, end to end (runs 1 and 2).
- Cross-chain preimage extraction from a Liquid witness.
- The **W1 automated driver**, including retry recovery from an unconfirmed-UTXO
  wait and an Esplora propagation race.
- W2 reservation → spend accounting; completed-state W4 persistence across two
  restarts (not an in-flight crash drill).
- The **W5 refund watcher**, CSV refund path, and recycle-after-refund.
- Demo-exit sweep on **both** chains, and its dust guard.
- Preimage redaction on the public view, mid-flight.

**Still unproven**
- **Turnstile against a real request.** Every run used
  `LABD_DEMO_TURNSTILE_REQUIRED=0` (no Cloudflare secret available locally). The
  fail-closed path is unit-tested; the pass path is not exercised live. This is
  the last functional gap before public exposure.
- Behaviour under a slow-block stretch, and a restart *during* a live swap.
- Confidential Liquid outputs are invisible to the sweep (explicit L-BTC only).
