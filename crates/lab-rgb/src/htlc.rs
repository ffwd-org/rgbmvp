//! Cross-chain HTLC scripts + spend builders (Bitcoin).
//!
//! Script layout matches kaleidoswap/rgb-on-liquid-spike (claim + CSV refund).

use anyhow::{bail, Context, Result};
use bitcoin::absolute::LockTime;
use bitcoin::bip32::{ChildNumber, DerivationPath, Xpriv};
use bitcoin::consensus::encode::serialize_hex;
use bitcoin::hashes::{sha256, Hash};
use bitcoin::key::Secp256k1;
use bitcoin::secp256k1::{Message, PublicKey, SecretKey};
use bitcoin::sighash::{EcdsaSighashType, SighashCache};
use bitcoin::transaction::Version;
use bitcoin::{Amount, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid, Witness};
use serde::{Deserialize, Serialize};

pub enum HtlcSpend<'a> {
    Claim { preimage: &'a [u8] },
    Refund,
}

/// OP_IF OP_SHA256 <H> OP_EQUALVERIFY <claimerPk> OP_CHECKSIG
/// OP_ELSE <T> OP_CSV OP_DROP <refundPk> OP_CHECKSIG OP_ENDIF
pub fn htlc_witness_script(
    hash: &[u8; 32],
    claimer_pk: &[u8; 33],
    refund_pk: &[u8; 33],
    csv_delay: u32,
) -> Vec<u8> {
    let mut s = Vec::with_capacity(112);
    s.push(0x63); // OP_IF
    s.push(0xa8); // OP_SHA256
    s.push(0x20);
    s.extend_from_slice(hash);
    s.push(0x88); // OP_EQUALVERIFY
    s.push(0x21);
    s.extend_from_slice(claimer_pk);
    s.push(0xac); // OP_CHECKSIG
    s.push(0x67); // OP_ELSE
    s.extend_from_slice(&push_script_num(csv_delay));
    s.push(0xb2); // OP_CHECKSEQUENCEVERIFY
    s.push(0x75); // OP_DROP
    s.push(0x21);
    s.extend_from_slice(refund_pk);
    s.push(0xac); // OP_CHECKSIG
    s.push(0x68); // OP_ENDIF
    s
}

fn push_script_num(n: u32) -> Vec<u8> {
    assert!(n > 0, "CSV delay must be positive");
    if n <= 16 {
        return vec![0x50 + n as u8];
    }
    let mut le = Vec::new();
    let mut v = n;
    while v > 0 {
        le.push((v & 0xff) as u8);
        v >>= 8;
    }
    if le.last().copied().unwrap_or(0) & 0x80 != 0 {
        le.push(0x00);
    }
    let mut out = vec![le.len() as u8];
    out.extend_from_slice(&le);
    out
}

pub fn p2wsh_spk(witness_script: &[u8]) -> Vec<u8> {
    let wsh = sha256::Hash::hash(witness_script);
    let mut spk = Vec::with_capacity(34);
    spk.push(0x00);
    spk.push(0x20);
    spk.extend_from_slice(wsh.as_byte_array());
    spk
}

pub fn p2wsh_address(network_hrp: &str, witness_script: &[u8]) -> Result<String> {
    use bech32::{segwit, Hrp};
    let wsh = sha256::Hash::hash(witness_script);
    let hrp = Hrp::parse(network_hrp).map_err(|e| anyhow::anyhow!("hrp: {e}"))?;
    let v0 = bech32::Fe32::try_from(0u8).unwrap();
    segwit::encode(hrp, v0, wsh.as_byte_array()).map_err(|e| anyhow::anyhow!("bech32: {e}"))
}

/// Version recorded in sessions created with custody-backed exit keys.
pub const DEMO_EXIT_KEY_SCHEME: &str = "bip32-hardened-v1";

fn legacy_exit_key_scheme() -> String {
    "legacy-public-label-v0".into()
}

