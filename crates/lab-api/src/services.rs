//! Application services shared by CLI and labd (U5 preparation).
//!
//! Domain rules stay in `lab_rgb` / `lab_core`. This layer owns session I/O
//! and S3 fund-wrap / claim orchestration that both surfaces can call without
//! parsing Clap args.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use lab_core::Config;
use lab_rgb::storage::RgbStore;
use lab_rgb::swap::{self, SwapSession, SwapStore};
use serde_json::Value;

use crate::s3;

/// Swap session application service (init / load / save + S3 ops).
#[derive(Debug, Clone)]
pub struct SwapService {
    store: PathBuf,
}

impl SwapService {
    pub fn new(data_dir: impl AsRef<Path>) -> Self {
        Self {
            store: data_dir.as_ref().to_path_buf(),
        }
    }

    pub fn data_dir(&self) -> &Path {
        &self.store
    }

    pub fn store(&self) -> SwapStore {
        SwapStore::new(&self.store)
    }

    pub fn rgb_store(&self) -> RgbStore {
        RgbStore::new(&self.store)
    }

    pub fn init(
        &self,
        cfg: &Config,
        id: &str,
        csv_delay: u32,
        alice_btc_wallet: &str,
        bob_lq_wallet: &str,
        btc_contract_id: Option<String>,
        lq_contract_id: Option<String>,
        rgb_wrap: bool,
    ) -> Result<SwapSession> {
        let keyring = lab_rgb::htlc::DemoKeyring::new(cfg.demo_exit_seed()?)?;
        let session = swap::init_swap(
            &keyring,
            id,
            csv_delay,
            alice_btc_wallet,
            bob_lq_wallet,
            btc_contract_id,
            lq_contract_id,
            rgb_wrap,
        )?;
        self.store()
            .save(&session)
            .context("save swap session")?;
        Ok(session)
    }

    pub fn load(&self, id: &str) -> Result<SwapSession> {
        self.store().load(id)
    }

    pub fn save(&self, session: &SwapSession) -> Result<PathBuf> {
        self.store().save(session)
    }

    pub fn exists(&self, id: &str) -> bool {
        self.store().path_exists(id)
    }

    /// Persist phase recompute after mutation.
    pub fn recompute_and_save(&self, s: &mut SwapSession) -> Result<PathBuf> {
        swap::recompute_phase(s);
        self.save(s)
    }

    /// RGB fund-wrap onto BTC HTLC seal (session must already have value fund txid).
    pub fn fund_wrap_btc(
        &self,
        cfg: &Config,
        btc: &lab_btc::BtcConfig,
        s: &mut SwapSession,
        commitment_sats: u64,
        entropy: u64,
    ) -> Result<Value> {
        s3::fund_wrap_btc(cfg, btc, &self.rgb_store(), s, commitment_sats, entropy)
    }

    /// RGB fund-wrap onto Liquid HTLC seal.
    pub fn fund_wrap_lq(
        &self,
        cfg: &Config,
        s: &mut SwapSession,
        commitment_sats: u64,
        entropy: u64,
    ) -> Result<Value> {
        s3::fund_wrap_lq(cfg, &self.rgb_store(), s, commitment_sats, entropy)
    }

    /// Claim Liquid HTLC (value or S3 RGB depending on session).
    pub fn claim_lq(
        &self,
        cfg: &Config,
        s: &mut SwapSession,
        fee_sats: u64,
        commitment_sats: u64,
        entropy: u64,
    ) -> Result<Value> {
        s3::claim_lq(
            cfg,
            &self.rgb_store(),
            s,
            fee_sats,
            commitment_sats,
            entropy,
        )
    }

    /// Claim BTC HTLC (value or S3 RGB). `from_witness` pulls preimage from LQ claim.
    pub fn claim_btc(
        &self,
        cfg: &Config,
        s: &mut SwapSession,
        fee_sats: u64,
        commitment_sats: u64,
        entropy: u64,
        from_witness: bool,
    ) -> Result<Value> {
        let preimage = if from_witness {
            s3::resolve_preimage_from_lq_claim(cfg, s)?
        } else {
            hex::decode(&s.preimage_hex)?
        };
        s3::claim_btc(
            cfg,
            &self.rgb_store(),
            s,
            &preimage,
            fee_sats,
            commitment_sats,
            entropy,
        )
    }

    /// Extract preimage from a claim tx (optionally note match on session).
    pub fn extract_preimage(
        &self,
        cfg: &Config,
        chain: &str,
        txid: &str,
        session_id: Option<&str>,
    ) -> Result<Value> {
        let pre = s3::extract_preimage(cfg, chain, txid)?;
        let pre_hex = hex::encode(pre);
        let hash = htlc_sha256(&pre);
        let mut matched = None;
        if let Some(sid) = session_id {
            let mut s = self.load(sid)?;
            let session_hash = hex::decode(&s.hash_hex)?;
            let ok = session_hash.as_slice() == hash.as_slice();
            matched = Some(ok);
            if ok {
                s.notes
                    .push(format!("extract-preimage matched hash from {chain} tx {txid}"));
                self.save(&s)?;
            }
        }
        Ok(serde_json::json!({
            "preimage_hex": pre_hex,
            "hash_hex": hex::encode(hash),
            "chain": chain,
            "txid": txid,
            "session_hash_match": matched,
        }))
    }
}

fn htlc_sha256(pre: &[u8]) -> [u8; 32] {
    lab_rgb::htlc::sha256_preimage(pre)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_load_without_cli_args() {
        let dir = std::env::temp_dir().join(format!("rgbmvp-svc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let svc = SwapService::new(&dir);
        let mut cfg = Config::load().unwrap();
        cfg.data_dir = dir.clone();
        cfg.wallet_dir = dir.join("wallets");
        cfg.consignment_dir = dir.join("consignments");
        cfg.ensure_dirs().unwrap();
        let s = svc
            .init(
                &cfg,
                "svc1",
                6,
                "btc-alice",
                "bob",
                Some("rgb:a".into()),
                Some("rgb:b".into()),
                true,
            )
            .unwrap();
        assert!(s.rgb_wrap);
        assert_eq!(s.version, 2);
        let loaded = svc.load("svc1").unwrap();
        assert_eq!(loaded.id, "svc1");
        assert_eq!(loaded.hash_hex, s.hash_hex);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
