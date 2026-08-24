//! S3 fund-wrap and claim orchestration (sync).
//!
//! Shared by CLI and future labd so claim construction is not duplicated in HTTP
//! handlers or Clap arms. Chain I/O stays here; session phase recompute/save
//! remains the caller's responsibility after mutation.

use anyhow::{Context, Result};
use lab_core::Config;
use lab_rgb::htlc;
use lab_rgb::storage::RgbStore;
use lab_rgb::swap::SwapSession;
use lab_rgb::{
    plan_claim_transfer, plan_transfer_to_seal, verify_against_witness, TransferPlan, VerifyResult,
    DEMO_INTERNAL_XONLY_HEX,
};
use serde_json::{json, Value};

/// Liquid testnet policy asset (explicit L-BTC).
pub const LQ_POLICY_ASSET: &str =
    "144c654344aa716d6f3abcc1ca90e5641e4e2a7f633bc09fe3baf64585819a49";

trait ClaimBroadcaster {
    fn broadcast(&self, raw_hex: &str) -> Result<String>;
}

trait ClaimVerifier {
    fn verify(&self, plan: &TransferPlan, txid: &str) -> Result<Option<VerifyResult>>;
}

/// Keep broadcast-before-verify ordering explicit and independently testable.
/// A successful broadcast with no witness yet is represented by `None`, never
/// by a false `valid` result.
fn execute_claim_io(
    broadcaster: &impl ClaimBroadcaster,
    verifier: &impl ClaimVerifier,
    raw_hex: &str,
    plan: &TransferPlan,
) -> Result<(String, Option<VerifyResult>)> {
    let txid = broadcaster.broadcast(raw_hex)?;
    let verification = verifier.verify(plan, &txid)?;
    Ok((txid, verification))
}

struct LiquidClaimIo<'a> {
    cfg: &'a Config,
}

impl ClaimBroadcaster for LiquidClaimIo<'_> {
    fn broadcast(&self, raw_hex: &str) -> Result<String> {
        lab_chain::broadcast_raw_hex(self.cfg, raw_hex)
    }
}

impl ClaimVerifier for LiquidClaimIo<'_> {
    fn verify(&self, plan: &TransferPlan, txid: &str) -> Result<Option<VerifyResult>> {
        let api = lab_chain::esplora_api_base(self.cfg);
        for _ in 0..5 {
            std::thread::sleep(std::time::Duration::from_millis(400));
            if let Ok(witness) = lab_chain::fetch_witness_esplora(&api, txid) {
                return verify_against_witness(plan, &witness, &self.cfg.explorer_base).map(Some);
            }
        }
        Ok(None)
    }
}

struct BtcClaimIo<'a> {
    cfg: &'a lab_btc::BtcConfig,
}

impl ClaimBroadcaster for BtcClaimIo<'_> {
    fn broadcast(&self, raw_hex: &str) -> Result<String> {
        lab_btc::broadcast_raw(self.cfg, raw_hex)
    }
}

impl ClaimVerifier for BtcClaimIo<'_> {
    fn verify(&self, plan: &TransferPlan, txid: &str) -> Result<Option<VerifyResult>> {
        for _ in 0..5 {
            std::thread::sleep(std::time::Duration::from_millis(400));
            if let Ok(witness) = lab_btc::fetch_witness_for_rgb(self.cfg, txid) {
                return verify_against_witness(plan, &witness, &self.cfg.explorer_base).map(Some);
            }
        }
        Ok(None)
    }
}

/// Demo claimer P2WPKH (testnet) for HTLC claim outputs.
pub fn claimer_p2wpkh_spk(
    keyring: &htlc::DemoKeyring,
    info: &htlc::HtlcAddressInfo,
    label: &str,
) -> Result<(bitcoin::secp256k1::SecretKey, bitcoin::ScriptBuf, String)> {
    use bitcoin::key::{CompressedPublicKey, Secp256k1};
    use bitcoin::{Address, Network};
    let (sk, _) = keyring.derive_for_session(info, label)?;
    let secp = Secp256k1::new();
    let pk = bitcoin::secp256k1::PublicKey::from_secret_key(&secp, &sk);
    let compressed = CompressedPublicKey(pk);
    let dest = Address::p2wpkh(&compressed, Network::Testnet);
    Ok((sk, dest.script_pubkey(), dest.to_string()))
}