/// T1 exit-key root. Its seed is never serialized or exposed through `/v1`.
#[derive(Clone)]
pub struct DemoKeyring {
    seed: [u8; 32],
    key_id: String,
}

impl DemoKeyring {
    pub fn new(seed: [u8; 32]) -> Result<Self> {
        let mut keyring = Self {
            seed,
            key_id: String::new(),
        };
        let (_, pk) = keyring.derive_raw("rgbmvp/t1/key-id")?;
        keyring.key_id = hex::encode(&sha256::Hash::hash(&pk).to_byte_array()[..8]);
        Ok(keyring)
    }

    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    fn derive_raw(&self, label: &str) -> Result<(SecretKey, [u8; 33])> {
        let secp = Secp256k1::new();
        let master = Xpriv::new_master(bitcoin::Network::Testnet, &self.seed)
            .context("derive T1 BIP32 master key")?;

        // Four hardened indices provide 124 bits of domain-separated label
        // space. Public child material cannot derive siblings or the root.
        let mut domain = b"rgbmvp/t1/demo-exit/v1\0".to_vec();
        domain.extend_from_slice(label.as_bytes());
        let digest = sha256::Hash::hash(&domain).to_byte_array();
        let mut path = Vec::with_capacity(4);
        for chunk in digest[..16].chunks_exact(4) {
            let index = u32::from_be_bytes(chunk.try_into().expect("four-byte chunk"))
                & 0x7fff_ffff;
            path.push(
                ChildNumber::from_hardened_idx(index)
                    .context("construct hardened T1 BIP32 child")?,
            );
        }
        let child = master
            .derive_priv(&secp, &DerivationPath::from(path))
            .context("derive hardened T1 exit key")?;
        let sk = child.private_key;
        let pk = PublicKey::from_secret_key(&secp, &sk);
        Ok((sk, pk.serialize()))
    }

    pub fn derive(&self, label: &str) -> Result<(SecretKey, [u8; 33])> {
        if label.trim().is_empty() {
            bail!("demo exit key label must not be empty");
        }
        self.derive_raw(label)
    }

    /// Refuse legacy sessions and sessions created under a different seed.
    pub fn derive_for_session(
        &self,
        info: &HtlcAddressInfo,
        label: &str,
    ) -> Result<(SecretKey, [u8; 33])> {
        if info.exit_key_scheme != DEMO_EXIT_KEY_SCHEME {
            bail!(
                "swap uses unsupported exit-key scheme {:?}; drain legacy exits before upgrade",
                info.exit_key_scheme
            );
        }
        if info.exit_key_id != self.key_id {
            bail!(
                "swap exit-key id {} does not match mounted key id {}; restore the original seed",
                info.exit_key_id,
                self.key_id
            );
        }
        self.derive(label)
    }
}

#[cfg(test)]
pub fn test_keyring() -> DemoKeyring {
    DemoKeyring::new([0x42; 32]).expect("valid test keyring")
}

