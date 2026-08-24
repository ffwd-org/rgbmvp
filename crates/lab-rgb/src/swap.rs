//! Swap session state for BTC ↔ Liquid RGB atomic swaps (P1 / S3).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::htlc::{self, HtlcAddressInfo};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SwapPhase {
    Created,
    FundedBtc,
    FundedLq,
    /// Both legs funded (either order ends here via transition helpers)
    FundedBoth,
    ClaimedLq,
    ClaimedBtc,
    Done,
    Refunded,
}

/// Per-leg RGB wrap state (S3). Absent / default when `rgb_wrap=false`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SwapLegRgb {
    pub contract_id: String,
    /// RGB units placed on the HTLC seal (usually full supply).
    pub amount: u64,
    /// Original issue seal spent when funding RGB onto HTLC.
    pub issue_seal: Option<String>,
    /// HTLC outpoint that holds RGB after fund-wrap.
    pub htlc_seal: Option<String>,
    pub fund_plan_id: Option<String>,
    pub fund_anchor_txid: Option<String>,
    pub fund_verify: Option<String>,
    /// Transition opid hex of fund plan (prev for claim).
    pub fund_transition_opid_hex: Option<String>,
    pub claim_plan_id: Option<String>,
    pub claim_anchor_txid: Option<String>,
    pub claim_verify: Option<String>,
    /// Successor seal after claim (`claim_txid:vout`).
    pub successor_seal: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwapSession {
    pub id: String,
    /// Schema version: 1 = value-only P1, 2 = S3 rgb_wrap fields.
    #[serde(default = "default_session_version")]
    pub version: u32,
    pub phase: SwapPhase,
    pub csv_delay: u32,
    pub preimage_hex: String,
    pub hash_hex: String,
    pub btc_contract_id: Option<String>,
    pub lq_contract_id: Option<String>,
    /// When true, fund/claim must re-seat RGB and verify anchors for Done.
    #[serde(default)]
    pub rgb_wrap: bool,
    #[serde(default)]
    pub btc_rgb: Option<SwapLegRgb>,
    #[serde(default)]
    pub lq_rgb: Option<SwapLegRgb>,
    pub alice_btc_wallet: String,
    pub bob_lq_wallet: String,
    /// BTC leg: Bob claims, Alice refunds
    pub htlc_btc: HtlcAddressInfo,
    /// Liquid leg: Alice claims, Bob refunds
    pub htlc_lq: HtlcAddressInfo,
    pub btc_fund_txid: Option<String>,
    pub btc_fund_vout: Option<u32>,
    pub btc_fund_sats: Option<u64>,
    pub lq_fund_txid: Option<String>,
    pub lq_fund_vout: Option<u32>,
    pub lq_fund_sats: Option<u64>,
    pub lq_claim_txid: Option<String>,
    pub btc_claim_txid: Option<String>,
    pub notes: Vec<String>,
}

fn default_session_version() -> u32 {
    1
}

pub struct SwapStore {
    root: PathBuf,
}

impl SwapStore {
    pub fn new(data_dir: impl AsRef<Path>) -> Self {
        Self {
            root: data_dir.as_ref().join("swaps"),
        }
    }

    pub fn ensure(&self) -> Result<()> {
        fs::create_dir_all(&self.root)?;
        Ok(())
    }

    fn path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.json"))
    }

    pub fn save(&self, s: &SwapSession) -> Result<PathBuf> {
        self.ensure()?;
        let p = self.path(&s.id);
        fs::write(&p, serde_json::to_vec_pretty(s)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&p)?.permissions();
            perms.set_mode(0o600);
            fs::set_permissions(&p, perms)?;
        }
        Ok(p)
    }

    pub fn load(&self, id: &str) -> Result<SwapSession> {
        let p = self.path(id);
        let raw = fs::read_to_string(&p).with_context(|| format!("load {}", p.display()))?;
        Ok(serde_json::from_str(&raw)?)
    }

    pub fn path_exists(&self, id: &str) -> bool {
        self.path(id).exists()
    }
}