/// After value fund: plan transfer of full supply onto HTLC seal + broadcast BTC commitment.
pub fn fund_wrap_btc(
    cfg: &Config,
    btc: &lab_btc::BtcConfig,
    rgb_store: &RgbStore,
    s: &mut SwapSession,
    commitment_sats: u64,
    entropy: u64,
) -> Result<Value> {
    let cid = s
        .btc_contract_id
        .clone()
        .context("rgb_wrap BTC requires --btc-contract on init")?;
    let issue = rgb_store.load_issue(&cid)?;
    let fund_txid = s.btc_fund_txid.clone().context("btc fund txid")?;
    let fund_vout = s.btc_fund_vout.unwrap_or(0);
    let htlc_seal = format!("{fund_txid}:{fund_vout}");

    let plan = plan_transfer_to_seal(
        &issue.contract_id,
        issue.supply,
        issue.supply,
        &issue.seal,
        &htlc_seal,
        None,
        DEMO_INTERNAL_XONLY_HEX,
        entropy,
        &issue.ticker,
        "bitcoin-testnet",
    )?;
    let plan_id = format!(
        "s3-fund-btc-{}-{}",
        s.id,
        &plan.bundle_id_hex[..12.min(plan.bundle_id_hex.len())]
    );
    rgb_store.save_transfer(&plan_id, &plan)?;

    let utxos = lab_btc::utxos(cfg, btc, &s.alice_btc_wallet)?;
    let seal_val = utxos
        .iter()
        .find(|u| u.outpoint == issue.seal)
        .map(|u| u.value_sats)
        .context(
            "BTC issue seal UTXO not found in alice wallet (issue then fund-wrap before spending seal)",
        )?;
    let fee = 800u64;
    let bc = lab_btc::broadcast_commitment_tx(
        cfg,
        btc,
        &s.alice_btc_wallet,
        &issue.seal,
        seal_val,
        &plan.tapret_address,
        commitment_sats,
        fee,
    )?;

    let mut fund_verify = None;
    if let Ok(w) = lab_btc::fetch_witness_for_rgb(btc, &bc.txid) {
        if let Ok(vr) = verify_against_witness(&plan, &w, &btc.explorer_base) {
            fund_verify = Some(vr.status.clone());
            let _ = rgb_store.save_proof(&format!("{plan_id}-fund"), &vr);
        }
    }

    if let Some(leg) = s.btc_rgb.as_mut() {
        leg.contract_id = issue.contract_id.clone();
        leg.amount = issue.supply;
        leg.issue_seal = Some(issue.seal.clone());
        leg.htlc_seal = Some(htlc_seal.clone());
        leg.fund_plan_id = Some(plan_id.clone());
        leg.fund_anchor_txid = Some(bc.txid.clone());
        leg.fund_verify = fund_verify.clone();
        leg.fund_transition_opid_hex = Some(plan.transition_opid_hex.clone());
    }
    s.notes
        .push(format!("S3 BTC fund-wrap plan={plan_id} seal={htlc_seal}"));

    Ok(json!({
        "plan_id": plan_id,
        "htlc_seal": htlc_seal,
        "fund_anchor": bc,
        "fund_verify": fund_verify,
        "plan": plan,
    }))
}