pub fn sha256_preimage(preimage: &[u8]) -> [u8; 32] {
    sha256::Hash::hash(preimage).to_byte_array()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HtlcAddressInfo {
    pub hash_hex: String,
    pub csv_delay: u32,
    pub claimer_label: String,
    pub refund_label: String,
    #[serde(default = "legacy_exit_key_scheme")]
    pub exit_key_scheme: String,
    #[serde(default)]
    pub exit_key_id: String,
    pub witness_script_hex: String,
    pub spk_hex: String,
    pub address_btc: String,
    pub address_liquid_unconf: String,
}

pub fn build_htlc_addresses(
    keyring: &DemoKeyring,
    hash: &[u8; 32],
    claimer_label: &str,
    refund_label: &str,
    csv_delay: u32,
) -> Result<HtlcAddressInfo> {
    let (_, claimer_pk) = keyring.derive(claimer_label)?;
    let (_, refund_pk) = keyring.derive(refund_label)?;
    let ws = htlc_witness_script(hash, &claimer_pk, &refund_pk, csv_delay);
    Ok(HtlcAddressInfo {
        hash_hex: hex::encode(hash),
        csv_delay,
        claimer_label: claimer_label.into(),
        refund_label: refund_label.into(),
        exit_key_scheme: DEMO_EXIT_KEY_SCHEME.into(),
        exit_key_id: keyring.key_id().into(),
        witness_script_hex: hex::encode(&ws),
        spk_hex: hex::encode(p2wsh_spk(&ws)),
        address_btc: p2wsh_address("tb", &ws)?,
        address_liquid_unconf: p2wsh_address("tex", &ws)?,
    })
}

fn htlc_sequence(spend: &HtlcSpend, csv_delay: u32) -> u32 {
    match spend {
        HtlcSpend::Claim { .. } => 0xffff_fffd,
        HtlcSpend::Refund => csv_delay,
    }
}

fn htlc_witness_stack(sig_der_all: Vec<u8>, spend: &HtlcSpend, ws: &[u8]) -> Vec<Vec<u8>> {
    match spend {
        HtlcSpend::Claim { preimage } => {
            vec![sig_der_all, preimage.to_vec(), vec![0x01], ws.to_vec()]
        }
        HtlcSpend::Refund => vec![sig_der_all, vec![], ws.to_vec()],
    }
}

/// Build + sign Bitcoin HTLC spend (claim or refund). Returns raw tx hex.
#[allow(clippy::too_many_arguments)]
pub fn build_htlc_spend_btc(
    prev_txid: &str,
    prev_vout: u32,
    input_value_sat: u64,
    output_value_sat: u64,
    dest_spk: &[u8],
    witness_script: &[u8],
    spend: HtlcSpend,
    csv_delay: u32,
    signer_sk: &SecretKey,
) -> Result<String> {
    build_htlc_spend_btc_outs(
        prev_txid,
        prev_vout,
        input_value_sat,
        &[(output_value_sat, dest_spk)],
        witness_script,
        spend,
        csv_delay,
        signer_sk,
    )
}

/// Multi-output BTC HTLC spend (S3: vout0=tapret commitment, vout1=claimer).
#[allow(clippy::too_many_arguments)]
pub fn build_htlc_spend_btc_outs(
    prev_txid: &str,
    prev_vout: u32,
    input_value_sat: u64,
    outputs: &[(u64, &[u8])],
    witness_script: &[u8],
    spend: HtlcSpend,
    csv_delay: u32,
    signer_sk: &SecretKey,
) -> Result<String> {
    if outputs.is_empty() {
        bail!("need at least one output");
    }
    let out_sum: u64 = outputs.iter().map(|(v, _)| *v).sum();
    if out_sum >= input_value_sat {
        bail!("outputs must leave room for fee ({out_sum} >= {input_value_sat})");
    }
    let prev_txid: Txid = prev_txid.parse().context("prev_txid")?;
    let tx_outs: Vec<TxOut> = outputs
        .iter()
        .map(|(v, spk)| TxOut {
            value: Amount::from_sat(*v),
            script_pubkey: ScriptBuf::from_bytes(spk.to_vec()),
        })
        .collect();
    let mut tx = Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint::new(prev_txid, prev_vout),
            script_sig: ScriptBuf::new(),
            sequence: Sequence::from_consensus(htlc_sequence(&spend, csv_delay)),
            witness: Witness::new(),
        }],
        output: tx_outs,
    };

    let sighash = SighashCache::new(&tx)
        .p2wsh_signature_hash(
            0,
            ScriptBuf::from_bytes(witness_script.to_vec()).as_script(),
            Amount::from_sat(input_value_sat),
            EcdsaSighashType::All,
        )
        .context("p2wsh sighash")?;

    let secp = Secp256k1::new();
    let msg = Message::from_digest(sighash.to_byte_array());
    let mut sig = secp.sign_ecdsa(&msg, signer_sk).serialize_der().to_vec();
    sig.push(EcdsaSighashType::All as u8);

    for item in htlc_witness_stack(sig, &spend, witness_script) {
        tx.input[0].witness.push(item);
    }
    Ok(serialize_hex(&tx))
}