pub fn init_swap(
    keyring: &htlc::DemoKeyring,
    id: &str,
    csv_delay: u32,
    alice_btc_wallet: &str,
    bob_lq_wallet: &str,
    btc_contract_id: Option<String>,
    lq_contract_id: Option<String>,
    rgb_wrap: bool,
) -> Result<SwapSession> {
    if csv_delay == 0 {
        bail!("csv_delay must be > 0");
    }
    if rgb_wrap && btc_contract_id.is_none() && lq_contract_id.is_none() {
        bail!("--rgb-wrap requires at least one of --btc-contract / --lq-contract");
    }
    let mut preimage = [0u8; 32];
    fill_random(&mut preimage)?;
    let hash = htlc::sha256_preimage(&preimage);

    // BTC: Bob claims Alice's locked coins; Alice refunds after CSV
    let htlc_btc =
        htlc::build_htlc_addresses(keyring, &hash, "bob-claimer", "alice-refund", csv_delay)?;
    // LQ: Alice claims Bob's locked L-BTC; Bob refunds after CSV
    let htlc_lq =
        htlc::build_htlc_addresses(keyring, &hash, "alice-claimer", "bob-refund", csv_delay)?;

    let btc_rgb = btc_contract_id.as_ref().map(|cid| SwapLegRgb {
        contract_id: cid.clone(),
        amount: 0, // filled on fund from issue.supply
        ..Default::default()
    });
    let lq_rgb = lq_contract_id.as_ref().map(|cid| SwapLegRgb {
        contract_id: cid.clone(),
        amount: 0,
        ..Default::default()
    });

    let mut notes = vec![
        "Alice claims Liquid first (reveals preimage). Bob then claims BTC.".into(),
        "Preimage file is mode 600 under .rgbmvp/swaps/.".into(),
    ];
    if rgb_wrap {
        notes.push(
            "S3 rgb_wrap: fund assigns RGB to HTLC seal; claim re-anchors + verify required for done."
                .into(),
        );
    }

    Ok(SwapSession {
        id: id.into(),
        version: if rgb_wrap { 2 } else { 1 },
        phase: SwapPhase::Created,
        csv_delay,
        preimage_hex: hex::encode(preimage),
        hash_hex: hex::encode(hash),
        btc_contract_id,
        lq_contract_id,
        rgb_wrap,
        btc_rgb,
        lq_rgb,
        alice_btc_wallet: alice_btc_wallet.into(),
        bob_lq_wallet: bob_lq_wallet.into(),
        htlc_btc,
        htlc_lq,
        btc_fund_txid: None,
        btc_fund_vout: None,
        btc_fund_sats: None,
        lq_fund_txid: None,
        lq_fund_vout: None,
        lq_fund_sats: None,
        lq_claim_txid: None,
        btc_claim_txid: None,
        notes,
    })
}

fn fill_random(buf: &mut [u8]) -> Result<()> {
    use bitcoin::secp256k1::rand::rngs::OsRng;
    use bitcoin::secp256k1::rand::RngCore;
    OsRng.fill_bytes(buf);
    Ok(())
}

/// True when value claims are complete and (if rgb_wrap) required claim verifies are valid.
pub fn rgb_done_ok(s: &SwapSession) -> bool {
    if !s.rgb_wrap {
        return true;
    }
    let btc_ok = if s.btc_contract_id.is_some() {
        s.btc_rgb
            .as_ref()
            .map(|r| r.claim_verify.as_deref() == Some("valid"))
            .unwrap_or(false)
    } else {
        true
    };
    let lq_ok = if s.lq_contract_id.is_some() {
        s.lq_rgb
            .as_ref()
            .map(|r| r.claim_verify.as_deref() == Some("valid"))
            .unwrap_or(false)
    } else {
        true
    };
    btc_ok && lq_ok
}

/// S3 gate: RGB claim requires a prior fund-wrap transition id on the leg.
pub fn require_fund_wrap_for_claim(leg: Option<&SwapLegRgb>, side: &str) -> Result<()> {
    let leg = leg.with_context(|| format!("{side}_rgb missing; fund --rgb-wrap first"))?;
    if leg
        .fund_transition_opid_hex
        .as_ref()
        .map(|s| s.trim().is_empty())
        .unwrap_or(true)
    {
        bail!("fund_transition_opid_hex missing (fund-{side} --rgb-wrap first)");
    }
    Ok(())
}

/// Which leg's RGB claim_verify to update (S3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RgbClaimLeg {
    Btc,
    Lq,
}

/// Record claim_verify on a leg and recompute phase.
/// Invalid / missing verify can never yield [`SwapPhase::Done`] when `rgb_wrap`.
pub fn set_claim_verify(s: &mut SwapSession, leg: RgbClaimLeg, status: impl Into<String>) {
    let status = status.into();
    match leg {
        RgbClaimLeg::Btc => {
            if let Some(r) = s.btc_rgb.as_mut() {
                r.claim_verify = Some(status);
            }
        }
        RgbClaimLeg::Lq => {
            if let Some(r) = s.lq_rgb.as_mut() {
                r.claim_verify = Some(status);
            }
        }
    }
    recompute_phase(s);
}