/// After value fund: plan transfer onto Liquid HTLC seal + broadcast commitment.
pub fn fund_wrap_lq(
    cfg: &Config,
    rgb_store: &RgbStore,
    s: &mut SwapSession,
    commitment_sats: u64,
    entropy: u64,
) -> Result<Value> {
    let cid = s
        .lq_contract_id
        .clone()
        .context("rgb_wrap LQ requires --lq-contract on init")?;
    let issue = rgb_store.load_issue(&cid)?;
    let fund_txid = s.lq_fund_txid.clone().context("lq fund txid")?;
    let fund_vout = s.lq_fund_vout.unwrap_or(0);
    let htlc_seal = format!("{fund_txid}:{fund_vout}");

    let plan = plan_transfer_to_seal(
        &issue.contract_id,
        issue.supply,
        issue.supply,
        &issue.seal,
        &htlc_seal,
        None,
        DEMO_INTERNAL_XONLY_HEX,
        entropy,
        &issue.ticker,
        "liquid-testnet",
    )?;
    let plan_id = format!(
        "s3-fund-lq-{}-{}",
        s.id,
        &plan.bundle_id_hex[..12.min(plan.bundle_id_hex.len())]
    );
    rgb_store.save_transfer(&plan_id, &plan)?;

    let bc = lab_chain::broadcast_commitment_tx(
        cfg,
        &s.bob_lq_wallet,
        &issue.seal,
        &plan.tapret_address,
        None,
        commitment_sats,
        0,
    )?;

    let mut fund_verify = None;
    let api = lab_chain::esplora_api_base(cfg);
    if let Ok(w) = lab_chain::fetch_witness_esplora(&api, &bc.txid) {
        if let Ok(vr) = verify_against_witness(&plan, &w, &cfg.explorer_base) {
            fund_verify = Some(vr.status.clone());
            let _ = rgb_store.save_proof(&format!("{plan_id}-fund"), &vr);
        }
    }

    if let Some(leg) = s.lq_rgb.as_mut() {
        leg.contract_id = issue.contract_id.clone();
        leg.amount = issue.supply;
        leg.issue_seal = Some(issue.seal.clone());
        leg.htlc_seal = Some(htlc_seal.clone());
        leg.fund_plan_id = Some(plan_id.clone());
        leg.fund_anchor_txid = Some(bc.txid.clone());
        leg.fund_verify = fund_verify.clone();
        leg.fund_transition_opid_hex = Some(plan.transition_opid_hex.clone());
    }
    s.notes
        .push(format!("S3 LQ fund-wrap plan={plan_id} seal={htlc_seal}"));

    Ok(json!({
        "plan_id": plan_id,
        "htlc_seal": htlc_seal,
        "fund_anchor": bc,
        "fund_verify": fund_verify,
        "plan": plan,
    }))
}

/// Value-only Liquid HTLC claim (P1 path).
pub fn claim_lq_value(cfg: &Config, s: &mut SwapSession, fee_sats: u64) -> Result<Value> {
    let keyring = htlc::DemoKeyring::new(cfg.demo_exit_seed()?)?;
    let amount = s.lq_fund_sats.context("lq not funded (run fund-lq)")?;
    let (txid, vout, value) = lab_chain::find_address_utxo(
        cfg,
        &s.htlc_lq.address_liquid_unconf,
        amount.saturating_sub(1),
    )?;
    s.lq_fund_txid = Some(txid.clone());
    s.lq_fund_vout = Some(vout);
    s.lq_fund_sats = Some(value);

    let preimage = hex::decode(&s.preimage_hex)?;
    let (claimer_sk, dest_spk, dest_addr) =
        claimer_p2wpkh_spk(&keyring, &s.htlc_lq, &s.htlc_lq.claimer_label)?;
    let ws = hex::decode(&s.htlc_lq.witness_script_hex)?;
    let out_sats = value.saturating_sub(fee_sats);
    let raw = htlc::build_htlc_spend_liquid(
        &txid,
        vout,
        value,
        out_sats,
        fee_sats,
        dest_spk.as_bytes(),
        LQ_POLICY_ASSET,
        &ws,
        htlc::HtlcSpend::Claim {
            preimage: &preimage,
        },
        s.csv_delay,
        &claimer_sk,
    )?;
    let claim_txid = lab_chain::broadcast_raw_hex(cfg, &raw)?;
    s.lq_claim_txid = Some(claim_txid.clone());
    Ok(json!({
        "status": "claimed_lq",
        "phase": s.phase,
        "rgb_wrap": false,
        "txid": claim_txid,
        "dest": dest_addr,
        "explorer": format!("{}/tx/{}", cfg.explorer_base, claim_txid),
        "preimage_published": true,
        "note": "Preimage is public on Liquid; Bob can claim BTC.",
    }))
}

