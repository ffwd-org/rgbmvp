//! rgbmvp CLI — Phase 0 + P0 (network, LWK wallet, RGB issue/transfer/verify, labd).

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use lab_core::Config;
use lab_rgb::htlc;
use lab_rgb::storage::RgbStore;
use lab_rgb::swap::{self, SwapStore};
use lab_rgb::{
    issue_nia, plan_transfer, verify_against_witness, IssueRequest, DEMO_INTERNAL_XONLY_HEX,
};

mod demo_swap;
mod http_api;
mod labd_axum;
mod labd_legacy;
mod rgb_demo;

#[derive(Parser, Debug)]
#[command(
    name = "rgbmvp",
    about = "RGB Liquid Testnet Lab — CLI (P0: RGB issue/transfer/verify)",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    Net {
        #[command(subcommand)]
        cmd: NetCmd,
    },
    Wallet {
        #[command(subcommand)]
        cmd: WalletCmd,
    },
    /// RGB-on-Liquid (NIA) commands
    Rgb {
        #[command(subcommand)]
        cmd: RgbCmd,
    },
    /// Bitcoin testnet wallet (P1 twin-swap leg)
    Btc {
        #[command(subcommand)]
        cmd: BtcCmd,
    },
    /// BTC ↔ Liquid RGB atomic swap (HTLC)
    Swap {
        #[command(subcommand)]
        cmd: SwapCmd,
    },
    /// Serve labd HTTP API + web verifier (static)
    Serve {
        #[arg(long, env = "LABD_BIND")]
        bind: Option<String>,
    },
    /// P2 Simplicity covenants (C0: RGB-anchor seal)
    Covenant {
        #[command(subcommand)]
        cmd: CovenantCmd,
    },
    /// P2 C3 BFA (backed fungible asset) issue / mint plan / audit
    Bfa {
        #[command(subcommand)]
        cmd: BfaCmd,
    },
    ApiRoot,
}

#[derive(Subcommand, Debug)]
enum BfaCmd {
    /// Issue BFA genesis (prints contract_id + terms echo)
    Issue {
        #[arg(long, default_value = "LiquidRgbUSD")]
        name: String,
        #[arg(long, default_value = "LRUSD")]
        ticker: String,
        #[arg(long, default_value_t = 1_000_000)]
        max_supply: u64,
        /// Gate seal outpoint txid:vout
        #[arg(long)]
        gate_seal: String,
        /// Canonical terms: elements-backing:v1;vault=…;asset=…;rate=n/d
        #[arg(long)]
        backing: String,
        #[arg(long, default_value = "elements-regtest")]
        chain: String,
    },
    /// Plan a BFA mint transition (tapret address + opid JSON)
    MintPlan {
        #[arg(long, default_value = "LiquidRgbUSD")]
        name: String,
        #[arg(long, default_value = "LRUSD")]
        ticker: String,
        #[arg(long, default_value_t = 1_000_000)]
        max_supply: u64,
        #[arg(long)]
        backing: String,
        #[arg(long)]
        genesis_gate: String,
        #[arg(long)]
        gate_seal: String,
        #[arg(long)]
        mint: u64,
        #[arg(long)]
        recipient_seal: String,
        #[arg(long)]
        new_gate_seal: String,
        #[arg(long)]
        consume_opid: Option<String>,
        #[arg(long)]
        allowance: Option<u64>,
        #[arg(long, default_value = lab_rgb::DEMO_INTERNAL_XONLY_HEX)]
        internal_key: String,
        #[arg(long, default_value_t = 12_648_430)]
        entropy: u64,
        #[arg(long, default_value = "elements-regtest")]
        chain: String,
    },
    /// Full-history audit from JSON history file
    Audit {
        #[arg(long)]
        history: PathBuf,
        /// Fetch missing witness txs via regtest_simplicity.sh cli
        #[arg(long, default_value_t = true)]
        fetch_rpc: bool,
    },
    /// End-to-end C3 demo script
    Demo {
        #[arg(long, default_value = "scripts/demo_c3_bfa_audit.sh")]
        script: PathBuf,
    },
}

#[derive(Subcommand, Debug)]
enum CovenantCmd {
    /// Compile C0 rgb_anchor program → taproot address (leaf 0xbe)
    Address {
        /// SHA256(preimage) as 32-byte hex (param EXPECTED_HASH)
        #[arg(long)]
        hash: String,
        #[arg(long)]
        program: Option<PathBuf>,
    },
    /// End-to-end C0 demo on Elements Simplicity regtest
    Demo {
        #[arg(long, default_value = "scripts/demo_c0_simplicity.sh")]
        script: PathBuf,
    },
    /// End-to-end C1 mint-gate demo (vault + recursion)
    DemoC1 {
        #[arg(long, default_value = "scripts/demo_c1_mint_gate.sh")]
        script: PathBuf,
    },
    /// End-to-end C2 mint-gate burn demo (empty SPK + recursion)
    DemoC2 {
        #[arg(long, default_value = "scripts/demo_c2_mint_gate_burn.sh")]
        script: PathBuf,
    },
    /// End-to-end C4 time-locked stake demo
    DemoC4 {
        #[arg(long, default_value = "scripts/demo_c4_stake.sh")]
        script: PathBuf,
    },
}