/// Reject extracted preimage that does not hash to the session commitment.
pub fn check_preimage_matches_session(preimage: &[u8], session_hash_hex: &str) -> Result<()> {
    let hash = htlc::sha256_preimage(preimage);
    let session_hash = hex::decode(session_hash_hex.trim()).context("session hash_hex")?;
    if session_hash.as_slice() != hash.as_slice() {
        bail!("extracted preimage hash does not match session hash_hex");
    }
    Ok(())
}

/// Contract id on leg must match session field when both are set (S3 negative).
pub fn check_leg_contract_matches_session(
    leg: &SwapLegRgb,
    session_contract: Option<&str>,
    side: &str,
) -> Result<()> {
    if let Some(want) = session_contract {
        if leg.contract_id != want {
            bail!("{side} contract_id mismatch: leg={} session={want}", leg.contract_id);
        }
    }
    Ok(())
}

pub fn recompute_phase(s: &mut SwapSession) {
    if matches!(s.phase, SwapPhase::Refunded) {
        return;
    }
    let btc = s.btc_fund_txid.is_some();
    let lq = s.lq_fund_txid.is_some();
    let clq = s.lq_claim_txid.is_some();
    let cbtc = s.btc_claim_txid.is_some();
    s.phase = match (btc, lq, clq, cbtc) {
        (true, true, true, true) if rgb_done_ok(s) => SwapPhase::Done,
        (true, true, true, true) => SwapPhase::ClaimedBtc, // value claimed; RGB verify pending
        (true, true, true, false) => SwapPhase::ClaimedLq,
        (true, true, false, false) => SwapPhase::FundedBoth,
        (true, false, _, _) => SwapPhase::FundedBtc,
        (false, true, _, _) => SwapPhase::FundedLq,
        _ => SwapPhase::Created,
    };
}