/// S3 RGB-wrapped Liquid claim: preimage + re-anchor + verify.
pub fn claim_lq_rgb(
    cfg: &Config,
    rgb_store: &RgbStore,
    s: &mut SwapSession,
    fee_sats: u64,
    commitment_sats: u64,
    entropy: u64,
) -> Result<Value> {
    let keyring = htlc::DemoKeyring::new(cfg.demo_exit_seed()?)?;
    lab_rgb::swap::require_fund_wrap_for_claim(s.lq_rgb.as_ref(), "lq")?;
    let leg = s.lq_rgb.as_ref().expect("checked").clone();
    lab_rgb::swap::check_leg_contract_matches_session(&leg, s.lq_contract_id.as_deref(), "lq")?;
    let prev_opid = leg
        .fund_transition_opid_hex
        .clone()
        .expect("checked by require_fund_wrap");
    let amount_rgb = if leg.amount > 0 {
        leg.amount
    } else {
        rgb_store.load_issue(&leg.contract_id)?.supply
    };

    let fund_amount = s.lq_fund_sats.context("lq not funded")?;
    let (txid, vout, value) = lab_chain::find_address_utxo(
        cfg,
        &s.htlc_lq.address_liquid_unconf,
        fund_amount.saturating_sub(1),
    )?;
    s.lq_fund_txid = Some(txid.clone());
    s.lq_fund_vout = Some(vout);
    s.lq_fund_sats = Some(value);
    let htlc_seal = format!("{txid}:{vout}");

    let plan = plan_claim_transfer(
        &leg.contract_id,
        &prev_opid,
        0,
        amount_rgb,
        amount_rgb,
        &htlc_seal,
        1,
        None,
        DEMO_INTERNAL_XONLY_HEX,
        entropy,
        "lRGB",
        "liquid-testnet",
    )?;
    let plan_id = format!(
        "s3-claim-lq-{}-{}",
        s.id,
        &plan.bundle_id_hex[..12.min(plan.bundle_id_hex.len())]
    );
    rgb_store.save_transfer(&plan_id, &plan)?;

    let commit_spk = hex::decode(&plan.commitment_spk_hex)?;
    let preimage = hex::decode(&s.preimage_hex)?;
    let (claimer_sk, dest_spk, dest_addr) =
        claimer_p2wpkh_spk(&keyring, &s.htlc_lq, &s.htlc_lq.claimer_label)?;
    let ws = hex::decode(&s.htlc_lq.witness_script_hex)?;
    if commitment_sats + fee_sats >= value {
        anyhow::bail!("commitment+fee must be < HTLC value");
    }
    let claimer_sats = value - commitment_sats - fee_sats;
    let raw = htlc::build_htlc_spend_liquid_outs(
        &txid,
        vout,
        value,
        &[
            (commitment_sats, commit_spk.as_slice()),
            (claimer_sats, dest_spk.as_bytes()),
        ],
        fee_sats,
        LQ_POLICY_ASSET,
        &ws,
        htlc::HtlcSpend::Claim {
            preimage: &preimage,
        },
        s.csv_delay,
        &claimer_sk,
    )?;
    let io = LiquidClaimIo { cfg };
    let (claim_txid, verification) = execute_claim_io(&io, &io, &raw, &plan)?;
    s.lq_claim_txid = Some(claim_txid.clone());
    let claim_verify = verification.as_ref().map(|result| result.status.clone());
    if let Some(result) = &verification {
        let _ = rgb_store.save_proof(&format!("{plan_id}-claim"), result);
    }

    if let Some(r) = s.lq_rgb.as_mut() {
        r.htlc_seal = Some(htlc_seal.clone());
        r.claim_plan_id = Some(plan_id.clone());
        r.claim_anchor_txid = Some(claim_txid.clone());
        r.claim_verify = claim_verify.clone();
        r.successor_seal = Some(format!("{claim_txid}:1"));
    }
    s.notes.push(format!(
        "S3 LQ claim plan={plan_id} verify={claim_verify:?}"
    ));

    Ok(json!({
        "status": "claimed_lq",
        "phase": s.phase,
        "rgb_wrap": true,
        "txid": claim_txid,
        "dest": dest_addr,
        "explorer": format!("{}/tx/{}", cfg.explorer_base, claim_txid),
        "preimage_published": true,
        "claim_plan_id": plan_id,
        "claim_verify": claim_verify,
        "successor_seal": format!("{claim_txid}:1"),
        "note": "Preimage public on Liquid; Bob can extract-preimage / claim-btc --from-witness.",
    }))
}