#[derive(Subcommand, Debug)]
enum SwapCmd {
    /// Create swap session (preimage + dual HTLCs)
    Init {
        #[arg(long)]
        id: String,
        #[arg(long, default_value_t = 6)]
        csv_delay: u32,
        #[arg(long, default_value = "btc-alice")]
        alice_btc: String,
        #[arg(long, default_value = "bob")]
        bob_lq: String,
        #[arg(long)]
        btc_contract: Option<String>,
        #[arg(long)]
        lq_contract: Option<String>,
        /// S3: fund assigns RGB to HTLC seal; claim re-anchors + verify for done
        #[arg(long, default_value_t = false)]
        rgb_wrap: bool,
    },
    Status {
        #[arg(long)]
        id: String,
    },
    /// Fund BTC HTLC from alice BTC wallet
    FundBtc {
        #[arg(long)]
        id: String,
        #[arg(long, default_value_t = 10000)]
        amount_sats: u64,
        #[arg(long, default_value_t = 800)]
        fee_sats: u64,
        /// Override session; transfer RGB onto HTLC after value fund
        #[arg(long, default_value_t = false)]
        rgb_wrap: bool,
        #[arg(long, default_value_t = 546)]
        commitment_sats: u64,
        #[arg(long, default_value_t = 42)]
        entropy: u64,
    },
    /// Fund Liquid HTLC from bob LWK wallet
    FundLq {
        #[arg(long)]
        id: String,
        #[arg(long, default_value_t = 5000)]
        amount_sats: u64,
        #[arg(long, default_value_t = false)]
        rgb_wrap: bool,
        #[arg(long, default_value_t = 546)]
        commitment_sats: u64,
        #[arg(long, default_value_t = 42)]
        entropy: u64,
    },
    /// Alice claims Liquid HTLC (reveals preimage)
    ClaimLq {
        #[arg(long)]
        id: String,
        #[arg(long, default_value_t = 300)]
        fee_sats: u64,
        #[arg(long, default_value_t = 546)]
        commitment_sats: u64,
        #[arg(long, default_value_t = 43)]
        entropy: u64,
    },
    /// Bob claims BTC HTLC using preimage
    ClaimBtc {
        #[arg(long)]
        id: String,
        #[arg(long, default_value_t = 500)]
        fee_sats: u64,
        #[arg(long, default_value_t = 546)]
        commitment_sats: u64,
        #[arg(long, default_value_t = 44)]
        entropy: u64,
        /// Prefer preimage from Liquid claim witness (S3) instead of session file
        #[arg(long, default_value_t = false)]
        from_witness: bool,
    },
    /// Alice refunds BTC HTLC after CSV delay (no preimage)
    RefundBtc {
        #[arg(long)]
        id: String,
        #[arg(long, default_value_t = 500)]
        fee_sats: u64,
    },
    /// Bob refunds Liquid HTLC after CSV delay (no preimage)
    RefundLq {
        #[arg(long)]
        id: String,
        #[arg(long, default_value_t = 300)]
        fee_sats: u64,
    },
    HtlcAddress {
        #[arg(long)]
        hash_hex: String,
        #[arg(long, default_value_t = 6)]
        csv_delay: u32,
        #[arg(long, default_value = "bob-claimer")]
        claimer: String,
        #[arg(long, default_value = "alice-refund")]
        refund: String,
    },
    /// Extract claim preimage from a confirmed HTLC claim tx (S3 / R2)
    ExtractPreimage {
        #[arg(long)]
        chain: String,
        #[arg(long)]
        txid: String,
        /// Optional swap id: store note that preimage matched session hash
        #[arg(long)]
        id: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum BtcCmd {
    /// Probe Bitcoin testnet Esplora (+ btc-alice balance if imported)
    Status,
    /// Import BTC_TESTNET_WIF from .env as wallet btc-alice
    ImportEnv {
        #[arg(long)]
        force: bool,
    },
    /// Import a WIF into a named BTC wallet
    ImportWif {
        #[arg(long, default_value = "btc-alice")]
        name: String,
        #[arg(long)]
        wif: String,
        #[arg(long)]
        expect_address: Option<String>,
        #[arg(long)]
        force: bool,
    },
    Address {
        #[arg(long, default_value = "btc-alice")]
        name: String,
    },
    Balance {
        #[arg(long, default_value = "btc-alice")]
        name: String,
    },
    Utxos {
        #[arg(long, default_value = "btc-alice")]
        name: String,
    },
    /// Send sats from a named BTC wallet to an address (testnet only)
    Send {
        #[arg(long, default_value = "btc-alice")]
        from: String,
        #[arg(long)]
        to: String,
        #[arg(long)]
        amount_sats: u64,
        #[arg(long, default_value_t = 500)]
        fee_sats: u64,
    },
    /// Show the deterministic demo HTLC exit addresses and their balances
    DemoExits,
    /// T1/W5: sweep demo HTLC exit addresses back into a funding wallet
    SweepDemo {
        #[arg(long, default_value = "btc-alice")]
        to: String,
        #[arg(long, default_value_t = 500)]
        fee_sats: u64,
        /// Also sweep the Liquid-side exits into --lq-to
        #[arg(long, default_value_t = false)]
        include_liquid: bool,
        #[arg(long, default_value = "bob")]
        lq_to: String,
        #[arg(long, default_value_t = 300)]
        lq_fee_sats: u64,
    },
}

#[derive(Subcommand, Debug)]
enum NetCmd {
    Status,
}

#[derive(Subcommand, Debug)]
enum WalletCmd {
    /// Random ad-hoc wallet (prefer bootstrap-testnet for tests)
    Create {
        #[arg(long, default_value = "default")]
        name: String,
        #[arg(long)]
        force: bool,
    },
    /// Import a mnemonic into a named reusable wallet
    Import {
        #[arg(long)]
        name: String,
        #[arg(long)]
        mnemonic: Option<String>,
        #[arg(long)]
        mnemonic_file: Option<PathBuf>,
        #[arg(long)]
        role: Option<String>,
        #[arg(long)]
        force: bool,
    },
    /// Import alice/bob/carol/maker from fixtures/testnet_wallets.json
    BootstrapTestnet {
        #[arg(long, default_value = "fixtures/testnet_wallets.json")]
        fixture: PathBuf,
        #[arg(long)]
        force: bool,
    },
    /// List local wallets
    List {
        #[arg(long)]
        sync: bool,
    },
    /// Write address registry (no secrets)
    Registry,
    Address {
        #[arg(long, default_value = "default")]
        name: String,
        #[arg(long)]
        index: Option<u32>,
    },
    Balance {
        #[arg(long, default_value = "default")]
        name: String,
    },
    Utxos {
        #[arg(long, default_value = "default")]
        name: String,
    },
    /// Send L-BTC between wallets or to an address (testnet rebalance)
    Send {
        #[arg(long)]
        from: String,
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        to_address: Option<String>,
        #[arg(long)]
        amount_sats: u64,
    },
}

#[derive(Subcommand, Debug)]
enum RgbCmd {
    /// Issue RGB20 (NIA) on Liquid Testnet using a wallet L-BTC UTXO as seal
    Issue {
        /// liquid-testnet | bitcoin-testnet
        #[arg(long, default_value = "liquid-testnet")]
        chain: String,
        /// Liquid LWK name or BTC wallet name (btc-alice)
        #[arg(long, default_value = "alice")]
        wallet: String,
        #[arg(long, default_value = "Test RGB")]
        name: String,
        #[arg(long, default_value = "tRGB")]
        ticker: String,
        #[arg(long, default_value_t = 1_000_000)]
        supply: u64,
        /// Optional seal outpoint; default = largest UTXO on that chain
        #[arg(long)]
        seal: Option<String>,
    },
    /// Build receive invoice JSON (seal intent + contract)
    Invoice {
        #[arg(long)]
        contract: String,
        #[arg(long, default_value = "bob")]
        wallet: String,
        #[arg(long, default_value_t = 1000)]
        amount: u64,
    },
    /// Build transfer plan (MPC + tapret); optionally broadcast commitment tx
    Transfer {
        #[arg(long, default_value = "liquid-testnet")]
        chain: String,
        #[arg(long)]
        contract: String,
        #[arg(long, default_value = "alice")]
        wallet: String,
        #[arg(long, default_value_t = 600_000)]
        amount: u64,
        /// Optional Bob L-BTC address (Liquid confidential/unconfidential)
        #[arg(long)]
        bob_address: Option<String>,
        #[arg(long, default_value_t = 1000)]
        bob_sats: u64,
        #[arg(long, default_value_t = 500)]
        commitment_sats: u64,
        /// Broadcast the seal-closing Liquid tx via LWK
        #[arg(long)]
        broadcast: bool,
        #[arg(long, default_value_t = 42)]
        entropy: u64,
    },
    /// Verify transfer plan against a Liquid witness txid (Esplora)
    Verify {
        #[arg(long)]
        plan: String,
        #[arg(long)]
        txid: String,
        #[arg(long)]
        proof_id: Option<String>,
        /// Override chain for witness fetch: liquid-testnet | bitcoin-testnet
        #[arg(long)]
        chain: Option<String>,
    },
    /// Store an opaque consignment blob (file) for exchange
    Consign {
        #[command(subcommand)]
        cmd: ConsignCmd,
    },
}

#[derive(Subcommand, Debug)]
enum ConsignCmd {
    Put {
        #[arg(long)]
        id: String,
        #[arg(long)]
        file: PathBuf,
    },
    Get {
        #[arg(long)]
        id: String,
        #[arg(long)]
        out: PathBuf,
    },
}

fn main() {
    if let Err(e) = run() {
        let err = serde_json::json!({
            "status": "error",
            "error": format!("{e:#}"),
        });
        eprintln!(
            "{}",
            serde_json::to_string_pretty(&err).unwrap_or_else(|_| e.to_string())
        );
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let cfg = Config::load()?;
    cfg.ensure_dirs()?;
    let store = RgbStore::new(&cfg.data_dir);
    store.ensure()?;

    match cli.command {
        Commands::Net {
            cmd: NetCmd::Status,
        } => {
            let report = lab_chain::network_status(&cfg)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&lab_api::health_json(&report))?
            );
            if report.status != "ready" {
                std::process::exit(2);
            }
        }
        Commands::Wallet { cmd } => match cmd {
            WalletCmd::Create { name, force } => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&lab_chain::wallet_create(&cfg, &name, force)?)?
                );
            }
            WalletCmd::Import {
                name,
                mnemonic,
                mnemonic_file,
                role,
                force,
            } => {
                let phrase = if let Some(p) = mnemonic {
                    p
                } else if let Some(f) = mnemonic_file {
                    std::fs::read_to_string(&f)?.trim().to_string()
                } else {
                    anyhow::bail!("provide --mnemonic or --mnemonic-file");
                };
                println!(
                    "{}",
                    serde_json::to_string_pretty(&lab_chain::wallet_import(
                        &cfg,
                        &name,
                        &phrase,
                        force,
                        role.as_deref()
                    )?)?
                );
            }
            WalletCmd::BootstrapTestnet { fixture, force } => {
                let path = if fixture.exists() {
                    fixture
                } else {
                    PathBuf::from("fixtures/testnet_wallets.json")
                };
                println!(
                    "{}",
                    serde_json::to_string_pretty(&lab_chain::wallet_bootstrap_fixtures(
                        &cfg, &path, force
                    )?)?
                );
            }
            WalletCmd::List { sync } => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&lab_chain::wallet_list(&cfg, sync)?)?
                );
            }
            WalletCmd::Registry => {
                let p = lab_chain::write_wallet_registry(&cfg)?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "status": "ok",
                        "registry": p.display().to_string(),
                    }))?
                );
            }
            WalletCmd::Address { name, index } => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&lab_chain::wallet_address(&cfg, &name, index)?)?
                );
            }
            WalletCmd::Balance { name } => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&lab_chain::wallet_balance(&cfg, &name)?)?
                );
            }
            WalletCmd::Utxos { name } => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&lab_chain::wallet_utxos(&cfg, &name)?)?
                );
            }
            WalletCmd::Send {
                from,
                to,
                to_address,
                amount_sats,
            } => {
                let dest = if let Some(addr) = to_address {
                    addr
                } else if let Some(w) = to {
                    lab_chain::wallet_receive_address(&cfg, &w)?
                } else {
                    anyhow::bail!("provide --to <wallet> or --to-address");
                };
                println!(
                    "{}",
                    serde_json::to_string_pretty(&lab_chain::send_lbtc(
                        &cfg,
                        &from,
                        &dest,
                        amount_sats
                    )?)?
                );
            }
        },
        Commands::Rgb { cmd } => match cmd {
            RgbCmd::Issue {
                chain,
                wallet,
                name,
                ticker,
                supply,
                seal,
            } => {
                let seal = match seal {
                    Some(s) => s,
                    None if chain.starts_with("bitcoin")
                        || chain == "testnet"
                        || chain == "testnet3" =>
                    {
                        let btc = lab_btc::BtcConfig::from_env();
                        lab_btc::pick_largest_utxo(&cfg, &btc, &wallet)?.outpoint
                    }
                    None => lab_chain::pick_lbtc_seal(&cfg, &wallet)?.outpoint,
                };
                let issue = issue_nia(&IssueRequest {
                    name,
                    ticker,
                    supply,
                    seal: seal.clone(),
                    chain: chain.clone(),
                })?;
                let path = store.save_issue(&issue)?;
                let out = serde_json::json!({
                    "status": "issued",
                    "issue": issue,
                    "stored": path.display().to_string(),
                    "note": "Genesis is off-chain; seal UTXO must be closed by a transfer witness tx."
                });
                println!("{}", serde_json::to_string_pretty(&out)?);
            }
            RgbCmd::Invoice {
                contract,
                wallet,
                amount,
            } => {
                let issue = store.load_issue(&contract).with_context(|| {
                    format!("contract not found in store: {contract} (run rgb issue first)")
                })?;
                let addr = lab_chain::wallet_address(&cfg, &wallet, None)?;
                let inv = serde_json::json!({
                    "type": "rgbmvp-invoice-v1",
                    "network": "liquid-testnet",
                    "contract_id": issue.contract_id,
                    "ticker": issue.ticker,
                    "amount": amount,
                    "receive_address": addr.address,
                    "note": "P0 invoice: Bob should fund/use a seal UTXO; amount is RGB units."
                });
                println!("{}", serde_json::to_string_pretty(&inv)?);
            }
            RgbCmd::Transfer {
                chain,
                contract,
                wallet,
                amount,
                bob_address,
                bob_sats,
                commitment_sats,
                broadcast,
                entropy,
            } => {
                let issue = store.load_issue(&contract)?;
                let chain = if chain != "liquid-testnet" {
                    chain
                } else if issue.chain_net.starts_with("bitcoin") {
                    issue.chain_net.clone()
                } else {
                    chain
                };
                let plan = plan_transfer(
                    &issue.contract_id,
                    issue.supply,
                    amount,
                    &issue.seal,
                    &format!("bob-{}", issue.contract_id),
                    &format!("change-{}", issue.contract_id),
                    DEMO_INTERNAL_XONLY_HEX,
                    entropy,
                    &issue.ticker,
                    &chain,
                )?;
                let plan_id = format!(
                    "{}-{}",
                    issue.ticker,
                    &plan.bundle_id_hex[..16.min(plan.bundle_id_hex.len())]
                );
                let plan_path = store.save_transfer(&plan_id, &plan)?;

                let mut out = serde_json::json!({
                    "status": "planned",
                    "plan_id": plan_id,
                    "plan_path": plan_path.display().to_string(),
                    "plan": plan,
                });

                if broadcast {
                    let is_btc = chain.starts_with("bitcoin") || chain.contains("testnet3");
                    let bc_val = if is_btc {
                        let btc = lab_btc::BtcConfig::from_env();
                        // resolve seal value from utxo list
                        let utxos = lab_btc::utxos(&cfg, &btc, &wallet)?;
                        let seal_val = utxos
                            .iter()
                            .find(|u| u.outpoint == issue.seal)
                            .map(|u| u.value_sats)
                            .context("seal UTXO not found in btc wallet")?;
                        let fee = 800u64;
                        let bc = lab_btc::broadcast_commitment_tx(
                            &cfg,
                            &btc,
                            &wallet,
                            &issue.seal,
                            seal_val,
                            &plan.tapret_address,
                            commitment_sats,
                            fee,
                        )?;
                        serde_json::to_value(bc)?
                    } else {
                        let bc = lab_chain::broadcast_commitment_tx(
                            &cfg,
                            &wallet,
                            &issue.seal,
                            &plan.tapret_address,
                            bob_address.as_deref(),
                            commitment_sats,
                            bob_sats,
                        )?;
                        serde_json::to_value(bc)?
                    };
                    let meta = serde_json::json!({
                        "plan": plan,
                        "broadcast": bc_val,
                    });
                    let meta_path = cfg
                        .data_dir
                        .join("rgb/transfers")
                        .join(format!("{plan_id}.broadcast.json"));
                    fs::write(&meta_path, serde_json::to_vec_pretty(&meta)?)?;
                    out["status"] = serde_json::json!("broadcast");
                    out["broadcast"] = bc_val;
                    out["broadcast_meta"] = serde_json::json!(meta_path.display().to_string());
                }
                println!("{}", serde_json::to_string_pretty(&out)?);
            }
            RgbCmd::Verify {
                plan,
                txid,
                proof_id,
                chain,
            } => {
                let plan_obj = store.load_transfer(&plan)?;
                let chain = chain.unwrap_or_else(|| plan_obj.chain_net.clone());
                let (witness, explorer) = if chain.starts_with("bitcoin") {
                    let btc = lab_btc::BtcConfig::from_env();
                    (
                        lab_btc::fetch_witness_for_rgb(&btc, &txid)?,
                        btc.explorer_base.clone(),
                    )
                } else {
                    let api = lab_chain::esplora_api_base(&cfg);
                    (
                        lab_chain::fetch_witness_esplora(&api, &txid)?,
                        cfg.explorer_base.clone(),
                    )
                };
                let result = verify_against_witness(&plan_obj, &witness, &explorer)?;
                let pid =
                    proof_id.unwrap_or_else(|| format!("proof-{}", &txid[..16.min(txid.len())]));
                let path = store.save_proof(&pid, &result)?;
                let out = serde_json::json!({
                    "proof_id": pid,
                    "stored": path.display().to_string(),
                    "result": result,
                });
                println!("{}", serde_json::to_string_pretty(&out)?);
                if result.status != "valid" {
                    std::process::exit(2);
                }
            }
            RgbCmd::Consign { cmd } => match cmd {
                ConsignCmd::Put { id, file } => {
                    let bytes =
                        fs::read(&file).with_context(|| format!("read {}", file.display()))?;
                    let path = store.save_consignment_blob(&id, &bytes)?;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "status": "stored",
                            "id": id,
                            "bytes": bytes.len(),
                            "path": path.display().to_string(),
                        }))?
                    );
                }
                ConsignCmd::Get { id, out } => {
                    let src = store.root().join("consignments").join(format!("{id}.bin"));
                    fs::copy(&src, &out)
                        .with_context(|| format!("copy {} -> {}", src.display(), out.display()))?;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "status": "exported",
                            "id": id,
                            "out": out.display().to_string(),
                        }))?
                    );
                }
            },
        },
        Commands::Swap { cmd } => {
            let store = SwapStore::new(&cfg.data_dir);
            match cmd {
                SwapCmd::Init {
                    id,
                    csv_delay,
                    alice_btc,
                    bob_lq,
                    btc_contract,
                    lq_contract,
                    rgb_wrap,
                } => {
                    let keyring = htlc::DemoKeyring::new(cfg.demo_exit_seed()?)?;
                    let session = swap::init_swap(
                        &keyring,
                        &id,
                        csv_delay,
                        &alice_btc,
                        &bob_lq,
                        btc_contract,
                        lq_contract,
                        rgb_wrap,
                    )?;
                    let path = store.save(&session)?;
                    let mut next = vec![
                        "rgbmvp swap fund-btc --id …".to_string(),
                        "rgbmvp swap fund-lq --id …".to_string(),
                        "rgbmvp swap claim-lq --id …  # Alice reveals preimage".to_string(),
                        "rgbmvp swap claim-btc --id … # Bob claims BTC".to_string(),
                    ];
                    if rgb_wrap {
                        next = vec![
                            "rgbmvp swap fund-btc --id … --rgb-wrap  # value + RGB→HTLC seal"
                                .into(),
                            "rgbmvp swap fund-lq --id … --rgb-wrap".into(),
                            "rgbmvp swap claim-lq --id …  # preimage + re-anchor + verify".into(),
                            "rgbmvp swap claim-btc --id … --from-witness".into(),
                            "rgbmvp swap extract-preimage --chain liquid --txid <lq_claim>".into(),
                        ];
                    }
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "status": "created",
                            "stored": path.display().to_string(),
                            "rgb_wrap": session.rgb_wrap,
                            "version": session.version,
                            "session": session,
                            "next": next,
                        }))?
                    );
                }
                SwapCmd::Status { id } => {
                    let s = store.load(&id)?;
                    println!("{}", serde_json::to_string_pretty(&s)?);
                }
                SwapCmd::FundBtc {
                    id,
                    amount_sats,
                    fee_sats,
                    rgb_wrap,
                    commitment_sats,
                    entropy,
                } => {
                    let svc = lab_api::SwapService::new(&cfg.data_dir);
                    let mut s = store.load(&id)?;
                    let do_wrap = rgb_wrap || s.rgb_wrap;
                    let btc = lab_btc::BtcConfig::from_env();
                    let bc = lab_btc::fund_address(
                        &cfg,
                        &btc,
                        &s.alice_btc_wallet,
                        &s.htlc_btc.address_btc,
                        amount_sats,
                        fee_sats,
                    )?;
                    s.btc_fund_txid = Some(bc.txid.clone());
                    s.btc_fund_vout = Some(0);
                    s.btc_fund_sats = Some(amount_sats);
                    let mut rgb_meta = serde_json::Value::Null;
                    if do_wrap {
                        rgb_meta =
                            svc.fund_wrap_btc(&cfg, &btc, &mut s, commitment_sats, entropy)?;
                    }
                    svc.recompute_and_save(&mut s)?;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "status": "funded_btc",
                            "phase": s.phase,
                            "rgb_wrap": do_wrap,
                            "broadcast": bc,
                            "htlc_address": s.htlc_btc.address_btc,
                            "rgb": rgb_meta,
                        }))?
                    );
                }
                SwapCmd::FundLq {
                    id,
                    amount_sats,
                    rgb_wrap,
                    commitment_sats,
                    entropy,
                } => {
                    let mut s = store.load(&id)?;
                    let do_wrap = rgb_wrap || s.rgb_wrap;
                    // Prefer an existing HTLC UTXO (retry after wrap failure, or manual fund).
                    // Critical for S3: value fund must not re-spend the RGB issue seal.
                    let existing = lab_chain::find_address_utxo(
                        &cfg,
                        &s.htlc_lq.address_liquid_unconf,
                        amount_sats.saturating_sub(1),
                    )
                    .ok();
                    let (bc_val, ftxid, fvout, fval, reused) = if let Some((tx, vo, va)) = existing
                    {
                        (
                            serde_json::json!({
                                "txid": tx,
                                "note": "reused existing HTLC UTXO (no new send_lbtc)",
                                "reused": true,
                            }),
                            tx,
                            vo,
                            va,
                            true,
                        )
                    } else {
                        // When rgb_wrap, refuse if issue seal is the only large UTXO
                        // (would be spent by send and break wrap). Best-effort check.
                        if do_wrap {
                            if let Some(cid) = s.lq_contract_id.as_ref() {
                                let rgb_store = RgbStore::new(&cfg.data_dir);
                                if let Ok(issue) = rgb_store.load_issue(cid) {
                                    let utxos = lab_chain::wallet_utxos(&cfg, &s.bob_lq_wallet)?;
                                    let large: Vec<_> = utxos
                                        .iter()
                                        .filter(|u| u.value >= amount_sats.saturating_add(500))
                                        .collect();
                                    if large.len() == 1 && large[0].outpoint == issue.seal {
                                        anyhow::bail!(
                                            "S3 fund-lq: only spendable UTXO is the RGB issue seal {}. \
                                             Split funds first (wallet send to self) or re-issue on a \
                                             UTXO that will not be used for HTLC value.",
                                            issue.seal
                                        );
                                    }
                                }
                            }
                        }
                        let bc = lab_chain::send_lbtc(
                            &cfg,
                            &s.bob_lq_wallet,
                            &s.htlc_lq.address_liquid_unconf,
                            amount_sats,
                        )?;
                        let (tx, vo, va) = lab_chain::find_address_utxo(
                            &cfg,
                            &s.htlc_lq.address_liquid_unconf,
                            amount_sats.saturating_sub(1),
                        )
                        .unwrap_or((bc.txid.clone(), 0, amount_sats));
                        (serde_json::to_value(&bc)?, tx, vo, va, false)
                    };
                    s.lq_fund_txid = Some(ftxid);
                    s.lq_fund_vout = Some(fvout);
                    s.lq_fund_sats = Some(fval);
                    // Persist value fund even if RGB wrap fails later.
                    swap::recompute_phase(&mut s);
                    store.save(&s)?;
                    let mut rgb_meta = serde_json::Value::Null;
                    if do_wrap {
                        let svc = lab_api::SwapService::new(&cfg.data_dir);
                        match svc.fund_wrap_lq(&cfg, &mut s, commitment_sats, entropy) {
                            Ok(m) => {
                                rgb_meta = m;
                                svc.recompute_and_save(&mut s)?;
                            }
                            Err(e) => {
                                store.save(&s)?;
                                anyhow::bail!(
                                    "LQ value funded (txid {}) but RGB wrap failed: {e}. \
                                     Fix seal (re-issue on unspent UTXO) then re-run fund-lq --rgb-wrap \
                                     (HTLC UTXO will be reused).",
                                    s.lq_fund_txid.as_deref().unwrap_or("?")
                                );
                            }
                        }
                    }
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "status": "funded_lq",
                            "phase": s.phase,
                            "rgb_wrap": do_wrap,
                            "reused_htlc_utxo": reused,
                            "broadcast": bc_val,
                            "htlc_address": s.htlc_lq.address_liquid_unconf,
                            "htlc_seal": format!("{}:{}", s.lq_fund_txid.as_deref().unwrap_or(""), s.lq_fund_vout.unwrap_or(0)),
                            "rgb": rgb_meta,
                        }))?
                    );
                }
                SwapCmd::ClaimLq {
                    id,
                    fee_sats,
                    commitment_sats,
                    entropy,
                } => {
                    let svc = lab_api::SwapService::new(&cfg.data_dir);
                    let mut s = store.load(&id)?;
                    let mut out = svc.claim_lq(&cfg, &mut s, fee_sats, commitment_sats, entropy)?;
                    svc.recompute_and_save(&mut s)?;
                    out["phase"] = serde_json::json!(s.phase);
                    println!("{}", serde_json::to_string_pretty(&out)?);
                }
                SwapCmd::ClaimBtc {
                    id,
                    fee_sats,
                    commitment_sats,
                    entropy,
                    from_witness,
                } => {
                    let svc = lab_api::SwapService::new(&cfg.data_dir);
                    let mut s = store.load(&id)?;
                    let mut out = svc.claim_btc(
                        &cfg,
                        &mut s,
                        fee_sats,
                        commitment_sats,
                        entropy,
                        from_witness,
                    )?;
                    svc.recompute_and_save(&mut s)?;
                    out["phase"] = serde_json::json!(s.phase);
                    println!("{}", serde_json::to_string_pretty(&out)?);
                }
                SwapCmd::RefundBtc { id, fee_sats } => {
                    let mut s = store.load(&id)?;
                    if s.btc_claim_txid.is_some() {
                        anyhow::bail!("BTC already claimed; cannot refund");
                    }
                    let btc = lab_btc::BtcConfig::from_env();
                    let amount = s.btc_fund_sats.context("btc not funded")?;
                    let utxo = lab_btc::find_htlc_utxo(
                        &btc,
                        &s.htlc_btc.address_btc,
                        amount.saturating_sub(1),
                    )?;
                    let keyring = htlc::DemoKeyring::new(cfg.demo_exit_seed()?)?;
                    let (refund_sk, _) =
                        keyring.derive_for_session(&s.htlc_btc, &s.htlc_btc.refund_label)?;
                    let ws = hex::decode(&s.htlc_btc.witness_script_hex)?;
                    use bitcoin::key::{CompressedPublicKey, Secp256k1};
                    use bitcoin::{Address, Network};
                    let secp = Secp256k1::new();
                    let pk = bitcoin::secp256k1::PublicKey::from_secret_key(&secp, &refund_sk);
                    let compressed = CompressedPublicKey(pk);
                    let dest = Address::p2wpkh(&compressed, Network::Testnet);
                    let out_sats = utxo.value_sats.saturating_sub(fee_sats);
                    let raw = htlc::build_htlc_spend_btc(
                        &utxo.txid,
                        utxo.vout,
                        utxo.value_sats,
                        out_sats,
                        dest.script_pubkey().as_bytes(),
                        &ws,
                        htlc::HtlcSpend::Refund,
                        s.csv_delay,
                        &refund_sk,
                    )?;
                    let txid = lab_btc::broadcast_raw(&btc, &raw)?;
                    s.notes.push(format!("btc_refund_txid={txid}"));
                    s.phase = lab_rgb::swap::SwapPhase::Refunded;
                    store.save(&s)?;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "status": "refunded_btc",
                            "phase": s.phase,
                            "txid": txid,
                            "explorer": format!("{}/tx/{}", btc.explorer_base, txid),
                            "dest": dest.to_string(),
                            "note": "Requires CSV maturity (nSequence = csv_delay blocks) since fund.",
                        }))?
                    );
                }
                SwapCmd::RefundLq { id, fee_sats } => {
                    let mut s = store.load(&id)?;
                    if s.lq_claim_txid.is_some() {
                        anyhow::bail!("Liquid already claimed; cannot refund");
                    }
                    let amount = s.lq_fund_sats.context("lq not funded")?;
                    let (txid, vout, value) = lab_chain::find_address_utxo(
                        &cfg,
                        &s.htlc_lq.address_liquid_unconf,
                        amount.saturating_sub(1),
                    )?;
                    let keyring = htlc::DemoKeyring::new(cfg.demo_exit_seed()?)?;
                    let (refund_sk, _) =
                        keyring.derive_for_session(&s.htlc_lq, &s.htlc_lq.refund_label)?;
                    let ws = hex::decode(&s.htlc_lq.witness_script_hex)?;
                    use bitcoin::key::{CompressedPublicKey, Secp256k1};
                    use bitcoin::{Address, Network};
                    let secp = Secp256k1::new();
                    let pk = bitcoin::secp256k1::PublicKey::from_secret_key(&secp, &refund_sk);
                    let compressed = CompressedPublicKey(pk);
                    let dest = Address::p2wpkh(&compressed, Network::Testnet);
                    let policy = "144c654344aa716d6f3abcc1ca90e5641e4e2a7f633bc09fe3baf64585819a49";
                    let out_sats = value.saturating_sub(fee_sats);
                    let raw = htlc::build_htlc_spend_liquid(
                        &txid,
                        vout,
                        value,
                        out_sats,
                        fee_sats,
                        dest.script_pubkey().as_bytes(),
                        policy,
                        &ws,
                        htlc::HtlcSpend::Refund,
                        s.csv_delay,
                        &refund_sk,
                    )?;
                    let claim_txid = lab_chain::broadcast_raw_hex(&cfg, &raw)?;
                    s.notes.push(format!("lq_refund_txid={claim_txid}"));
                    s.phase = lab_rgb::swap::SwapPhase::Refunded;
                    store.save(&s)?;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "status": "refunded_lq",
                            "phase": s.phase,
                            "txid": claim_txid,
                            "explorer": format!("{}/tx/{}", cfg.explorer_base, claim_txid),
                            "note": "Requires CSV maturity since fund.",
                        }))?
                    );
                }
                SwapCmd::HtlcAddress {
                    hash_hex,
                    csv_delay,
                    claimer,
                    refund,
                } => {
                    let mut h = [0u8; 32];
                    let b = hex::decode(hash_hex.trim())?;
                    if b.len() != 32 {
                        anyhow::bail!("hash must be 32 bytes");
                    }
                    h.copy_from_slice(&b);
                    let keyring = htlc::DemoKeyring::new(cfg.demo_exit_seed()?)?;
                    let info =
                        htlc::build_htlc_addresses(&keyring, &h, &claimer, &refund, csv_delay)?;
                    println!("{}", serde_json::to_string_pretty(&info)?);
                }
                SwapCmd::ExtractPreimage { chain, txid, id } => {
                    let svc = lab_api::SwapService::new(&cfg.data_dir);
                    let mut out = svc.extract_preimage(&cfg, &chain, &txid, id.as_deref())?;
                    if let Some(obj) = out.as_object_mut() {
                        obj.insert("status".into(), serde_json::json!("ok"));
                        obj.insert(
                            "note".into(),
                            serde_json::json!(
                                "Preimage is public once claim is mined; still never log in labd GET."
                            ),
                        );
                    }
                    println!("{}", serde_json::to_string_pretty(&out)?);
                }
            }
        }
        Commands::Btc { cmd } => {
            let btc = lab_btc::BtcConfig::from_env();
            match cmd {
                BtcCmd::Status => {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&lab_btc::network_status(&cfg, &btc)?)?
                    );
                }
                BtcCmd::ImportEnv { force } => {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&lab_btc::import_from_env(
                            &cfg, &btc, force
                        )?)?
                    );
                }
                BtcCmd::ImportWif {
                    name,
                    wif,
                    expect_address,
                    force,
                } => {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&lab_btc::import_wif(
                            &cfg,
                            &btc,
                            &name,
                            &wif,
                            expect_address.as_deref(),
                            force
                        )?)?
                    );
                }
                BtcCmd::Address { name } => {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&lab_btc::load_wallet_address(
                            &cfg, &btc, &name
                        )?)?
                    );
                }
                BtcCmd::Balance { name } => {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&lab_btc::balance(&cfg, &btc, &name)?)?
                    );
                }
                BtcCmd::DemoExits => {
                    let keyring = htlc::DemoKeyring::new(cfg.demo_exit_seed()?)?;
                    let mut out = Vec::new();
                    // Liquid-side exits: inspection only (no sweep implemented).
                    for label in lab_chain::LQ_DEMO_EXIT_LABELS {
                        match lab_chain::demo_exit_balance_lq(&cfg, label) {
                            Ok(b) => out.push(serde_json::json!({
                                "chain": "liquid",
                                "label": b.label,
                                "address": b.address,
                                "utxo_count": b.utxo_count,
                                "balance_sats": b.balance_sats,
                                "sweep": "NOT IMPLEMENTED (inspection only)",
                            })),
                            Err(e) => out.push(serde_json::json!({
                                "chain": "liquid", "label": label, "error": e.to_string(),
                            })),
                        }
                    }
                    for label in lab_btc::BTC_DEMO_EXIT_LABELS {
                        let (_, addr) = lab_btc::demo_exit_address(&btc, &keyring, label)?;
                        let a = addr.to_string();
                        let utxos = lab_btc::address_utxos(&btc, &a).unwrap_or_default();
                        out.push(serde_json::json!({
                            "chain": "bitcoin",
                            "label": label,
                            "address": a,
                            "utxo_count": utxos.len(),
                            "balance_sats": utxos.iter().map(|u| u.value_sats).sum::<u64>(),
                            "confirmed_sats": utxos.iter().filter(|u| u.confirmed)
                                .map(|u| u.value_sats).sum::<u64>(),
                            "explorer": format!("{}/address/{}", btc.explorer_base, a),
                        }));
                    }
                    println!("{}", serde_json::to_string_pretty(&out)?);
                }
                BtcCmd::SweepDemo {
                    to,
                    fee_sats,
                    include_liquid,
                    lq_to,
                    lq_fee_sats,
                } => {
                    let btc_res = lab_btc::sweep_all_demo_exits(&cfg, &btc, &to, fee_sats)?;
                    let mut out = serde_json::json!({ "bitcoin": btc_res });
                    if include_liquid {
                        let lq_res = lab_chain::sweep_all_demo_exits_lq(&cfg, &lq_to, lq_fee_sats)?;
                        out["liquid"] = serde_json::to_value(&lq_res)?;
                    }
                    println!("{}", serde_json::to_string_pretty(&out)?);
                }
                BtcCmd::Utxos { name } => {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&lab_btc::utxos(&cfg, &btc, &name)?)?
                    );
                }
                BtcCmd::Send {
                    from,
                    to,
                    amount_sats,
                    fee_sats,
                } => {
                    let bc = lab_btc::fund_address(&cfg, &btc, &from, &to, amount_sats, fee_sats)?;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "status": "sent",
                            "from": from,
                            "to": to,
                            "amount_sats": amount_sats,
                            "fee_sats": fee_sats,
                            "broadcast": bc,
                        }))?
                    );
                }
            }
        }
        Commands::Serve { bind } => {
            let bind = bind.unwrap_or_else(|| cfg.labd_bind.clone());
            // U5 Axum is default; LABD_HTTP=legacy restores handwritten TCP server.
            let backend = std::env::var("LABD_HTTP").unwrap_or_else(|_| "axum".into());
            if backend.eq_ignore_ascii_case("legacy") {
                anyhow::ensure!(
                    !rgb_demo::policy_from_env().enabled,
                    "LABD_RGB_DEMO requires the default Axum HTTP backend"
                );
                labd_legacy::serve_labd_legacy(&cfg, &bind)?;
            } else {
                labd_axum::serve(&cfg, &bind)?;
            }
        }
        Commands::Covenant { cmd } => match cmd {
            CovenantCmd::Address { hash, program } => {
                let path = program.unwrap_or_else(lab_simplicity::resolve_rgb_anchor_program);
                let src = fs::read_to_string(&path)
                    .with_context(|| format!("read {}", path.display()))?;
                let args = lab_simplicity::args_expected_hash_json(&hash)?;
                let compiled = lab_simplicity::compile_src(&src, &args)?;
                let info = lab_simplicity::address_info(&compiled)?;
                println!("{}", serde_json::to_string_pretty(&info)?);
            }
            CovenantCmd::Demo { script } => {
                anyhow::ensure!(
                    script.is_file(),
                    "demo script missing: {} (run from repo root)",
                    script.display()
                );
                let status = std::process::Command::new("bash")
                    .arg(&script)
                    .status()
                    .with_context(|| format!("run {}", script.display()))?;
                if !status.success() {
                    anyhow::bail!("demo exited with {status}");
                }
            }
            CovenantCmd::DemoC1 { script } => {
                anyhow::ensure!(
                    script.is_file(),
                    "demo script missing: {} (run from repo root)",
                    script.display()
                );
                let status = std::process::Command::new("bash")
                    .arg(&script)
                    .status()
                    .with_context(|| format!("run {}", script.display()))?;
                if !status.success() {
                    anyhow::bail!("demo exited with {status}");
                }
            }
            CovenantCmd::DemoC2 { script } => {
                anyhow::ensure!(
                    script.is_file(),
                    "demo script missing: {} (run from repo root)",
                    script.display()
                );
                let status = std::process::Command::new("bash")
                    .arg(&script)
                    .status()
                    .with_context(|| format!("run {}", script.display()))?;
                if !status.success() {
                    anyhow::bail!("demo exited with {status}");
                }
            }
            CovenantCmd::DemoC4 { script } => {
                anyhow::ensure!(
                    script.is_file(),
                    "demo script missing: {} (run from repo root)",
                    script.display()
                );
                let status = std::process::Command::new("bash")
                    .arg(&script)
                    .status()
                    .with_context(|| format!("run {}", script.display()))?;
                if !status.success() {
                    anyhow::bail!("demo exited with {status}");
                }
            }
        },
        Commands::Bfa { cmd } => match cmd {
            BfaCmd::Issue {
                name,
                ticker,
                max_supply,
                gate_seal,
                backing,
                chain,
            } => {
                let json = lab_rgb::bfa::issue_json(
                    &name, &ticker, max_supply, &gate_seal, &backing, &chain,
                )?;
                println!("{}", serde_json::to_string_pretty(&json)?);
            }
            BfaCmd::MintPlan {
                name,
                ticker,
                max_supply,
                backing,
                genesis_gate,
                gate_seal,
                mint,
                recipient_seal,
                new_gate_seal,
                consume_opid,
                allowance,
                internal_key,
                entropy,
                chain,
            } => {
                let json = lab_rgb::bfa::plan_mint_json(
                    &name,
                    &ticker,
                    max_supply,
                    &backing,
                    &genesis_gate,
                    &gate_seal,
                    mint,
                    &recipient_seal,
                    &new_gate_seal,
                    consume_opid.as_deref(),
                    allowance,
                    &internal_key,
                    entropy,
                    &chain,
                )?;
                println!("{}", serde_json::to_string_pretty(&json)?);
            }
            BfaCmd::Audit { history, fetch_rpc } => {
                let s = fs::read_to_string(&history)
                    .with_context(|| format!("read {}", history.display()))?;
                let hist: lab_rgb::bfa::BfaHistory = serde_json::from_str(&s)?;
                let fetch = |txid: &str| -> Result<String> {
                    if !fetch_rpc {
                        anyhow::bail!("no witness_tx_hex for {txid} and --fetch-rpc disabled");
                    }
                    let out = std::process::Command::new("./scripts/regtest_simplicity.sh")
                        .args(["cli", "getrawtransaction", txid])
                        .output()
                        .context("regtest_simplicity.sh cli getrawtransaction")?;
                    if !out.status.success() {
                        anyhow::bail!(
                            "getrawtransaction failed: {}",
                            String::from_utf8_lossy(&out.stderr)
                        );
                    }
                    Ok(String::from_utf8(out.stdout)?.trim().to_string())
                };
                let result = lab_rgb::bfa::audit_history(&hist, &fetch)?;
                println!("{}", serde_json::to_string_pretty(&result)?);
                if !result.ok {
                    anyhow::bail!("{}", result.summary);
                }
            }
            BfaCmd::Demo { script } => {
                anyhow::ensure!(
                    script.is_file(),
                    "demo script missing: {} (run from repo root)",
                    script.display()
                );
                let status = std::process::Command::new("bash")
                    .arg(&script)
                    .status()
                    .with_context(|| format!("run {}", script.display()))?;
                if !status.success() {
                    anyhow::bail!("demo exited with {status}");
                }
            }
        },
        Commands::ApiRoot => {
            println!("{}", serde_json::to_string_pretty(&lab_api::root_json())?);
        }
    }
    Ok(())
}

// S3 fund-wrap / claim / extract-preimage live in lab_api::s3 + SwapService.
