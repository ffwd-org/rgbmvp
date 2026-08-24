//! Liquid Testnet chain helpers via LWK (not RGB).
//!
//! Phase 0: reachability probes + singlesig CT wallet create/address/balance.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use lab_core::{write_secret_file, Config, HealthCheck, HealthReport};
use lwk_common::Signer;
use lwk_signer::SwSigner;
use lwk_wollet::{
    full_scan_with_electrum_client, ElectrumClient, ElectrumUrl, Network, Wollet, WolletBuilder,
    WolletDescriptor,
};
use serde::{Deserialize, Serialize};

const DEFAULT_WALLET: &str = "default";

/// Probe public Liquid Testnet endpoints (Esplora tip + Electrum TLS dial).
pub fn network_status(cfg: &Config) -> Result<HealthReport> {
    let mut report = HealthReport::phase0_base(cfg.network);
    let mut checks = Vec::new();

    // Esplora tip height
    let esplora = probe_esplora(&cfg.esplora_tip_url);
    checks.push(HealthCheck {
        name: "esplora_tip".into(),
        ok: esplora.is_ok(),
        detail: match &esplora {
            Ok(h) => Some(format!("height={h} url={}", cfg.esplora_tip_url)),
            Err(e) => Some(e.to_string()),
        },
    });

    // Electrum: construct client (TLS handshake / connect)
    let electrum = probe_electrum(cfg);
    checks.push(HealthCheck {
        name: "electrum".into(),
        ok: electrum.is_ok(),
        detail: match &electrum {
            Ok(()) => Some(format!(
                "ok host={} tls={}",
                cfg.electrum_host_port, cfg.electrum_tls
            )),
            Err(e) => Some(e.to_string()),
        },
    });

    checks.push(HealthCheck {
        name: "rgb_stack".into(),
        ok: true,
        detail: Some("lab-rgb linked (WitnessTx-patched rgb-consensus 0.11.1-rc.10)".into()),
    });

    let all_net_ok = checks.iter().all(|c| c.ok);
    report.status = if all_net_ok {
        "ready".into()
    } else {
        "degraded".into()
    };
    report.rgb_ready = true;
    report.phase = "0-p0".into();
    report.checks = checks;
    Ok(report)
}

fn probe_esplora(url: &str) -> Result<u64> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;
    let body = client
        .get(url)
        .send()
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("HTTP error for {url}"))?
        .text()?;
    let height: u64 = body
        .trim()
        .parse()
        .with_context(|| format!("parse tip height from {body:?}"))?;
    Ok(height)
}