/// Dispatch Liquid claim: RGB wrap when session has `rgb_wrap` + lq contract.
pub fn claim_lq(
    cfg: &Config,
    rgb_store: &RgbStore,
    s: &mut SwapSession,
    fee_sats: u64,
    commitment_sats: u64,
    entropy: u64,
) -> Result<Value> {
    if s.rgb_wrap && s.lq_contract_id.is_some() {
        claim_lq_rgb(cfg, rgb_store, s, fee_sats, commitment_sats, entropy)
    } else {
        claim_lq_value(cfg, s, fee_sats)
    }
}

/// Value-only BTC HTLC claim (P1 path).
pub fn claim_btc_value(
    cfg: &Config,
    s: &mut SwapSession,
    preimage: &[u8],
    fee_sats: u64,
) -> Result<Value> {
    let keyring = htlc::DemoKeyring::new(cfg.demo_exit_seed()?)?;
    let btc = lab_btc::BtcConfig::from_env();
    let amount = s.btc_fund_sats.context("btc_fund_sats")?;
    let utxo = lab_btc::find_htlc_utxo(&btc, &s.htlc_btc.address_btc, amount.saturating_sub(1))?;
    let (claimer_sk, dest_spk, dest_addr) =
        claimer_p2wpkh_spk(&keyring, &s.htlc_btc, &s.htlc_btc.claimer_label)?;
    let ws = hex::decode(&s.htlc_btc.witness_script_hex)?;
    let out_sats = utxo.value_sats.saturating_sub(fee_sats);
    let raw = htlc::build_htlc_spend_btc(
        &utxo.txid,
        utxo.vout,
        utxo.value_sats,
        out_sats,
        dest_spk.as_bytes(),
        &ws,
        htlc::HtlcSpend::Claim { preimage },
        s.csv_delay,
        &claimer_sk,
    )?;
    let txid = lab_btc::broadcast_raw(&btc, &raw)?;
    s.btc_claim_txid = Some(txid.clone());
    Ok(json!({
        "status": "claimed_btc",
        "phase": s.phase,
        "rgb_wrap": false,
        "txid": txid,
        "dest": dest_addr,
        "explorer": format!("{}/tx/{}", btc.explorer_base, txid),
    }))
}

