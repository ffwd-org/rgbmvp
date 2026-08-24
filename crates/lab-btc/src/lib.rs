//! Bitcoin testnet helpers for P1 (Esplora + WIF P2WPKH).
//!
//! Does **not** use LWK (Liquid-only). Secrets stay under `.rgbmvp/wallets/<name>/`.

use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use bitcoin::address::NetworkUnchecked;
use bitcoin::key::Secp256k1;
use bitcoin::secp256k1::SecretKey;
use bitcoin::{Address, CompressedPublicKey, Network, PrivateKey};
use lab_core::{write_secret_file, Config};
use serde::{Deserialize, Serialize};

const DEFAULT_NAME: &str = "btc-alice";
const DEFAULT_ESPLORA: &str = "https://blockstream.info/testnet/api";
const DEFAULT_EXPLORER: &str = "https://blockstream.info/testnet";

#[derive(Debug, Clone)]
pub struct BtcConfig {
    pub network: Network,
    pub esplora_api: String,
    pub explorer_base: String,
    pub env_address: Option<String>,
    pub env_wif: Option<String>,
}

impl BtcConfig {
    pub fn from_env() -> Self {
        let _ = dotenvy::dotenv();
        let network = match std::env::var("BTC_NETWORK")
            .unwrap_or_else(|_| "testnet".into())
            .to_ascii_lowercase()
            .as_str()
        {
            "mainnet" | "bitcoin" => Network::Bitcoin,
            "testnet4" => Network::Testnet4,
            "signet" => Network::Signet,
            "regtest" => Network::Regtest,
            _ => Network::Testnet,
        };
        Self {
            network,
            esplora_api: std::env::var("BTC_ESPLORA_URL")
                .unwrap_or_else(|_| DEFAULT_ESPLORA.into())
                .trim_end_matches('/')
                .to_string(),
            explorer_base: std::env::var("BTC_TESTNET_EXPLORER")
                .unwrap_or_else(|_| DEFAULT_EXPLORER.into())
                .trim_end_matches('/')
                .to_string(),
            env_address: std::env::var("BTC_TESTNET_ADDRESS").ok(),
            env_wif: std::env::var("BTC_TESTNET_WIF").ok(),
        }
    }

