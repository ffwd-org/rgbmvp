# Liquid Testnet wallets (controlled, reusable)

All automated RGB/LWK tests should use the **named fixture wallets** below, not
one-off random wallets. Fixtures are public BIP39 phrases that are **only** for
Liquid Testnet. Never fund them with mainnet value.

## Where the P0 demo spend went

The RGB transfer broadcast spent `lab0`’s seal UTXO:

| Field | Value |
|-------|--------|
| **Txid** | [`2b1b2f045ab9797ff34dd919293fb5b67e4e123d5557d99fc90fd06cb36c2635`](https://blockstream.info/liquidtestnet/tx/2b1b2f045ab9797ff34dd919293fb5b67e4e123d5557d99fc90fd06cb36c2635) |
| **Input (seal)** | `206139b6…2b3a:0` — **100_000** L-BTC sats from faucet → lab0 |
| **vout[0]** | **500** sats → **tapret commitment** `tex1p6h4w3wnexfvdx…` (demo internal key, not a lab wallet; dust left on commitment) |
| **vout[1]** | **1_000** sats → lab0 **address index 1** (`tlq1qqd35zexc…`) — still under lab0 |
| **vout[2]** | **~98_466** sats → lab0 **change** (`…:2` on same tx) |
| **Fee** | **34** sats |

**Net:** almost all value stayed in `lab0` (change + bob self-pay). Only **500 + 34** sats left the controllable set (commitment dust + fee).  
Current lab0 balance after that demo: **~99_466** L-BTC sats + faucet side asset.

`lab0` was a **random** wallet created during Phase 0. Prefer **alice/bob/carol/maker** fixtures going forward.

---

## Fixture wallets (source of truth)

File: [`fixtures/testnet_wallets.json`](../fixtures/testnet_wallets.json)

| Role | Local name | Purpose |
|------|------------|---------|
| **alice** | `alice` | Issuer / RGB sender |
| **bob** | `bob` | Receiver / counterparty |
| **carol** | `carol` | Observer / third hop / verifier demos |
| **maker** | `maker` | P1 swap maker inventory (Liquid leg) |

Mnemonics live in that JSON (testnet-only public fixtures). On disk after bootstrap:

```text
.rgbmvp/wallets/<name>/mnemonic     # mode 600, gitignored
.rgbmvp/wallets/<name>/descriptor
.rgbmvp/wallets/<name>/meta.json    # includes role
.rgbmvp/wallet_registry.json        # addresses only (safe to share locally)
```

The public `/demo` board never receives these files. Bitcoin balances come
from Esplora. Confidential Liquid amounts require blinding material, so the
Cloud Run board uses a separately mounted bundle of watch-only descriptors
(public derivation keys + SLIP77) and returns only aggregate testnet L-BTC with
`balance_status` and `balance_as_of_epoch`. Fixture mnemonics are not uploaded
for this purpose. See `deploy/README.md` §5.3.1.

---

## Bootstrap (every machine / CI)

```bash
cargo build -p lab-cli
./target/debug/rgbmvp wallet bootstrap-testnet
# or force re-import:
./target/debug/rgbmvp wallet bootstrap-testnet --force

./target/debug/rgbmvp wallet list --sync
./target/debug/rgbmvp wallet registry
```

Fund **alice** first (largest balance), then rebalance:

```bash
# Show alice receive address → https://liquidtestnet.com/faucet
./target/debug/rgbmvp wallet address --name alice

# After faucet confirms:
./target/debug/rgbmvp wallet balance --name alice
./target/debug/rgbmvp wallet send --from alice --to bob --amount-sats 20000
./target/debug/rgbmvp wallet send --from alice --to carol --amount-sats 10000
./target/debug/rgbmvp wallet send --from alice --to maker --amount-sats 15000
./target/debug/rgbmvp wallet list --sync
```

Rebalance anytime the same way — wallets are **stable** across runs because
mnemonics are fixed.

---

## CLI cheat sheet

| Command | Use |
|---------|-----|
| `wallet bootstrap-testnet` | Import alice/bob/carol/maker from fixture |
| `wallet import --name X --mnemonic "…"` | One-off import |
| `wallet list [--sync]` | Names, roles, optional balances |
| `wallet registry` | Refresh address registry (no secrets) |
| `wallet address --name alice` | Receive address |
| `wallet balance --name bob` | Sync + balances |
| `wallet utxos --name alice` | Seal candidates |
| `wallet send --from alice --to bob --amount-sats N` | Rebalance |
| `wallet rebalance-demo` | Dry-run the threshold-based fixed Alice → Bob RGB demo rebalance |
| `wallet rebalance-demo --apply` | Broadcast only the bounded amount shown by a fresh plan |
| `wallet send --from alice --to-address tlq1… --amount-sats N` | External pay |

RGB commands should pass `--wallet alice` (or bob) explicitly:

```bash
./target/debug/rgbmvp rgb issue --wallet alice --ticker tRGB --supply 1000000
./target/debug/rgbmvp rgb transfer --contract 'rgb:…' --wallet alice \
  --bob-address "$(./target/debug/rgbmvp wallet address --name bob | jq -r .address)" \
  --broadcast
```

---

## Legacy `lab0`

Created with random mnemonic during early Phase 0. Still usable if funded.
For new work and automated tests, **do not depend on lab0** — use fixture roles
so every developer/CI shares the same address set after bootstrap.

Optional: empty lab0 after sweeping to alice:

```bash
# if lab0 still has L-BTC:
./target/debug/rgbmvp wallet send --from lab0 --to alice --amount-sats <almost-all>
```

(Leave fee headroom; if send fails, lower amount.)

---

## Bitcoin testnet (`btc-alice`)

| Field | Value |
|-------|--------|
| Name | `btc-alice` |
| Address | `tb1q85aadpqgzjgrgp69gf2ejf0883yx7s9wy85h4p` |
| WIF | `.env` → `BTC_TESTNET_WIF` only (not in git) |
| Fixture | [`fixtures/testnet_btc.json`](../fixtures/testnet_btc.json) |

```bash
./target/debug/rgbmvp btc import-env          # once per machine
./target/debug/rgbmvp btc status
./target/debug/rgbmvp btc balance --name btc-alice
./target/debug/rgbmvp btc utxos --name btc-alice
```

Funded UTXO (example): `84a6faf0…76a6:1` ≈ **189_026** sats.

## Security rules

1. Fixture mnemonics are **public** and **testnet-only**.
2. Never use them on Liquid mainnet or Bitcoin mainnet.
3. `.rgbmvp/` stays gitignored; do not commit real private keys or extra secrets.
4. `wallet_registry.json` is addresses-only — fine for logs; still not a backup of funds.
5. Project memory / Redis must never store mnemonics.

---

## Automated test pattern

```text
bootstrap-testnet (once per env)
    → faucet fund alice (manual or faucet API if available)
    → send alice→bob / alice→maker (scriptable)
    → rgb issue --wallet alice
    → rgb transfer --wallet alice --bob-address <bob>
    → rgb verify
    → wallet list --sync  (assert balances moved as expected)
```

Helper script: [`scripts/bootstrap_testnet_wallets.sh`](../scripts/bootstrap_testnet_wallets.sh).