/// S3 RGB-wrapped BTC claim.
pub fn claim_btc_rgb(
    cfg: &Config,
    rgb_store: &RgbStore,
    s: &mut SwapSession,
    preimage: &[u8],
    fee_sats: u64,
    commitment_sats: u64,
    entropy: u64,
) -> Result<Value> {
    let keyring = htlc::DemoKeyring::new(cfg.demo_exit_seed()?)?;
    let btc = lab_btc::BtcConfig::from_env();
    lab_rgb::swap::require_fund_wrap_for_claim(s.btc_rgb.as_ref(), "btc")?;
    let leg = s.btc_rgb.as_ref().expect("checked").clone();
    lab_rgb::swap::check_leg_contract_matches_session(&leg, s.btc_contract_id.as_deref(), "btc")?;
    let prev_opid = leg
        .fund_transition_opid_hex
        .clone()
        .expect("checked by require_fund_wrap");
    let amount_rgb = if leg.amount > 0 {
        leg.amount
    } else {
        rgb_store.load_issue(&leg.contract_id)?.supply
    };
    let amount = s.btc_fund_sats.context("btc_fund_sats")?;
    let utxo = lab_btc::find_htlc_utxo(&btc, &s.htlc_btc.address_btc, amount.saturating_sub(1))?;
    let htlc_seal = format!("{}:{}", utxo.txid, utxo.vout);

    let plan = plan_claim_transfer(
        &leg.contract_id,
        &prev_opid,
        0,
        amount_rgb,
        amount_rgb,
        &htlc_seal,
        1,
        None,
        DEMO_INTERNAL_XONLY_HEX,
        entropy,
        "bRGB",
        "bitcoin-testnet",
    )?;
    let plan_id = format!(
        "s3-claim-btc-{}-{}",
        s.id,
        &plan.bundle_id_hex[..12.min(plan.bundle_id_hex.len())]
    );
    rgb_store.save_transfer(&plan_id, &plan)?;

    let commit_spk = hex::decode(&plan.commitment_spk_hex)?;
    let (claimer_sk, dest_spk, dest_addr) =
        claimer_p2wpkh_spk(&keyring, &s.htlc_btc, &s.htlc_btc.claimer_label)?;
    let ws = hex::decode(&s.htlc_btc.witness_script_hex)?;
    if commitment_sats + fee_sats >= utxo.value_sats {
        anyhow::bail!("commitment+fee must be < HTLC value");
    }
    let claimer_sats = utxo.value_sats - commitment_sats - fee_sats;
    let raw = htlc::build_htlc_spend_btc_outs(
        &utxo.txid,
        utxo.vout,
        utxo.value_sats,
        &[
            (commitment_sats, commit_spk.as_slice()),
            (claimer_sats, dest_spk.as_bytes()),
        ],
        &ws,
        htlc::HtlcSpend::Claim { preimage },
        s.csv_delay,
        &claimer_sk,
    )?;
    let io = BtcClaimIo { cfg: &btc };
    let (claim_txid, verification) = execute_claim_io(&io, &io, &raw, &plan)?;
    s.btc_claim_txid = Some(claim_txid.clone());
    let claim_verify = verification.as_ref().map(|result| result.status.clone());
    if let Some(result) = &verification {
        let _ = rgb_store.save_proof(&format!("{plan_id}-claim"), result);
    }

    if let Some(r) = s.btc_rgb.as_mut() {
        r.htlc_seal = Some(htlc_seal);
        r.claim_plan_id = Some(plan_id.clone());
        r.claim_anchor_txid = Some(claim_txid.clone());
        r.claim_verify = claim_verify.clone();
        r.successor_seal = Some(format!("{claim_txid}:1"));
    }
    s.notes.push(format!(
        "S3 BTC claim plan={plan_id} verify={claim_verify:?}"
    ));

    Ok(json!({
        "status": "claimed_btc",
        "phase": s.phase,
        "rgb_wrap": true,
        "txid": claim_txid,
        "dest": dest_addr,
        "explorer": format!("{}/tx/{}", btc.explorer_base, claim_txid),
        "claim_plan_id": plan_id,
        "claim_verify": claim_verify,
        "successor_seal": format!("{claim_txid}:1"),
    }))
}

/// Dispatch BTC claim: RGB wrap when session has `rgb_wrap` + btc contract.
pub fn claim_btc(
    cfg: &Config,
    rgb_store: &RgbStore,
    s: &mut SwapSession,
    preimage: &[u8],
    fee_sats: u64,
    commitment_sats: u64,
    entropy: u64,
) -> Result<Value> {
    if s.rgb_wrap && s.btc_contract_id.is_some() {
        claim_btc_rgb(
            cfg,
            rgb_store,
            s,
            preimage,
            fee_sats,
            commitment_sats,
            entropy,
        )
    } else {
        claim_btc_value(cfg, s, preimage, fee_sats)
    }
}

/// Fetch claim tx and extract preimage; verify against session hash.
pub fn resolve_preimage_from_lq_claim(cfg: &Config, s: &SwapSession) -> Result<Vec<u8>> {
    let txid = s
        .lq_claim_txid
        .as_ref()
        .context("no lq_claim_txid; claim-lq first or omit --from-witness")?;
    let pre = extract_preimage(cfg, "liquid", txid)?;
    lab_rgb::swap::check_preimage_matches_session(&pre, &s.hash_hex)?;
    Ok(pre.to_vec())
}