    pub fn ensure_testnet(&self) -> Result<()> {
        if matches!(self.network, Network::Bitcoin) {
            bail!("refusing mainnet Bitcoin in lab-btc (set BTC_NETWORK=testnet)");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BtcStatus {
    pub network: String,
    pub esplora_api: String,
    pub tip_height: Option<u64>,
    pub tip_ok: bool,
    pub tip_detail: String,
    pub wallet: Option<String>,
    pub address: Option<String>,
    pub balance_sats: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BtcWalletInfo {
    pub name: String,
    pub network: String,
    pub address: String,
    pub script_type: String,
    pub wif_path: String,
    pub explorer_url: String,
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BtcUtxo {
    pub outpoint: String,
    pub txid: String,
    pub vout: u32,
    pub value_sats: u64,
    pub confirmed: bool,
    pub block_height: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BtcBalance {
    pub name: String,
    pub address: String,
    pub balance_sats: u64,
    pub utxo_count: usize,
    pub explorer_url: String,
}

fn http() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?)
}

pub fn tip_height(btc: &BtcConfig) -> Result<u64> {
    let url = format!("{}/blocks/tip/height", btc.esplora_api);
    let body = http()?
        .get(&url)
        .send()
        .with_context(|| format!("GET {url}"))?
        .error_for_status()?
        .text()?;
    Ok(body.trim().parse()?)
}

pub fn network_status(cfg: &Config, btc: &BtcConfig) -> Result<BtcStatus> {
    btc.ensure_testnet()?;
    let tip = tip_height(btc);
    let (tip_height, tip_ok, tip_detail) = match tip {
        Ok(h) => (Some(h), true, format!("height={h}")),
        Err(e) => (None, false, e.to_string()),
    };

    let mut status = BtcStatus {
        network: format!("{:?}", btc.network).to_ascii_lowercase(),
        esplora_api: btc.esplora_api.clone(),
        tip_height,
        tip_ok,
        tip_detail,
        wallet: None,
        address: None,
        balance_sats: None,
    };

    // Prefer imported btc-alice if present
    if wallet_exists(cfg, DEFAULT_NAME) {
        if let Ok(info) = load_wallet_address(cfg, btc, DEFAULT_NAME) {
            status.wallet = Some(DEFAULT_NAME.into());
            status.address = Some(info.address.clone());
            if let Ok(bal) = balance(cfg, btc, DEFAULT_NAME) {
                status.balance_sats = Some(bal.balance_sats);
            }
        }
    } else if let Some(addr) = &btc.env_address {
        status.address = Some(addr.clone());
        if let Ok(u) = address_utxos(btc, addr) {
            status.balance_sats = Some(u.iter().map(|x| x.value_sats).sum());
        }
    }
    Ok(status)
}

fn wallet_dir(cfg: &Config, name: &str) -> PathBuf {
    cfg.wallet_path(name)
}

pub fn wallet_exists(cfg: &Config, name: &str) -> bool {
    wallet_dir(cfg, name).join("wif").exists()
        || wallet_dir(cfg, name).join("mnemonic").exists()
}

/// Import a WIF into `.rgbmvp/wallets/<name>/` and verify derived address.
pub fn import_wif(
    cfg: &Config,
    btc: &BtcConfig,
    name: &str,
    wif: &str,
    expect_address: Option<&str>,
    force: bool,
) -> Result<BtcWalletInfo> {
    btc.ensure_testnet()?;
    cfg.ensure_dirs()?;
    let name = if name.is_empty() { DEFAULT_NAME } else { name };
    let dir = wallet_dir(cfg, name);
    let wif_path = dir.join("wif");
    if wif_path.exists() && !force {
        bail!(
            "btc wallet {name:?} already exists at {}; pass --force to overwrite",
            dir.display()
        );
    }
    std::fs::create_dir_all(&dir)?;

    let sk = PrivateKey::from_wif(wif.trim()).context("parse WIF")?;
    // WIF network kind must match lab BTC network (testnet/signet/regtest vs main).
    let wif_mainnet = sk.network == bitcoin::NetworkKind::Main;
    let want_mainnet = btc.network == Network::Bitcoin;
    if wif_mainnet != want_mainnet {
        bail!(
            "WIF network mismatch: key is for {} but lab BTC_NETWORK is {:?}",
            if wif_mainnet { "mainnet" } else { "testnet-family" },
            btc.network
        );
    }
    let addr = p2wpkh_address(btc, &sk)?;
    if let Some(exp) = expect_address {
        if exp != addr {
            bail!("WIF derives {addr}, but expected address {exp}");
        }
    }
    if let Some(env_addr) = &btc.env_address {
        if name == DEFAULT_NAME && env_addr != &addr {
            bail!(
                "WIF derives {addr}, but BTC_TESTNET_ADDRESS is {env_addr}"
            );
        }
    }

    write_secret_file(&wif_path, wif.trim())?;
    write_secret_file(&dir.join("address"), &addr)?;
    let meta = serde_json::json!({
        "name": name,
        "role": name,
        "chain": "bitcoin",
        "network": format!("{:?}", btc.network),
        "script_type": "p2wpkh",
        "address": addr,
        "created_with": "lab-btc/import_wif",
    });
    std::fs::write(dir.join("meta.json"), serde_json::to_vec_pretty(&meta)?)?;

    Ok(BtcWalletInfo {
        name: name.into(),
        network: format!("{:?}", btc.network).to_ascii_lowercase(),
        address: addr.clone(),
        script_type: "p2wpkh".into(),
        wif_path: wif_path.display().to_string(),
        explorer_url: format!("{}/address/{}", btc.explorer_base, addr),
        note: "TESTNET ONLY. WIF stored under .rgbmvp (gitignored).".into(),
    })
}

/// Import `btc-alice` from `BTC_TESTNET_WIF` / `BTC_TESTNET_ADDRESS` env.
pub fn import_from_env(cfg: &Config, btc: &BtcConfig, force: bool) -> Result<BtcWalletInfo> {
    let wif = btc
        .env_wif
        .as_deref()
        .context("BTC_TESTNET_WIF not set in environment/.env")?;
    let expect = btc.env_address.as_deref();
    import_wif(cfg, btc, DEFAULT_NAME, wif, expect, force)
}

/// Load a wallet's WIF for signing (never printed to logs).
///
/// W3: prefers a mounted Secret Manager entry (`RGBMVP_SECRET_DIR`) over the
/// local wallet copy, so public deployments never sign with image-baked keys.
fn load_wif(cfg: &Config, name: &str) -> Result<PrivateKey> {
    let fallback = wallet_dir(cfg, name);
    let path = lab_core::resolve_secret_path(
        &lab_core::secret_dirs(),
        &fallback,
        name,
        lab_core::KIND_WIF,
    )
    .ok_or_else(|| {
        anyhow::anyhow!(
            "btc wallet {name:?} missing WIF at {} (and no RGBMVP_SECRET_DIR mount); \
             run: rgbmvp btc import-env",
            fallback.join("wif").display()
        )
    })?;
    let s = lab_core::read_trimmed(&path)?;
    PrivateKey::from_wif(&s).context("parse stored WIF")
}

fn p2wpkh_address(btc: &BtcConfig, sk: &PrivateKey) -> Result<String> {
    let secp = Secp256k1::new();
    let compressed = CompressedPublicKey::from_private_key(&secp, sk)
        .map_err(|e| anyhow::anyhow!("compressed pubkey: {e}"))?;
    let addr = Address::p2wpkh(&compressed, btc.network);
    Ok(addr.to_string())
}

pub fn load_wallet_address(cfg: &Config, btc: &BtcConfig, name: &str) -> Result<BtcWalletInfo> {
    let sk = load_wif(cfg, name)?;
    let addr = p2wpkh_address(btc, &sk)?;
    let path = wallet_dir(cfg, name).join("wif");
    Ok(BtcWalletInfo {
        name: name.into(),
        network: format!("{:?}", btc.network).to_ascii_lowercase(),
        address: addr.clone(),
        script_type: "p2wpkh".into(),
        wif_path: path.display().to_string(),
        explorer_url: format!("{}/address/{}", btc.explorer_base, addr),
        note: "loaded from disk".into(),
    })
}

pub fn address_utxos(btc: &BtcConfig, address: &str) -> Result<Vec<BtcUtxo>> {
    let url = format!("{}/address/{}/utxo", btc.esplora_api, address);
    let v: serde_json::Value = http()?
        .get(&url)
        .send()
        .with_context(|| format!("GET {url}"))?
        .error_for_status()?
        .json()?;
    let arr = v.as_array().context("utxo array")?;
    let mut out = Vec::new();
    for u in arr {
        let txid = u["txid"].as_str().unwrap_or_default().to_string();
        let vout = u["vout"].as_u64().unwrap_or(0) as u32;
        let value = u["value"].as_u64().unwrap_or(0);
        let confirmed = u["status"]["confirmed"].as_bool().unwrap_or(false);
        let block_height = u["status"]["block_height"].as_u64();
        out.push(BtcUtxo {
            outpoint: format!("{txid}:{vout}"),
            txid,
            vout,
            value_sats: value,
            confirmed,
            block_height,
        });
    }
    out.sort_by(|a, b| b.value_sats.cmp(&a.value_sats));
    Ok(out)
}

pub fn utxos(cfg: &Config, btc: &BtcConfig, name: &str) -> Result<Vec<BtcUtxo>> {
    let info = load_wallet_address(cfg, btc, name)?;
    address_utxos(btc, &info.address)
}

pub fn balance(cfg: &Config, btc: &BtcConfig, name: &str) -> Result<BtcBalance> {
    let info = load_wallet_address(cfg, btc, name)?;
    let utxos = address_utxos(btc, &info.address)?;
    let sum: u64 = utxos.iter().map(|u| u.value_sats).sum();
    Ok(BtcBalance {
        name: name.into(),
        address: info.address.clone(),
        balance_sats: sum,
        utxo_count: utxos.len(),
        explorer_url: info.explorer_url,
    })
}

/// Expose secp secret for later signing (P1 fund/claim). Prefer not to log.
pub fn load_secret_key(cfg: &Config, name: &str) -> Result<SecretKey> {
    let pk = load_wif(cfg, name)?;
    Ok(pk.inner)
}

pub fn parse_address(btc: &BtcConfig, s: &str) -> Result<Address> {
    let a: Address<NetworkUnchecked> = Address::from_str(s).context("parse address")?;
    let a = a
        .require_network(btc.network)
        .map_err(|e| anyhow::anyhow!("address network: {e}"))?;
    Ok(a)
}

pub fn wallet_path(cfg: &Config, name: &str) -> PathBuf {
    wallet_dir(cfg, name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_wif_parse() {
        assert!(PrivateKey::from_wif("not-a-wif").is_err());
    }
}


// ── Broadcast P2WPKH spend with commitment + change (P1 RGB anchor) ─────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BtcBroadcastResult {
    pub txid: String,
    pub explorer_url: String,
    pub fee_sats: u64,
    pub commitment_address: String,
    pub change_address: String,
    pub note: String,
}

/// Spend seal UTXO: vout0 = tapret commitment, vout1 = change to same wallet.
pub fn broadcast_commitment_tx(
    cfg: &Config,
    btc: &BtcConfig,
    name: &str,
    seal_outpoint: &str,
    seal_value_sats: u64,
    commitment_address: &str,
    commitment_sats: u64,
    fee_sats: u64,
) -> Result<BtcBroadcastResult> {
    use bitcoin::absolute::LockTime;
    use bitcoin::consensus::encode::serialize_hex;
    use bitcoin::hashes::Hash;
    use bitcoin::key::Secp256k1;
    use bitcoin::secp256k1::Message;
    use bitcoin::sighash::{EcdsaSighashType, SighashCache};
    use bitcoin::transaction::Version;
    use bitcoin::{Amount, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid, Witness};

    btc.ensure_testnet()?;
    let sk = load_wif(cfg, name)?;
    let secp = Secp256k1::new();
    let compressed = CompressedPublicKey::from_private_key(&secp, &sk)
        .map_err(|e| anyhow::anyhow!("pubkey: {e}"))?;
    let change_addr = Address::p2wpkh(&compressed, btc.network);
    let input_script = change_addr.script_pubkey();

    let commit_addr = parse_address(btc, commitment_address)?;
    let commit_spk = commit_addr.script_pubkey();

    let (txid_s, vout_s) = seal_outpoint
        .split_once(':')
        .context("seal outpoint")?;
    let prev_txid: Txid = txid_s.parse().context("txid")?;
    let prev_vout: u32 = vout_s.parse()?;

    if commitment_sats + fee_sats >= seal_value_sats {
        bail!("commitment+fee must be < seal value ({seal_value_sats})");
    }
    let change_sats = seal_value_sats - commitment_sats - fee_sats;

    let mut tx = Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint::new(prev_txid, prev_vout),
            script_sig: ScriptBuf::new(),
            sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
            witness: Witness::new(),
        }],
        output: vec![
            TxOut {
                value: Amount::from_sat(commitment_sats),
                script_pubkey: commit_spk,
            },
            TxOut {
                value: Amount::from_sat(change_sats),
                script_pubkey: input_script.clone(),
            },
        ],
    };

    let sighash = SighashCache::new(&tx)
        .p2wpkh_signature_hash(
            0,
            &input_script,
            Amount::from_sat(seal_value_sats),
            EcdsaSighashType::All,
        )
        .context("p2wpkh sighash")?;

    let msg = Message::from_digest(sighash.to_byte_array());
    let sig = secp.sign_ecdsa(&msg, &sk.inner);
    let mut sig_bytes = sig.serialize_der().to_vec();
    sig_bytes.push(EcdsaSighashType::All as u8);
    let pk = compressed.to_bytes();
    tx.input[0].witness = Witness::from_slice(&[sig_bytes, pk.to_vec()]);

    let hex_tx = serialize_hex(&tx);
    let txid = broadcast_raw(btc, &hex_tx)?;

    Ok(BtcBroadcastResult {
        txid: txid.clone(),
        explorer_url: format!("{}/tx/{}", btc.explorer_base, txid),
        fee_sats,
        commitment_address: commitment_address.into(),
        change_address: change_addr.to_string(),
        note: "BTC RGB commitment broadcast (vout0=tapret, vout1=change)".into(),
    })
}

pub fn broadcast_raw(btc: &BtcConfig, tx_hex: &str) -> Result<String> {
    let url = format!("{}/tx", btc.esplora_api);
    let resp = http()?
        .post(&url)
        .header("Content-Type", "text/plain")
        .body(tx_hex.to_string())
        .send()
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    let body = resp.text()?;
    if !status.is_success() {
        bail!("broadcast failed HTTP {status}: {body}");
    }
    Ok(body.trim().to_string())
}

/// Fetch witness for RGB verify (vin outpoints + output scriptPubKeys).
pub fn fetch_witness_esplora(btc: &BtcConfig, txid: &str) -> Result<(String, Vec<(String, u32)>, Vec<String>)> {
    let url = format!("{}/tx/{}", btc.esplora_api, txid);
    let v: serde_json::Value = http()?
        .get(&url)
        .send()
        .with_context(|| format!("GET {url}"))?
        .error_for_status()?
        .json()?;
    let vin_outpoints = v["vin"]
        .as_array()
        .context("vin")?
        .iter()
        .map(|x| {
            (
                x["txid"].as_str().unwrap_or_default().to_lowercase(),
                x["vout"].as_u64().unwrap_or(0) as u32,
            )
        })
        .collect();
    let vouts_spk_hex = v["vout"]
        .as_array()
        .context("vout")?
        .iter()
        .map(|x| x["scriptpubkey"].as_str().unwrap_or_default().to_lowercase())
        .collect();
    Ok((txid.to_lowercase(), vin_outpoints, vouts_spk_hex))
}

pub fn fetch_witness_for_rgb(btc: &BtcConfig, txid: &str) -> Result<lab_rgb::seal::WitnessTx> {
    let (txid, vin, vouts) = fetch_witness_esplora(btc, txid)?;
    Ok(lab_rgb::seal::WitnessTx {
        txid,
        vin_outpoints: vin,
        vouts_spk_hex: vouts,
    })
}

/// Raw transaction hex from Esplora (`/tx/{txid}/hex`).
pub fn fetch_tx_hex(btc: &BtcConfig, txid: &str) -> Result<String> {
    let url = format!("{}/tx/{}/hex", btc.esplora_api.trim_end_matches('/'), txid);
    let body = http()?
        .get(&url)
        .send()
        .with_context(|| format!("GET {url}"))?
        .error_for_status()?
        .text()?;
    Ok(body.trim().to_string())
}

pub fn pick_largest_utxo(cfg: &Config, btc: &BtcConfig, name: &str) -> Result<BtcUtxo> {
    utxos(cfg, btc, name)?
        .into_iter()
        .filter(|x| x.value_sats >= 5000)
        .max_by_key(|x| x.value_sats)
        .ok_or_else(|| anyhow::anyhow!("no UTXO ≥ 5000 sats in {name}"))
}

/// Send `amount_sats` from named wallet to an arbitrary address (P2WPKH or P2WSH).
pub fn fund_address(
    cfg: &Config,
    btc: &BtcConfig,
    name: &str,
    to_address: &str,
    amount_sats: u64,
    fee_sats: u64,
) -> Result<BtcBroadcastResult> {
    use bitcoin::absolute::LockTime;
    use bitcoin::consensus::encode::serialize_hex;
    use bitcoin::hashes::Hash;
    use bitcoin::key::Secp256k1;
    use bitcoin::secp256k1::Message;
    use bitcoin::sighash::{EcdsaSighashType, SighashCache};
    use bitcoin::transaction::Version;
    use bitcoin::{Amount, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid, Witness};

    btc.ensure_testnet()?;
    let utxo = pick_largest_utxo(cfg, btc, name)?;
    if amount_sats + fee_sats >= utxo.value_sats {
        bail!(
            "need amount+fee < utxo ({} + {} >= {})",
            amount_sats,
            fee_sats,
            utxo.value_sats
        );
    }
    let change_sats = utxo.value_sats - amount_sats - fee_sats;

    let sk = load_wif(cfg, name)?;
    let secp = Secp256k1::new();
    let compressed = CompressedPublicKey::from_private_key(&secp, &sk)
        .map_err(|e| anyhow::anyhow!("pubkey: {e}"))?;
    let change_addr = Address::p2wpkh(&compressed, btc.network);
    let input_script = change_addr.script_pubkey();
    let dest = parse_address(btc, to_address)?;

    let prev_txid: Txid = utxo.txid.parse()?;
    let mut tx = Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint::new(prev_txid, utxo.vout),
            script_sig: ScriptBuf::new(),
            sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
            witness: Witness::new(),
        }],
        output: vec![
            TxOut {
                value: Amount::from_sat(amount_sats),
                script_pubkey: dest.script_pubkey(),
            },
            TxOut {
                value: Amount::from_sat(change_sats),
                script_pubkey: input_script.clone(),
            },
        ],
    };

    let sighash = SighashCache::new(&tx)
        .p2wpkh_signature_hash(
            0,
            &input_script,
            Amount::from_sat(utxo.value_sats),
            EcdsaSighashType::All,
        )
        .context("sighash")?;
    let msg = Message::from_digest(sighash.to_byte_array());
    let sig = secp.sign_ecdsa(&msg, &sk.inner);
    let mut sig_bytes = sig.serialize_der().to_vec();
    sig_bytes.push(EcdsaSighashType::All as u8);
    tx.input[0].witness = Witness::from_slice(&[sig_bytes, compressed.to_bytes().to_vec()]);

    let hex_tx = serialize_hex(&tx);
    let txid = broadcast_raw(btc, &hex_tx)?;
    Ok(BtcBroadcastResult {
        txid: txid.clone(),
        explorer_url: format!("{}/tx/{}", btc.explorer_base, txid),
        fee_sats,
        commitment_address: to_address.into(),
        change_address: change_addr.to_string(),
        note: format!("funded {amount_sats} sats to {to_address}"),
    })
}