/// Mark both value claims complete (test / offline helper). Does not set RGB verifies.
pub fn mark_value_claims_complete(s: &mut SwapSession) {
    if s.btc_fund_txid.is_none() {
        s.btc_fund_txid = Some("btc-fund".into());
    }
    if s.lq_fund_txid.is_none() {
        s.lq_fund_txid = Some("lq-fund".into());
    }
    if s.lq_claim_txid.is_none() {
        s.lq_claim_txid = Some("lq-claim".into());
    }
    if s.btc_claim_txid.is_none() {
        s.btc_claim_txid = Some("btc-claim".into());
    }
    recompute_phase(s);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dual_wrap_session() -> SwapSession {
        init_swap(
            &htlc::test_keyring(),
            "s3-neg",
            6,
            "btc-alice",
            "bob",
            Some("rgb:btc-c1".into()),
            Some("rgb:lq-c1".into()),
            true,
        )
        .unwrap()
    }

    #[test]
    fn legacy_session_deserializes() {
        let json = r#"{
            "id":"t","phase":"created","csv_delay":6,
            "preimage_hex":"aa","hash_hex":"bb",
            "btc_contract_id":null,"lq_contract_id":null,
            "alice_btc_wallet":"btc-alice","bob_lq_wallet":"bob",
            "htlc_btc":{"hash_hex":"bb","csv_delay":6,"claimer_label":"c","refund_label":"r",
                "witness_script_hex":"00","spk_hex":"00","address_btc":"tb1q","address_liquid_unconf":"tex1q"},
            "htlc_lq":{"hash_hex":"bb","csv_delay":6,"claimer_label":"c","refund_label":"r",
                "witness_script_hex":"00","spk_hex":"00","address_btc":"tb1q","address_liquid_unconf":"tex1q"},
            "btc_fund_txid":null,"btc_fund_vout":null,"btc_fund_sats":null,
            "lq_fund_txid":null,"lq_fund_vout":null,"lq_fund_sats":null,
            "lq_claim_txid":null,"btc_claim_txid":null,"notes":[]
        }"#;
        let s: SwapSession = serde_json::from_str(json).unwrap();
        assert!(!s.rgb_wrap);
        assert_eq!(s.version, 1);
        assert!(s.btc_rgb.is_none());
    }

    #[test]
    fn value_only_done_without_rgb_fields() {
        let mut s = init_swap(
            &htlc::test_keyring(),
            "vo",
            6,
            "btc-alice",
            "bob",
            None,
            None,
            false,
        )
        .unwrap();
        assert!(!s.rgb_wrap);
        mark_value_claims_complete(&mut s);
        assert_eq!(s.phase, SwapPhase::Done);
        assert!(rgb_done_ok(&s));
    }

    #[test]
    fn claim_without_fund_wrap_errors() {
        let s = dual_wrap_session();
        let err = require_fund_wrap_for_claim(s.lq_rgb.as_ref(), "lq").unwrap_err();
        assert!(
            err.to_string().contains("fund_transition_opid_hex"),
            "got {err}"
        );
        let err = require_fund_wrap_for_claim(None, "btc").unwrap_err();
        assert!(err.to_string().contains("btc_rgb missing"));
    }

    #[test]
    fn invalid_claim_verify_never_done() {
        let mut s = dual_wrap_session();
        // Simulate fund-wrap transition ids present.
        if let Some(r) = s.btc_rgb.as_mut() {
            r.fund_transition_opid_hex = Some("aa".repeat(32));
        }
        if let Some(r) = s.lq_rgb.as_mut() {
            r.fund_transition_opid_hex = Some("bb".repeat(32));
        }
        mark_value_claims_complete(&mut s);
        assert_eq!(s.phase, SwapPhase::ClaimedBtc);
        set_claim_verify(&mut s, RgbClaimLeg::Lq, "valid");
        set_claim_verify(&mut s, RgbClaimLeg::Btc, "invalid");
        assert_eq!(s.phase, SwapPhase::ClaimedBtc);
        assert!(!rgb_done_ok(&s));
    }

    #[test]
    fn one_valid_one_invalid_leg_blocks_done() {
        let mut s = dual_wrap_session();
        mark_value_claims_complete(&mut s);
        set_claim_verify(&mut s, RgbClaimLeg::Lq, "valid");
        // BTC still missing verify
        assert_eq!(s.phase, SwapPhase::ClaimedBtc);
        set_claim_verify(&mut s, RgbClaimLeg::Btc, "pending");
        assert_ne!(s.phase, SwapPhase::Done);
    }

    #[test]
    fn both_valid_claims_reach_done() {
        let mut s = dual_wrap_session();
        mark_value_claims_complete(&mut s);
        set_claim_verify(&mut s, RgbClaimLeg::Lq, "valid");
        set_claim_verify(&mut s, RgbClaimLeg::Btc, "valid");
        assert_eq!(s.phase, SwapPhase::Done);
    }

    #[test]
    fn wrong_commitment_style_status_blocks_done() {
        // wrong commitment SPK → verifier returns invalid (recorded as claim_verify)
        let mut s = dual_wrap_session();
        mark_value_claims_complete(&mut s);
        set_claim_verify(&mut s, RgbClaimLeg::Lq, "valid");
        set_claim_verify(&mut s, RgbClaimLeg::Btc, "invalid");
        assert_eq!(s.phase, SwapPhase::ClaimedBtc);
    }

    #[test]
    fn contract_id_mismatch_detected() {
        let s = dual_wrap_session();
        let leg = s.btc_rgb.as_ref().unwrap();
        let err = check_leg_contract_matches_session(leg, Some("rgb:other"), "btc").unwrap_err();
        assert!(err.to_string().contains("mismatch"));
        check_leg_contract_matches_session(leg, Some("rgb:btc-c1"), "btc").unwrap();
    }

    #[test]
    fn preimage_hash_mismatch_rejected() {
        let s = dual_wrap_session();
        let wrong = [0u8; 32];
        let err = check_preimage_matches_session(&wrong, &s.hash_hex).unwrap_err();
        assert!(err.to_string().contains("does not match"));
        let pre = hex::decode(&s.preimage_hex).unwrap();
        check_preimage_matches_session(&pre, &s.hash_hex).unwrap();
    }

    #[test]
    fn rgb_done_requires_verify_when_wrap() {
        let mut s = init_swap(
            &htlc::test_keyring(),
            "x",
            6,
            "btc-alice",
            "bob",
            Some("c1".into()),
            None,
            true,
        )
        .unwrap();
        s.btc_fund_txid = Some("a".into());
        s.lq_fund_txid = Some("b".into());
        s.lq_claim_txid = Some("c".into());
        s.btc_claim_txid = Some("d".into());
        recompute_phase(&mut s);
        assert_eq!(s.phase, SwapPhase::ClaimedBtc);
        if let Some(r) = s.btc_rgb.as_mut() {
            r.claim_verify = Some("valid".into());
        }
        recompute_phase(&mut s);
        assert_eq!(s.phase, SwapPhase::Done);
    }

    #[test]
    fn store_roundtrip_redacts_nothing_but_mode_ok() {
        let dir = std::env::temp_dir().join(format!("rgbmvp-swap-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = SwapStore::new(&dir);
        let s = init_swap(
            &htlc::test_keyring(),
            "id1",
            6,
            "a",
            "b",
            None,
            None,
            false,
        )
        .unwrap();
        store.save(&s).unwrap();
        let loaded = store.load("id1").unwrap();
        assert_eq!(loaded.preimage_hex, s.preimage_hex);
        assert_eq!(loaded.hash_hex, s.hash_hex);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