fn probe_electrum(cfg: &Config) -> Result<()> {
    let url = ElectrumUrl::new(
        &cfg.electrum_host_port,
        cfg.electrum_tls,
        cfg.electrum_validate_domain,
    )
    .map_err(|e| anyhow::anyhow!("ElectrumUrl: {e}"))?;
    let _client = ElectrumClient::new(&url).map_err(|e| anyhow::anyhow!("ElectrumClient: {e}"))?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletCreateResult {
    pub name: String,
    pub network: String,
    pub descriptor: String,
    pub address: String,
    pub address_index: u32,
    pub mnemonic_path: String,
    pub warning: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletAddressResult {
    pub name: String,
    pub network: String,
    pub address: String,
    pub address_index: u32,
    pub explorer_hint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletBalanceResult {
    pub name: String,
    pub network: String,
    pub balances_sats: BTreeMap<String, u64>,
    pub policy_asset: String,
    pub lbtc_sats: u64,
    pub synced: bool,
}

/// Create a new singlesig Liquid Testnet wallet (mnemonic written under data dir).
pub fn wallet_create(cfg: &Config, name: &str, force: bool) -> Result<WalletCreateResult> {
    cfg.ensure_dirs()?;
    let name = if name.is_empty() {
        DEFAULT_WALLET
    } else {
        name
    };
    let dir = cfg.wallet_path(name);
    let mnemonic_path = dir.join("mnemonic");
    let descriptor_path = dir.join("descriptor");
    let meta_path = dir.join("meta.json");

    if mnemonic_path.exists() && !force {
        bail!(
            "wallet {name:?} already exists at {}; pass --force to overwrite (destroys keys)",
            dir.display()
        );
    }

    std::fs::create_dir_all(&dir)?;

    let is_mainnet = false;
    let (signer, mnemonic) =
        SwSigner::random(is_mainnet).map_err(|e| anyhow::anyhow!("SwSigner::random: {e}"))?;
    let descriptor = signer
        .wpkh_slip77_descriptor()
        .map_err(|e| anyhow::anyhow!("descriptor: {e}"))?;

    write_secret_file(&mnemonic_path, &mnemonic.to_string())?;
    write_secret_file(&descriptor_path, &descriptor)?;

    let wollet = open_wollet(&descriptor)?;
    let addr = wollet
        .address(None)
        .map_err(|e| anyhow::anyhow!("address: {e}"))?;

    let meta = serde_json::json!({
        "name": name,
        "network": cfg.network.to_string(),
        "created_with": "lab-chain/phase0",
        "descriptor_kind": "wpkh_slip77",
    });
    std::fs::write(&meta_path, serde_json::to_vec_pretty(&meta)?)?;

    Ok(WalletCreateResult {
        name: name.into(),
        network: cfg.network.to_string(),
        descriptor,
        address: addr.address().to_string(),
        address_index: addr.index(),
        mnemonic_path: mnemonic_path.display().to_string(),
        warning: "TESTNET ONLY. Back up mnemonic offline. Never commit .rgbmvp/ or .env secrets."
            .into(),
    })
}

pub fn wallet_address(cfg: &Config, name: &str, index: Option<u32>) -> Result<WalletAddressResult> {
    let name = if name.is_empty() {
        DEFAULT_WALLET
    } else {
        name
    };
    let descriptor = load_descriptor(cfg, name)?;
    let wollet = open_wollet(&descriptor)?;
    let addr = wollet
        .address(index)
        .map_err(|e| anyhow::anyhow!("address: {e}"))?;
    Ok(WalletAddressResult {
        name: name.into(),
        network: cfg.network.to_string(),
        address: addr.address().to_string(),
        address_index: addr.index(),
        explorer_hint: cfg.explorer_base.clone(),
    })
}

pub fn wallet_balance(cfg: &Config, name: &str) -> Result<WalletBalanceResult> {
    let name = if name.is_empty() {
        DEFAULT_WALLET
    } else {
        name
    };
    let descriptor = load_descriptor(cfg, name)?;
    let mut wollet = open_wollet(&descriptor)?;

    let url = ElectrumUrl::new(
        &cfg.electrum_host_port,
        cfg.electrum_tls,
        cfg.electrum_validate_domain,
    )
    .map_err(|e| anyhow::anyhow!("ElectrumUrl: {e}"))?;
    let mut client =
        ElectrumClient::new(&url).map_err(|e| anyhow::anyhow!("ElectrumClient: {e}"))?;
    full_scan_with_electrum_client(&mut wollet, &mut client)
        .map_err(|e| anyhow::anyhow!("full_scan: {e}"))?;

    let balance = wollet
        .balance()
        .map_err(|e| anyhow::anyhow!("balance: {e}"))?;
    let policy = Network::TestnetLiquid.policy_asset().to_string();
    let mut balances_sats = BTreeMap::new();
    let mut lbtc_sats = 0u64;
    for (asset, amount) in balance.iter() {
        let key = asset.to_string();
        if key == policy {
            lbtc_sats = *amount;
        }
        balances_sats.insert(key, *amount);
    }

    Ok(WalletBalanceResult {
        name: name.into(),
        network: cfg.network.to_string(),
        balances_sats,
        policy_asset: policy,
        lbtc_sats,
        synced: true,
    })
}

fn load_descriptor(cfg: &Config, name: &str) -> Result<String> {
    let fallback = cfg.wallet_path(name);
    let path = lab_core::resolve_secret_path(
        &lab_core::secret_dirs(),
        &fallback,
        name,
        lab_core::KIND_DESCRIPTOR,
    )
    .ok_or_else(|| {
        anyhow::anyhow!(
        "wallet {name:?} not found (missing descriptor); run: rgbmvp wallet create --name {name}"
    )
    })?;
    lab_core::read_trimmed(&path)
}

fn open_wollet(descriptor: &str) -> Result<Wollet> {
    let wd: WolletDescriptor = descriptor
        .parse()
        .map_err(|e| anyhow::anyhow!("parse descriptor: {e}"))?;
    WolletBuilder::new(Network::TestnetLiquid, wd)
        .build()
        .map_err(|e| anyhow::anyhow!("WolletBuilder: {e}"))
}

/// Load mnemonic for signing (never printed to logs).
///
/// W3: a mounted Secret Manager entry (`RGBMVP_SECRET_DIR`) wins over the local
/// wallet copy, so a public deployment signs with runtime-injected material
/// instead of anything baked into the image.
pub fn load_mnemonic(cfg: &Config, name: &str) -> Result<String> {
    let fallback = cfg.wallet_path(name);
    let path = lab_core::resolve_secret_path(
        &lab_core::secret_dirs(),
        &fallback,
        name,
        lab_core::KIND_MNEMONIC,
    )
    .ok_or_else(|| anyhow::anyhow!("mnemonic missing for wallet {name}"))?;
    lab_core::read_trimmed(&path)
}

/// Create wallet from an explicit mnemonic (import / fixtures).
pub fn wallet_import(
    cfg: &Config,
    name: &str,
    mnemonic: &str,
    force: bool,
    role: Option<&str>,
) -> Result<WalletCreateResult> {
    cfg.ensure_dirs()?;
    let name = if name.is_empty() {
        DEFAULT_WALLET
    } else {
        name
    };
    let dir = cfg.wallet_path(name);
    let mnemonic_path = dir.join("mnemonic");
    if mnemonic_path.exists() && !force {
        bail!(
            "wallet {name:?} already exists at {}; pass --force to overwrite",
            dir.display()
        );
    }
    std::fs::create_dir_all(&dir)?;
    let is_mainnet = false;
    let signer = SwSigner::new(mnemonic.trim(), is_mainnet)
        .map_err(|e| anyhow::anyhow!("SwSigner::new (import): {e}"))?;
    let descriptor = signer
        .wpkh_slip77_descriptor()
        .map_err(|e| anyhow::anyhow!("descriptor: {e}"))?;
    write_secret_file(&mnemonic_path, mnemonic.trim())?;
    write_secret_file(&dir.join("descriptor"), &descriptor)?;
    let wollet = open_wollet(&descriptor)?;
    let addr = wollet
        .address(None)
        .map_err(|e| anyhow::anyhow!("address: {e}"))?;
    let meta = serde_json::json!({
        "name": name,
        "role": role,
        "network": cfg.network.to_string(),
        "created_with": "lab-chain/import",
        "descriptor_kind": "wpkh_slip77",
        "fixture": role.is_some(),
    });
    std::fs::write(dir.join("meta.json"), serde_json::to_vec_pretty(&meta)?)?;
    Ok(WalletCreateResult {
        name: name.into(),
        network: cfg.network.to_string(),
        descriptor,
        address: addr.address().to_string(),
        address_index: addr.index(),
        mnemonic_path: mnemonic_path.display().to_string(),
        warning: "TESTNET ONLY. Fixture/imported mnemonic.".into(),
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletListEntry {
    pub name: String,
    pub role: Option<String>,
    pub address0: Option<String>,
    pub lbtc_sats: Option<u64>,
    pub path: String,
}

pub fn wallet_list(cfg: &Config, sync_balances: bool) -> Result<Vec<WalletListEntry>> {
    cfg.ensure_dirs()?;
    let mut out = Vec::new();
    let root = &cfg.wallet_dir;
    if !root.exists() {
        return Ok(out);
    }
    let mut names: Vec<_> = std::fs::read_dir(root)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().join("descriptor").exists())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    for name in names {
        let meta_path = cfg.wallet_path(&name).join("meta.json");
        let role = std::fs::read_to_string(&meta_path)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| {
                v.get("role")
                    .and_then(|r| r.as_str().map(|s| s.to_string()))
            });
        let address0 = wallet_address(cfg, &name, Some(0)).ok().map(|a| a.address);
        let lbtc_sats = if sync_balances {
            wallet_balance(cfg, &name).ok().map(|b| b.lbtc_sats)
        } else {
            None
        };
        out.push(WalletListEntry {
            name: name.clone(),
            role,
            address0,
            lbtc_sats,
            path: cfg.wallet_path(&name).display().to_string(),
        });
    }
    Ok(out)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootstrapResult {
    pub wallets: Vec<WalletCreateResult>,
    pub registry_path: String,
}

/// Import all roles from fixtures/testnet_wallets.json (or path).
pub fn wallet_bootstrap_fixtures(
    cfg: &Config,
    fixture_path: &Path,
    force: bool,
) -> Result<BootstrapResult> {
    let raw = std::fs::read_to_string(fixture_path)
        .with_context(|| format!("read fixture {}", fixture_path.display()))?;
    let doc: serde_json::Value = serde_json::from_str(&raw)?;
    let mut wallets = Vec::new();
    let arr = doc
        .get("wallets")
        .and_then(|w| w.as_array())
        .context("fixture.wallets array")?;
    for w in arr {
        let name = w
            .get("name")
            .and_then(|x| x.as_str())
            .context("wallet.name")?;
        let role = w.get("role").and_then(|x| x.as_str());
        let mnemonic = w
            .get("mnemonic")
            .and_then(|x| x.as_str())
            .context("wallet.mnemonic")?;
        wallets.push(wallet_import(cfg, name, mnemonic, force, role)?);
    }
    let reg = write_wallet_registry(cfg)?;
    Ok(BootstrapResult {
        wallets,
        registry_path: reg.display().to_string(),
    })
}

/// Write a non-secret address registry for tests (no mnemonics).
pub fn write_wallet_registry(cfg: &Config) -> Result<std::path::PathBuf> {
    let list = wallet_list(cfg, false)?;
    let mut reg = Vec::new();
    for e in &list {
        let a0 = wallet_address(cfg, &e.name, Some(0))?;
        let a1 = wallet_address(cfg, &e.name, Some(1))?;
        reg.push(serde_json::json!({
            "name": e.name,
            "role": e.role,
            "network": cfg.network.to_string(),
            "address_0": a0.address,
            "address_1": a1.address,
        }));
    }
    let path = cfg.data_dir.join("wallet_registry.json");
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "network": cfg.network.to_string(),
            "updated": true,
            "wallets": reg,
        }))?,
    )?;
    // Also copy a non-secret mirror under fixtures if writable (best-effort)
    let mirror = Path::new("fixtures/wallet_registry.local.json");
    if let Some(parent) = mirror.parent() {
        let _ = std::fs::create_dir_all(parent);
        let _ = std::fs::copy(&path, mirror);
    }
    Ok(path)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendResult {
    pub from: String,
    pub to_address: String,
    pub amount_sats: u64,
    pub txid: String,
    pub explorer_url: String,
}

/// Send L-BTC between wallets (or to any address) for testnet rebalancing.
pub fn send_lbtc(
    cfg: &Config,
    from_wallet: &str,
    to_address: &str,
    amount_sats: u64,
) -> Result<SendResult> {
    use lwk_common::Signer as _;
    use lwk_wollet::clients::blocking::BlockchainBackend;
    use std::str::FromStr;

    if amount_sats < 500 {
        bail!("amount_sats too small (min 500 for fees headroom)");
    }
    let name = if from_wallet.is_empty() {
        DEFAULT_WALLET
    } else {
        from_wallet
    };
    let mnemonic = load_mnemonic(cfg, name)?;
    let signer = SwSigner::new(&mnemonic, false).map_err(|e| anyhow::anyhow!("signer: {e}"))?;
    let wollet = load_synced_wollet(cfg, name)?;
    let dest =
        elements::Address::from_str(to_address).map_err(|e| anyhow::anyhow!("to_address: {e}"))?;
    let policy = *Network::TestnetLiquid.policy_asset();

    let builder = if dest.blinding_pubkey.is_some() {
        wollet
            .tx_builder()
            .add_lbtc_recipient(&dest, amount_sats)
            .map_err(|e| anyhow::anyhow!("add_lbtc_recipient: {e}"))?
    } else {
        wollet
            .tx_builder()
            .add_explicit_recipient(&dest, amount_sats, policy)
            .map_err(|e| anyhow::anyhow!("add_explicit_recipient: {e}"))?
    };

    let mut pset = builder.finish().map_err(|e| anyhow::anyhow!("pset: {e}"))?;
    let _ = signer
        .sign(&mut pset)
        .map_err(|e| anyhow::anyhow!("sign: {e}"))?;
    let tx = wollet
        .finalize(&mut pset)
        .map_err(|e| anyhow::anyhow!("finalize: {e}"))?;
    let client = electrum_client(cfg)?;
    let txid = client
        .broadcast(&tx)
        .map_err(|e| anyhow::anyhow!("broadcast: {e}"))?;

    Ok(SendResult {
        from: name.into(),
        to_address: to_address.into(),
        amount_sats,
        txid: txid.to_string(),
        explorer_url: format!("{}/tx/{}", cfg.explorer_base.trim_end_matches('/'), txid),
    })
}

/// Resolve a wallet name to address index 0 (for scripts).
pub fn wallet_receive_address(cfg: &Config, name: &str) -> Result<String> {
    Ok(wallet_address(cfg, name, Some(0))?.address)
}

// ── P0: UTXOs, Esplora witness fetch, LWK spend-to-tapret ─────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UtxoInfo {
    pub outpoint: String,
    pub asset: String,
    pub value: u64,
    pub height: Option<u32>,
}

pub fn wallet_utxos(cfg: &Config, name: &str) -> Result<Vec<UtxoInfo>> {
    let name = if name.is_empty() {
        DEFAULT_WALLET
    } else {
        name
    };
    let wollet = load_synced_wollet(cfg, name)?;
    let mut out = Vec::new();
    for u in wollet.utxos().map_err(|e| anyhow::anyhow!("utxos: {e}"))? {
        out.push(UtxoInfo {
            outpoint: format!("{}:{}", u.outpoint.txid, u.outpoint.vout),
            asset: u.unblinded.asset.to_string(),
            value: u.unblinded.value,
            height: u.height,
        });
    }
    out.sort_by(|a, b| b.value.cmp(&a.value));
    Ok(out)
}

/// Pick the largest L-BTC UTXO as an RGB seal candidate.
pub fn pick_lbtc_seal(cfg: &Config, name: &str) -> Result<UtxoInfo> {
    let policy = Network::TestnetLiquid.policy_asset().to_string();
    let utxos = wallet_utxos(cfg, name)?;
    utxos
        .into_iter()
        .find(|u| u.asset == policy && u.value >= 1000)
        .ok_or_else(|| anyhow::anyhow!("no L-BTC UTXO ≥ 1000 sats in wallet {name}"))
}

fn load_synced_wollet(cfg: &Config, name: &str) -> Result<Wollet> {
    let descriptor = load_descriptor(cfg, name)?;
    let mut wollet = open_wollet(&descriptor)?;
    let mut client = electrum_client(cfg)?;
    full_scan_with_electrum_client(&mut wollet, &mut client)
        .map_err(|e| anyhow::anyhow!("full_scan: {e}"))?;
    Ok(wollet)
}

fn electrum_client(cfg: &Config) -> Result<ElectrumClient> {
    let url = ElectrumUrl::new(
        &cfg.electrum_host_port,
        cfg.electrum_tls,
        cfg.electrum_validate_domain,
    )
    .map_err(|e| anyhow::anyhow!("ElectrumUrl: {e}"))?;
    ElectrumClient::new(&url).map_err(|e| anyhow::anyhow!("ElectrumClient: {e}"))
}

/// Fetch a Liquid Testnet witness as lab_rgb::seal::WitnessTx via Esplora REST.
pub fn fetch_witness_esplora(esplora_base: &str, txid: &str) -> Result<lab_rgb::seal::WitnessTx> {
    // esplora_base like https://blockstream.info/liquidtestnet/api
    let base = if esplora_base.contains("/api") {
        esplora_base.trim_end_matches('/').to_string()
    } else {
        format!("{}/api", esplora_base.trim_end_matches('/'))
    };
    let url = format!("{base}/tx/{txid}");
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
    let v: serde_json::Value = client
        .get(&url)
        .send()
        .with_context(|| format!("GET {url}"))?
        .error_for_status()?
        .json()?;

    let vin_outpoints = v["vin"]
        .as_array()
        .context("no vin")?
        .iter()
        .map(|x| {
            let txid = x["txid"].as_str().unwrap_or_default().to_lowercase();
            let vout = x["vout"].as_u64().unwrap_or_default() as u32;
            (txid, vout)
        })
        .collect();

    let vouts_spk_hex = v["vout"]
        .as_array()
        .context("no vout")?
        .iter()
        .map(|x| {
            x["scriptpubkey"]
                .as_str()
                .or_else(|| x["scriptPubKey"]["hex"].as_str())
                .unwrap_or_default()
                .to_lowercase()
        })
        .collect();

    Ok(lab_rgb::seal::WitnessTx {
        txid: txid.to_lowercase(),
        vin_outpoints,
        vouts_spk_hex,
    })
}

/// Raw transaction hex from Esplora (`/tx/{txid}/hex`).
pub fn fetch_tx_hex(cfg: &Config, txid: &str) -> Result<String> {
    let api = esplora_api_base(cfg);
    let url = format!("{}/tx/{}/hex", api.trim_end_matches('/'), txid);
    let body = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?
        .get(&url)
        .send()
        .with_context(|| format!("GET {url}"))?
        .error_for_status()?
        .text()?;
    Ok(body.trim().to_string())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BroadcastResult {
    pub txid: String,
    pub explorer_url: String,
    pub commitment_address: String,
    pub note: String,
}

/// Spend the seal UTXO, paying dust L-BTC to the tapret commitment address
/// and optional extra L-BTC to `bob_address`. Signs with the wallet mnemonic.
pub fn broadcast_commitment_tx(
    cfg: &Config,
    wallet_name: &str,
    seal_outpoint: &str,
    tapret_address: &str,
    bob_address: Option<&str>,
    commitment_sats: u64,
    bob_sats: u64,
) -> Result<BroadcastResult> {
    use elements::OutPoint as ElOutPoint;
    use lwk_common::Signer as _;
    use lwk_wollet::clients::blocking::BlockchainBackend;
    use std::str::FromStr;

    let name = if wallet_name.is_empty() {
        DEFAULT_WALLET
    } else {
        wallet_name
    };
    let mnemonic = load_mnemonic(cfg, name)?;
    let signer = SwSigner::new(&mnemonic, false).map_err(|e| anyhow::anyhow!("signer: {e}"))?;
    let wollet = load_synced_wollet(cfg, name)?;

    let (txid_s, vout_s) = seal_outpoint
        .split_once(':')
        .context("seal outpoint txid:vout")?;
    let txid = elements::Txid::from_str(txid_s).context("parse elements txid")?;
    let vout: u32 = vout_s.parse()?;
    let op = ElOutPoint { txid, vout };

    // Tapret commitment is an *unconfidential* P2TR (tex1p…). LWK's
    // add_lbtc_recipient requires confidential addresses, so use explicit.
    let tapret_addr = elements::Address::from_str(tapret_address)
        .map_err(|e| anyhow::anyhow!("tapret address: {e}"))?;
    let policy = *Network::TestnetLiquid.policy_asset();

    let mut builder = wollet
        .tx_builder()
        .set_wallet_utxos(vec![op])
        .add_explicit_recipient(&tapret_addr, commitment_sats, policy)
        .map_err(|e| anyhow::anyhow!("add tapret (explicit) recipient: {e}"))?;

    if let Some(bob) = bob_address {
        if bob_sats > 0 {
            let bob_addr = elements::Address::from_str(bob)
                .map_err(|e| anyhow::anyhow!("bob address: {e}"))?;
            // Bob can be confidential (tlq1…) or not.
            builder = if bob_addr.blinding_pubkey.is_some() {
                builder
                    .add_lbtc_recipient(&bob_addr, bob_sats)
                    .map_err(|e| anyhow::anyhow!("add bob recipient: {e}"))?
            } else {
                builder
                    .add_explicit_recipient(&bob_addr, bob_sats, policy)
                    .map_err(|e| anyhow::anyhow!("add bob explicit: {e}"))?
            };
        }
    }

    let mut pset = builder
        .finish()
        .map_err(|e| anyhow::anyhow!("pset finish: {e}"))?;
    let _sigs = signer
        .sign(&mut pset)
        .map_err(|e| anyhow::anyhow!("sign: {e}"))?;
    let tx = wollet
        .finalize(&mut pset)
        .map_err(|e| anyhow::anyhow!("finalize: {e}"))?;

    let client = electrum_client(cfg)?;
    let txid = client
        .broadcast(&tx)
        .map_err(|e| anyhow::anyhow!("broadcast: {e}"))?;

    Ok(BroadcastResult {
        txid: txid.to_string(),
        explorer_url: format!("{}/tx/{}", cfg.explorer_base.trim_end_matches('/'), txid),
        commitment_address: tapret_address.into(),
        note:
            "Broadcast ok. TapretFirst requires commitment as first P2TR; verify with rgb verify."
                .into(),
    })
}

/// Esplora API base derived from tip URL or explorer.
pub fn esplora_api_base(cfg: &Config) -> String {
    if cfg.esplora_tip_url.contains("/api/") {
        let idx = cfg.esplora_tip_url.find("/api/").unwrap();
        format!("{}api", &cfg.esplora_tip_url[..idx + 1])
    } else {
        format!("{}/api", cfg.explorer_base.trim_end_matches('/'))
    }
}

/// Broadcast raw Elements tx hex via Esplora (Liquid testnet).
pub fn broadcast_raw_hex(cfg: &Config, tx_hex: &str) -> Result<String> {
    let api = esplora_api_base(cfg);
    let url = format!("{api}/tx");
    let resp = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?
        .post(&url)
        .header("Content-Type", "text/plain")
        .body(tx_hex.to_string())
        .send()
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    let body = resp.text()?;
    if !status.is_success() {
        bail!("liquid broadcast failed HTTP {status}: {body}");
    }
    Ok(body.trim().to_string())
}

/// Find UTXO on address via Esplora.
pub fn find_address_utxo(cfg: &Config, address: &str, min_sats: u64) -> Result<(String, u32, u64)> {
    let api = esplora_api_base(cfg);
    let url = format!("{api}/address/{address}/utxo");
    let v: serde_json::Value = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?
        .get(&url)
        .send()?
        .error_for_status()?
        .json()?;
    let arr = v.as_array().context("utxo array")?;
    for u in arr {
        let value = u["value"].as_u64().unwrap_or(0);
        if value >= min_sats {
            return Ok((
                u["txid"].as_str().unwrap_or_default().to_string(),
                u["vout"].as_u64().unwrap_or(0) as u32,
                value,
            ));
        }
    }
    bail!("no UTXO ≥ {min_sats} on {address}");
}

/// Unconfidential scriptPubKey hex for a wallet receive address (for HTLC claim dest).
pub fn address_spk_hex(address: &str) -> Result<Vec<u8>> {
    use std::str::FromStr;
    let a = elements::Address::from_str(address).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(a.script_pubkey().into_bytes())
}

// ---------------------------------------------------------------------------
// T1/W5 — Liquid demo HTLC exit addresses.
//
// Mirror of `lab_btc::demo_exit_address` for the Liquid legs. Both Liquid exit
// paths (Alice's claim, Bob's refund) pay a P2WPKH derived from a custody-backed
// root seed plus the public role label, never the funding wallet.
// ---------------------------------------------------------------------------

/// Labels every Liquid-side HTLC exit pays out to.
pub const LQ_DEMO_EXIT_LABELS: [&str; 2] = ["alice-claimer", "bob-refund"];

/// Unconfidential Liquid P2WPKH address a demo label receives at.
pub fn demo_exit_address_lq(keyring: &lab_rgb::htlc::DemoKeyring, label: &str) -> Result<String> {
    let (_, pk_bytes) = keyring.derive(label)?;
    // Same witness program as the Bitcoin form; Liquid testnet encodes it with
    // the `tex` HRP as an unconfidential address.
    let wpkh = elements::bitcoin::PublicKey::from_slice(&pk_bytes)
        .map_err(|e| anyhow::anyhow!("pubkey: {e}"))?
        .wpubkey_hash()
        .map_err(|e| anyhow::anyhow!("wpubkey_hash: {e}"))?;
    let spk = elements::script::Builder::new()
        .push_int(0)
        .push_slice(&wpkh[..])
        .into_script();
    let addr = elements::Address::from_script(&spk, None, &elements::AddressParams::LIQUID_TESTNET)
        .context("liquid address from script")?;
    Ok(addr.to_string())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LqExitBalance {
    pub label: String,
    pub address: String,
    pub utxo_count: usize,
    pub balance_sats: u64,
}

/// Read the L-BTC balance sitting at a Liquid demo exit address.
///
/// Only counts the explicit policy asset; confidential outputs are invisible
/// here and would need a wallet scan.
pub fn demo_exit_balance_lq(cfg: &Config, label: &str) -> Result<LqExitBalance> {
    let keyring = lab_rgb::htlc::DemoKeyring::new(cfg.demo_exit_seed()?)?;
    let address = demo_exit_address_lq(&keyring, label)?;
    let api = esplora_api_base(cfg);
    let url = format!("{api}/address/{address}/utxo");
    let v: serde_json::Value = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?
        .get(&url)
        .send()?
        .error_for_status()?
        .json()?;
    let arr = v.as_array().cloned().unwrap_or_default();
    let balance_sats = arr.iter().filter_map(|u| u["value"].as_u64()).sum();
    Ok(LqExitBalance {
        label: label.to_string(),
        address,
        utxo_count: arr.len(),
        balance_sats,
    })
}

/// Result of sweeping one Liquid demo exit address.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LqSweepResult {
    pub label: String,
    pub address: String,
    pub utxo_count: usize,
    pub swept_sats: u64,
    pub fee_sats: u64,
    pub txid: Option<String>,
    pub explorer_url: Option<String>,
    pub skipped_reason: Option<String>,
}

/// Sweep the explicit L-BTC sitting at one Liquid demo exit address.
///
/// Consolidates every confirmed policy-asset UTXO into a single output paid to
/// `to_address`, plus the explicit Elements fee output. Confidential outputs are
/// invisible to this path and are left untouched.
pub fn sweep_demo_exit_lq(
    cfg: &Config,
    keyring: &lab_rgb::htlc::DemoKeyring,
    label: &str,
    to_address: &str,
    fee_sats: u64,
) -> Result<LqSweepResult> {
    use elements::confidential::{Asset, Nonce, Value};
    use elements::encode::serialize_hex;
    use elements::hashes::Hash as _;
    use elements::secp256k1_zkp::{Message, Secp256k1};
    use elements::sighash::SighashCache;
    use elements::{
        AssetId, EcdsaSighashType, OutPoint, Script, Sequence, Transaction, TxIn, TxInWitness,
        TxOut, TxOutWitness, Txid,
    };
    use std::str::FromStr;

    let (sk_btc, pk_bytes) = keyring.derive(label)?;
    let address = demo_exit_address_lq(keyring, label)?;
    let policy = Network::TestnetLiquid.policy_asset().to_string();

    // Collect confirmed, explicit policy-asset UTXOs.
    let api = esplora_api_base(cfg);
    let v: serde_json::Value = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?
        .get(format!("{api}/address/{address}/utxo"))
        .send()?
        .error_for_status()?
        .json()?;
    let mut utxos: Vec<(Txid, u32, u64)> = Vec::new();
    for u in v.as_array().cloned().unwrap_or_default() {
        let confirmed = u["status"]["confirmed"].as_bool().unwrap_or(false);
        let asset_ok = u["asset"].as_str().map(|a| a == policy).unwrap_or(false);
        let value = u["value"].as_u64().unwrap_or(0);
        if confirmed && asset_ok && value > 0 {
            utxos.push((
                Txid::from_str(u["txid"].as_str().unwrap_or_default())?,
                u["vout"].as_u64().unwrap_or(0) as u32,
                value,
            ));
        }
    }
    let total: u64 = utxos.iter().map(|(_, _, v)| *v).sum();

    let mut result = LqSweepResult {
        label: label.to_string(),
        address: address.clone(),
        utxo_count: utxos.len(),
        swept_sats: 0,
        fee_sats,
        txid: None,
        explorer_url: None,
        skipped_reason: None,
    };
    if utxos.is_empty() {
        result.skipped_reason = Some("no confirmed explicit L-BTC utxos".into());
        return Ok(result);
    }
    if total <= fee_sats {
        result.skipped_reason = Some(format!("balance {total} does not cover fee {fee_sats}"));
        return Ok(result);
    }

    let asset = Asset::Explicit(AssetId::from_str(&policy)?);
    let dest_spk = Script::from(address_spk_hex(to_address)?);
    let out_sats = total - fee_sats;

    let mut tx = Transaction {
        version: 2,
        lock_time: elements::LockTime::ZERO,
        input: utxos
            .iter()
            .map(|(txid, vout, _)| TxIn {
                previous_output: OutPoint::new(*txid, *vout),
                is_pegin: false,
                script_sig: Script::new(),
                sequence: Sequence::from_consensus(0xffff_fffd),
                asset_issuance: Default::default(),
                witness: TxInWitness::default(),
            })
            .collect(),
        output: vec![
            TxOut {
                asset,
                value: Value::Explicit(out_sats),
                nonce: Nonce::Null,
                script_pubkey: dest_spk,
                witness: TxOutWitness::default(),
            },
            // Elements carries the fee as an explicit, script-less output.
            TxOut {
                asset,
                value: Value::Explicit(fee_sats),
                nonce: Nonce::Null,
                script_pubkey: Script::new(),
                witness: TxOutWitness::default(),
            },
        ],
    };

    // BIP143 script code for P2WPKH is the equivalent P2PKH script.
    let pk = elements::bitcoin::PublicKey::from_slice(&pk_bytes)
        .map_err(|e| anyhow::anyhow!("pubkey: {e}"))?;
    let wpkh = pk
        .wpubkey_hash()
        .map_err(|e| anyhow::anyhow!("wpubkey_hash: {e}"))?;
    let script_code = elements::script::Builder::new()
        .push_opcode(elements::opcodes::all::OP_DUP)
        .push_opcode(elements::opcodes::all::OP_HASH160)
        .push_slice(&wpkh[..])
        .push_opcode(elements::opcodes::all::OP_EQUALVERIFY)
        .push_opcode(elements::opcodes::all::OP_CHECKSIG)
        .into_script();

    let secp = Secp256k1::new();
    let sk = elements::secp256k1_zkp::SecretKey::from_slice(&sk_btc.secret_bytes())
        .context("secret key")?;
    let mut witnesses = Vec::with_capacity(utxos.len());
    for (i, (_, _, value)) in utxos.iter().enumerate() {
        let sighash = SighashCache::new(&tx).segwitv0_sighash(
            i,
            &script_code,
            Value::Explicit(*value),
            EcdsaSighashType::All,
        );
        let msg = Message::from_digest(sighash.to_byte_array());
        let mut sig = secp.sign_ecdsa(&msg, &sk).serialize_der().to_vec();
        sig.push(EcdsaSighashType::All as u8);
        witnesses.push(vec![sig, pk_bytes.to_vec()]);
    }
    for (i, w) in witnesses.into_iter().enumerate() {
        tx.input[i].witness.script_witness = w;
    }

    let hex_tx = serialize_hex(&tx);
    let txid = broadcast_raw_hex(cfg, &hex_tx)?;
    result.swept_sats = out_sats;
    result.txid = Some(txid.clone());
    result.explorer_url = Some(format!("{}/tx/{}", cfg.explorer_base, txid));
    Ok(result)
}

/// Sweep all Liquid demo exit addresses into a lab wallet's receive address.
pub fn sweep_all_demo_exits_lq(
    cfg: &Config,
    to_wallet: &str,
    fee_sats: u64,
) -> Result<Vec<LqSweepResult>> {
    let dest = wallet_address(cfg, to_wallet, None)?.address;
    let keyring = lab_rgb::htlc::DemoKeyring::new(cfg.demo_exit_seed()?)?;
    let mut out = Vec::new();
    for label in LQ_DEMO_EXIT_LABELS {
        match sweep_demo_exit_lq(cfg, &keyring, label, &dest, fee_sats) {
            Ok(r) => out.push(r),
            Err(e) => out.push(LqSweepResult {
                label: label.to_string(),
                address: String::new(),
                utxo_count: 0,
                swept_sats: 0,
                fee_sats,
                txid: None,
                explorer_url: None,
                skipped_reason: Some(format!("error: {e}")),
            }),
        }
    }
    Ok(out)
}