/// Find HTLC UTXO on address by scanning Esplora (value match preferred).
pub fn find_htlc_utxo(btc: &BtcConfig, address: &str, min_sats: u64) -> Result<BtcUtxo> {
    address_utxos(btc, address)?
        .into_iter()
        .find(|u| u.value_sats >= min_sats)
        .ok_or_else(|| anyhow::anyhow!("no UTXO ≥ {min_sats} on {address}"))
}

// ---------------------------------------------------------------------------
// T1/W5 — sweep demo HTLC exit addresses back into the funding wallet.
//
// Every HTLC exit path (claim AND refund, both chains) pays a P2WPKH address
// derived from the custody-backed demo-exit seed plus a public role label — NOT
// back to the wallet that funded the leg. Without a sweep, btc-alice drains on
// every swap regardless of outcome and the value strands at four addresses
// fixed for the lifetime of that seed.
// ---------------------------------------------------------------------------

/// Labels every BTC-side HTLC exit pays out to.
pub const BTC_DEMO_EXIT_LABELS: [&str; 2] = ["bob-claimer", "alice-refund"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SweepResult {
    pub label: String,
    pub address: String,
    pub utxo_count: usize,
    pub swept_sats: u64,
    pub fee_sats: u64,
    pub txid: Option<String>,
    pub explorer_url: Option<String>,
    pub skipped_reason: Option<String>,
}

/// P2WPKH address a demo label receives at.
pub fn demo_exit_address(
    btc: &BtcConfig,
    keyring: &lab_rgb::htlc::DemoKeyring,
    label: &str,
) -> Result<(SecretKey, Address)> {
    let (sk, _) = keyring.derive(label)?;
    let secp = Secp256k1::new();
    let pk = bitcoin::secp256k1::PublicKey::from_secret_key(&secp, &sk);
    let compressed = CompressedPublicKey(pk);
    Ok((sk, Address::p2wpkh(&compressed, btc.network)))
}

/// Sweep every confirmed UTXO at one demo exit address into `to_address`.
///
/// Consolidates all inputs into a single output, so accumulated dust from many
/// swaps is recovered in one transaction. Skips (rather than fails) when there
/// is nothing to sweep or the balance cannot cover the fee.
pub fn sweep_demo_exit(
    btc: &BtcConfig,
    keyring: &lab_rgb::htlc::DemoKeyring,
    label: &str,
    to_address: &str,
    fee_sats: u64,
) -> Result<SweepResult> {
    use bitcoin::absolute::LockTime;
    use bitcoin::consensus::encode::serialize_hex;
    use bitcoin::hashes::Hash;
    use bitcoin::secp256k1::Message;
    use bitcoin::sighash::{EcdsaSighashType, SighashCache};
    use bitcoin::transaction::Version;
    use bitcoin::{Amount, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid, Witness};

    btc.ensure_testnet()?;
    let (sk, from_addr) = demo_exit_address(btc, keyring, label)?;
    let spk = from_addr.script_pubkey();
    let address = from_addr.to_string();

    // Only confirmed UTXOs: an unconfirmed claim output may still be replaced.
    let utxos: Vec<BtcUtxo> = address_utxos(btc, &address)?
        .into_iter()
        .filter(|u| u.confirmed)
        .collect();
    let total: u64 = utxos.iter().map(|u| u.value_sats).sum();

    let mut result = SweepResult {
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
        result.skipped_reason = Some("no confirmed utxos".into());
        return Ok(result);
    }
    // Dust guard: sweeping for less than the fee would destroy value.
    if total <= fee_sats + 294 {
        result.skipped_reason = Some(format!(
            "balance {total} does not cover fee {fee_sats} plus dust threshold"
        ));
        return Ok(result);
    }

    let dest = parse_address(btc, to_address)?;
    let out_sats = total - fee_sats;

    let mut tx = Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input: utxos
            .iter()
            .map(|u| {
                Ok(TxIn {
                    previous_output: OutPoint::new(u.txid.parse::<Txid>()?, u.vout),
                    script_sig: ScriptBuf::new(),
                    sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                    witness: Witness::new(),
                })
            })
            .collect::<Result<Vec<_>>>()?,
        output: vec![TxOut {
            value: Amount::from_sat(out_sats),
            script_pubkey: dest.script_pubkey(),
        }],
    };

    let secp = Secp256k1::new();
    let pk = bitcoin::secp256k1::PublicKey::from_secret_key(&secp, &sk);
    let compressed = CompressedPublicKey(pk);
    // Each input is signed independently over its own prevout value.
    let mut witnesses = Vec::with_capacity(utxos.len());
    for (i, u) in utxos.iter().enumerate() {
        let sighash = SighashCache::new(&tx)
            .p2wpkh_signature_hash(
                i,
                &spk,
                Amount::from_sat(u.value_sats),
                EcdsaSighashType::All,
            )
            .with_context(|| format!("sighash input {i}"))?;
        let msg = Message::from_digest(sighash.to_byte_array());
        let sig = secp.sign_ecdsa(&msg, &sk);
        let mut sig_bytes = sig.serialize_der().to_vec();
        sig_bytes.push(EcdsaSighashType::All as u8);
        witnesses.push(Witness::from_slice(&[
            sig_bytes,
            compressed.to_bytes().to_vec(),
        ]));
    }
    for (i, w) in witnesses.into_iter().enumerate() {
        tx.input[i].witness = w;
    }

    let hex_tx = serialize_hex(&tx);
    let txid = broadcast_raw(btc, &hex_tx)?;
    result.swept_sats = out_sats;
    result.txid = Some(txid.clone());
    result.explorer_url = Some(format!("{}/tx/{}", btc.explorer_base, txid));
    Ok(result)
}

/// Sweep all BTC demo exit addresses back into `to_wallet`.
pub fn sweep_all_demo_exits(
    cfg: &Config,
    btc: &BtcConfig,
    to_wallet: &str,
    fee_sats: u64,
) -> Result<Vec<SweepResult>> {
    let info = load_wallet_address(cfg, btc, to_wallet)?;
    let keyring = lab_rgb::htlc::DemoKeyring::new(cfg.demo_exit_seed()?)?;
    let mut out = Vec::new();
    for label in BTC_DEMO_EXIT_LABELS {
        match sweep_demo_exit(btc, &keyring, label, &info.address, fee_sats) {
            Ok(r) => out.push(r),
            Err(e) => out.push(SweepResult {
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