/// Extract 32-byte preimage from a claim witness stack
/// (`[sig, preimage, 0x01, witness_script]`).
pub fn extract_preimage_from_witness_stack(stack: &[Vec<u8>]) -> Result<[u8; 32]> {
    // Claim path has 4 items; refund has 3 (sig, empty, script).
    if stack.len() < 4 {
        bail!(
            "witness stack too short for claim (got {} items; need sig+preimage+IF+script)",
            stack.len()
        );
    }
    let preimage = &stack[1];
    if preimage.len() != 32 {
        bail!(
            "preimage stack item must be 32 bytes, got {}",
            preimage.len()
        );
    }
    // Optional: check IF branch marker is non-empty (true).
    if stack[2].is_empty() {
        bail!("witness looks like refund (empty IF branch), not claim");
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(preimage);
    Ok(out)
}

/// Parse bitcoin consensus-encoded tx hex and extract claim preimage from vin0.
pub fn extract_preimage_from_btc_tx_hex(tx_hex: &str) -> Result<[u8; 32]> {
    use bitcoin::consensus::encode::deserialize;
    let raw = hex::decode(tx_hex.trim()).context("tx hex")?;
    let tx: Transaction = deserialize(&raw).context("deserialize bitcoin tx")?;
    if tx.input.is_empty() {
        bail!("tx has no inputs");
    }
    let stack: Vec<Vec<u8>> = tx.input[0].witness.to_vec();
    extract_preimage_from_witness_stack(&stack)
}

/// Parse Elements tx hex and extract claim preimage from vin0.
pub fn extract_preimage_from_liquid_tx_hex(tx_hex: &str) -> Result<[u8; 32]> {
    use elements::encode::deserialize;
    let raw = hex::decode(tx_hex.trim()).context("tx hex")?;
    let tx: elements::Transaction = deserialize(&raw).context("deserialize elements tx")?;
    if tx.input.is_empty() {
        bail!("tx has no inputs");
    }
    let stack = &tx.input[0].witness.script_witness;
    extract_preimage_from_witness_stack(stack)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_keys_require_the_secret_seed() {
        let label = "bob-claimer";
        let a = DemoKeyring::new([0x11; 32]).unwrap();
        let a_again = DemoKeyring::new([0x11; 32]).unwrap();
        let b = DemoKeyring::new([0x22; 32]).unwrap();

        let (a_sk, a_pk) = a.derive(label).unwrap();
        let (same_sk, same_pk) = a_again.derive(label).unwrap();
        let (b_sk, b_pk) = b.derive(label).unwrap();
        assert_eq!(a_sk.secret_bytes(), same_sk.secret_bytes());
        assert_eq!(a_pk, same_pk);
        assert_ne!(a_sk.secret_bytes(), b_sk.secret_bytes());
        assert_ne!(a_pk, b_pk);
        assert_ne!(a.key_id(), b.key_id());

        // Regression: the old construction made sha256(public_label) the key.
        let public_label_key =
            SecretKey::from_slice(sha256::Hash::hash(label.as_bytes()).as_byte_array()).unwrap();
        assert_ne!(a_sk.secret_bytes(), public_label_key.secret_bytes());
    }

    #[test]
    fn exit_key_labels_are_domain_separated() {
        let keyring = test_keyring();
        let (claim, _) = keyring.derive("bob-claimer").unwrap();
        let (refund, _) = keyring.derive("alice-refund").unwrap();
        assert_ne!(claim.secret_bytes(), refund.secret_bytes());
    }

    #[test]
    fn sessions_reject_legacy_or_rotated_exit_keys() {
        let current = DemoKeyring::new([0x11; 32]).unwrap();
        let rotated = DemoKeyring::new([0x22; 32]).unwrap();
        let mut info = build_htlc_addresses(&current, &[0x42; 32], "claimer", "refund", 6)
            .unwrap();

        assert!(current.derive_for_session(&info, "claimer").is_ok());
        let rotated_err = rotated.derive_for_session(&info, "claimer").unwrap_err();
        assert!(rotated_err.to_string().contains("does not match mounted key id"));

        info.exit_key_scheme = legacy_exit_key_scheme();
        info.exit_key_id.clear();
        let legacy_err = current.derive_for_session(&info, "claimer").unwrap_err();
        assert!(legacy_err.to_string().contains("unsupported exit-key scheme"));
    }

    #[test]
    fn legacy_json_fails_closed_without_key_metadata() {
        let keyring = test_keyring();
        let info = build_htlc_addresses(&keyring, &[0x42; 32], "claimer", "refund", 6)
            .unwrap();
        let mut value = serde_json::to_value(&info).unwrap();
        value.as_object_mut().unwrap().remove("exit_key_scheme");
        value.as_object_mut().unwrap().remove("exit_key_id");
        let legacy: HtlcAddressInfo = serde_json::from_value(value).unwrap();
        assert_eq!(legacy.exit_key_scheme, "legacy-public-label-v0");
        assert!(keyring.derive_for_session(&legacy, "claimer").is_err());
    }

    #[test]
    fn htlc_deterministic() {
        let h = [0x42u8; 32];
        let keyring = test_keyring();
        let a = build_htlc_addresses(&keyring, &h, "claimer", "refund", 6).unwrap();
        let b = build_htlc_addresses(&keyring, &h, "claimer", "refund", 6).unwrap();
        assert_eq!(a.address_btc, b.address_btc);
        assert!(a.address_btc.starts_with("tb1q"));
    }

    #[test]
    fn preimage_hash() {
        let s = [0x11u8; 32];
        assert_ne!(sha256_preimage(&s), [0u8; 32]);
    }

    #[test]
    fn extract_preimage_from_stack() {
        let pre = [0xABu8; 32];
        let stack = vec![
            vec![0x30, 0x01], // fake sig
            pre.to_vec(),
            vec![0x01],
            vec![0x63], // fake script
        ];
        assert_eq!(extract_preimage_from_witness_stack(&stack).unwrap(), pre);
    }

    #[test]
    fn extract_rejects_refund_empty_if_branch() {
        // Refund: [sig, empty, script] — stack len 3 and/or empty IF marker.
        let short = vec![vec![0x30], vec![], vec![0x63]];
        assert!(extract_preimage_from_witness_stack(&short).is_err());
        let refundish = vec![vec![0x30], vec![0x11; 32], vec![], vec![0x63]];
        let err = extract_preimage_from_witness_stack(&refundish).unwrap_err();
        assert!(
            err.to_string().contains("refund") || err.to_string().contains("empty IF"),
            "got {err}"
        );
    }

    #[test]
    fn extract_rejects_wrong_preimage_length() {
        let stack = vec![
            vec![0x30],
            vec![0x11; 16], // not 32
            vec![0x01],
            vec![0x63],
        ];
        let err = extract_preimage_from_witness_stack(&stack).unwrap_err();
        assert!(err.to_string().contains("32 bytes"), "got {err}");
    }

    #[test]
    fn extract_rejects_malformed_tx_hex() {
        assert!(extract_preimage_from_btc_tx_hex("not-hex").is_err());
        assert!(extract_preimage_from_btc_tx_hex("deadbeef").is_err());
        assert!(extract_preimage_from_liquid_tx_hex("zz").is_err());
    }

    #[test]
    fn extract_rejects_unrelated_empty_witness() {
        // Minimal invalid: decode may succeed on random bytes rarely; empty string fails.
        assert!(extract_preimage_from_btc_tx_hex("").is_err());
    }

    #[test]
    fn multi_out_btc_claim_builds() {
        let h = [0x42u8; 32];
        let keyring = test_keyring();
        let info = build_htlc_addresses(&keyring, &h, "claimer", "refund", 6).unwrap();
        let (sk, _) = keyring.derive("claimer").unwrap();
        let pre = [0x11u8; 32];
        let ws = hex::decode(&info.witness_script_hex).unwrap();
        let dest = p2wsh_spk(&ws); // reuse as dummy dest
        let commit = vec![0x51, 0x20];
        let mut commit_spk = commit;
        commit_spk.extend_from_slice(&[0x22u8; 32]);
        let hex_tx = build_htlc_spend_btc_outs(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            0,
            10_000,
            &[(500, commit_spk.as_slice()), (9_000, dest.as_slice())],
            &ws,
            HtlcSpend::Claim { preimage: &pre },
            6,
            &sk,
        )
        .unwrap();
        assert!(!hex_tx.is_empty());
        let got = extract_preimage_from_btc_tx_hex(&hex_tx).unwrap();
        assert_eq!(got, pre);
    }

    #[test]
    fn refund_btc_witness_rejects_preimage_extract() {
        let h = [0x42u8; 32];
        let keyring = test_keyring();
        let info = build_htlc_addresses(&keyring, &h, "claimer", "refund", 6).unwrap();
        let (sk, _) = keyring.derive("refund").unwrap();
        let ws = hex::decode(&info.witness_script_hex).unwrap();
        let dest = p2wsh_spk(&ws);
        let hex_tx = build_htlc_spend_btc(
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            0,
            10_000,
            9_500,
            dest.as_slice(),
            &ws,
            HtlcSpend::Refund,
            6,
            &sk,
        )
        .unwrap();
        let err = extract_preimage_from_btc_tx_hex(&hex_tx).unwrap_err();
        assert!(
            err.to_string().contains("claim")
                || err.to_string().contains("refund")
                || err.to_string().contains("short"),
            "got {err}"
        );
    }

    #[test]
    fn multi_out_liquid_claim_extracts_preimage() {
        let h = [0x42u8; 32];
        let keyring = test_keyring();
        let info = build_htlc_addresses(&keyring, &h, "claimer", "refund", 6).unwrap();
        let (sk, _) = keyring.derive("claimer").unwrap();
        let pre = [0x22u8; 32];
        let ws = hex::decode(&info.witness_script_hex).unwrap();
        let dest = p2wsh_spk(&ws);
        // Liquid testnet policy asset (explicit L-BTC).
        let lbtc = "144c654344aa716d6f3abcc1ca90e5641e4e2a7f633bc09fe3baf64585819a49";
        let mut commit_spk = vec![0x51, 0x20];
        commit_spk.extend_from_slice(&[0x33u8; 32]);
        let hex_tx = build_htlc_spend_liquid_outs(
            "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
            0,
            10_000,
            &[(330, commit_spk.as_slice()), (9_170, dest.as_slice())],
            500,
            lbtc,
            &ws,
            HtlcSpend::Claim { preimage: &pre },
            6,
            &sk,
        )
        .unwrap();
        let got = extract_preimage_from_liquid_tx_hex(&hex_tx).unwrap();
        assert_eq!(got, pre);
    }
}

/// Build + sign Elements/Liquid HTLC claim/refund (explicit L-BTC only).
#[allow(clippy::too_many_arguments)]
pub fn build_htlc_spend_liquid(
    prev_txid: &str,
    prev_vout: u32,
    input_value_sat: u64,
    output_value_sat: u64,
    fee_sat: u64,
    dest_spk: &[u8],
    lbtc_asset_hex: &str,
    witness_script: &[u8],
    spend: HtlcSpend,
    csv_delay: u32,
    signer_sk: &SecretKey,
) -> Result<String> {
    build_htlc_spend_liquid_outs(
        prev_txid,
        prev_vout,
        input_value_sat,
        &[(output_value_sat, dest_spk)],
        fee_sat,
        lbtc_asset_hex,
        witness_script,
        spend,
        csv_delay,
        signer_sk,
    )
}

/// Multi-output Liquid HTLC spend (S3: vout0=tapret, vout1=claimer, + fee out).
#[allow(clippy::too_many_arguments)]
pub fn build_htlc_spend_liquid_outs(
    prev_txid: &str,
    prev_vout: u32,
    input_value_sat: u64,
    outputs: &[(u64, &[u8])],
    fee_sat: u64,
    lbtc_asset_hex: &str,
    witness_script: &[u8],
    spend: HtlcSpend,
    csv_delay: u32,
    signer_sk: &SecretKey,
) -> Result<String> {
    use elements::confidential::{Asset, Nonce, Value};
    use elements::encode::serialize_hex;
    use elements::hashes::Hash as _;
    use elements::sighash::SighashCache;
    use elements::{
        AssetId, EcdsaSighashType, OutPoint, Script, Sequence, Transaction, TxIn, TxInWitness,
        TxOut, TxOutWitness, Txid,
    };
    use elements::secp256k1_zkp::Message as ElMessage;
    use elements::secp256k1_zkp::Secp256k1 as ElSecp;

    if outputs.is_empty() {
        bail!("need at least one output");
    }
    let out_sum: u64 = outputs.iter().map(|(v, _)| *v).sum();
    if out_sum + fee_sat != input_value_sat {
        bail!(
            "LQ claim: outputs+fee must equal input ({out_sum}+{fee_sat} != {input_value_sat})"
        );
    }
    let txid: Txid = prev_txid.parse().context("prev_txid")?;
    let asset_id: AssetId = lbtc_asset_hex.parse().context("asset id")?;
    let lbtc = Asset::Explicit(asset_id);

    let input = TxIn {
        previous_output: OutPoint::new(txid, prev_vout),
        is_pegin: false,
        script_sig: Script::new(),
        sequence: Sequence::from_consensus(htlc_sequence(&spend, csv_delay)),
        asset_issuance: Default::default(),
        witness: TxInWitness::default(),
    };

    let mut output: Vec<TxOut> = outputs
        .iter()
        .map(|(v, spk)| TxOut {
            asset: lbtc,
            value: Value::Explicit(*v),
            nonce: Nonce::Null,
            script_pubkey: Script::from(spk.to_vec()),
            witness: TxOutWitness::default(),
        })
        .collect();
    output.push(TxOut {
        asset: lbtc,
        value: Value::Explicit(fee_sat),
        nonce: Nonce::Null,
        script_pubkey: Script::new(),
        witness: TxOutWitness::default(),
    });

    let mut tx = Transaction {
        version: 2,
        lock_time: elements::LockTime::ZERO,
        input: vec![input],
        output,
    };

    let sighash = SighashCache::new(&tx).segwitv0_sighash(
        0,
        &Script::from(witness_script.to_vec()),
        Value::Explicit(input_value_sat),
        EcdsaSighashType::All,
    );

    let el_secp = ElSecp::new();
    let el_sk = elements::secp256k1_zkp::SecretKey::from_slice(&signer_sk.secret_bytes())
        .context("el secret key")?;
    let msg = ElMessage::from_digest(sighash.to_byte_array());
    let mut sig = el_secp.sign_ecdsa(&msg, &el_sk).serialize_der().to_vec();
    sig.push(EcdsaSighashType::All as u8);
    tx.input[0].witness.script_witness = htlc_witness_stack(sig, &spend, witness_script);
    Ok(serialize_hex(&tx))
}