/// Extract 32-byte preimage from a chain transaction hex (via explorer).
pub fn extract_preimage(cfg: &Config, chain: &str, txid: &str) -> Result<[u8; 32]> {
    let c = chain.trim().to_ascii_lowercase();
    if c.starts_with("bitcoin") || c == "btc" || c == "tb" {
        let btc = lab_btc::BtcConfig::from_env();
        let hex_tx = lab_btc::fetch_tx_hex(&btc, txid)?;
        htlc::extract_preimage_from_btc_tx_hex(&hex_tx)
    } else if c.starts_with("liquid") || c == "lq" || c == "elements" {
        let hex_tx = lab_chain::fetch_tx_hex(cfg, txid)?;
        htlc::extract_preimage_from_liquid_tx_hex(&hex_tx)
    } else {
        anyhow::bail!("chain must be bitcoin|btc or liquid|lq (got {chain})");
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    struct FakeBroadcaster<'a> {
        calls: &'a Cell<u32>,
        result: Result<String, &'static str>,
    }

    impl ClaimBroadcaster for FakeBroadcaster<'_> {
        fn broadcast(&self, _raw_hex: &str) -> Result<String> {
            self.calls.set(self.calls.get() + 1);
            self.result.clone().map_err(anyhow::Error::msg)
        }
    }

    struct FakeRgbVerifier<'a> {
        calls: &'a Cell<u32>,
        status: Option<&'static str>,
    }

    impl ClaimVerifier for FakeRgbVerifier<'_> {
        fn verify(&self, plan: &TransferPlan, txid: &str) -> Result<Option<VerifyResult>> {
            self.calls.set(self.calls.get() + 1);
            Ok(self.status.map(|status| VerifyResult {
                status: status.into(),
                contract_id: Some(plan.contract_id.clone()),
                anchor_txid: txid.into(),
                checks: vec![],
                explorer_url: None,
            }))
        }
    }

    fn fixture_plan() -> TransferPlan {
        serde_json::from_value(serde_json::json!({
            "contract_id": "rgb:test",
            "chain_net": "liquid-testnet",
            "ticker": "s3",
            "send_amount": 1,
            "change_amount": 0,
            "alice_seal": format!("{}:0", "aa".repeat(32)),
            "bob_seal_placeholder": "witness:1:0",
            "change_seal_placeholder": "none",
            "bundle_id_hex": "11".repeat(32),
            "transition_opid_hex": "22".repeat(32),
            "mpc_root_hex": "33".repeat(32),
            "commitment_spk_hex": "5120".to_string() + &"44".repeat(32),
            "tapret_address": "tex1fixture",
            "internal_key_hex": DEMO_INTERNAL_XONLY_HEX,
            "static_entropy": 7,
            "protocol_id_hex": "55".repeat(32),
            "message_hex": "66".repeat(32)
        }))
        .unwrap()
    }

    #[test]
    fn broadcast_failure_short_circuits_verifier() {
        let broadcasts = Cell::new(0);
        let verifies = Cell::new(0);
        let broadcaster = FakeBroadcaster {
            calls: &broadcasts,
            result: Err("broadcast rejected"),
        };
        let verifier = FakeRgbVerifier {
            calls: &verifies,
            status: Some("valid"),
        };
        let err = execute_claim_io(&broadcaster, &verifier, "00", &fixture_plan()).unwrap_err();
        assert!(err.to_string().contains("broadcast rejected"));
        assert_eq!(broadcasts.get(), 1);
        assert_eq!(verifies.get(), 0);
    }

    #[test]
    fn invalid_verification_is_never_promoted_to_valid() {
        let broadcasts = Cell::new(0);
        let verifies = Cell::new(0);
        let broadcaster = FakeBroadcaster {
            calls: &broadcasts,
            result: Ok("claim-txid".into()),
        };
        let verifier = FakeRgbVerifier {
            calls: &verifies,
            status: Some("invalid"),
        };
        let (txid, result) =
            execute_claim_io(&broadcaster, &verifier, "00", &fixture_plan()).unwrap();
        assert_eq!(txid, "claim-txid");
        assert_eq!(result.unwrap().status, "invalid");
        assert_eq!(broadcasts.get(), 1);
        assert_eq!(verifies.get(), 1);
    }

    #[test]
    fn unavailable_verifier_remains_pending() {
        let broadcasts = Cell::new(0);
        let verifies = Cell::new(0);
        let broadcaster = FakeBroadcaster {
            calls: &broadcasts,
            result: Ok("claim-txid".into()),
        };
        let verifier = FakeRgbVerifier {
            calls: &verifies,
            status: None,
        };
        let (_, result) = execute_claim_io(&broadcaster, &verifier, "00", &fixture_plan()).unwrap();
        assert!(result.is_none());
        assert_eq!(broadcasts.get(), 1);
        assert_eq!(verifies.get(), 1);
    }
}
