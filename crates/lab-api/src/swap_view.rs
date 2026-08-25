//! Public swap JSON (shared by CLI status and labd GET /v1/swap/*).
//!
//! Never include the session preimage on public surfaces.

use lab_rgb::swap::{SwapLegRgb, SwapPhase, SwapSession};
use serde_json::{json, Value};

/// Public leg metadata (no private consignment material).
pub fn public_leg(leg: &Option<SwapLegRgb>) -> Option<Value> {
    leg.as_ref().map(|r| {
        json!({
            "contract_id": r.contract_id,
            "amount": r.amount,
            "htlc_seal": r.htlc_seal,
            "fund_plan_id": r.fund_plan_id,
            "fund_anchor_txid": r.fund_anchor_txid,
            "fund_verify": r.fund_verify,
            "claim_plan_id": r.claim_plan_id,
            "claim_anchor_txid": r.claim_anchor_txid,
            "claim_verify": r.claim_verify,
            "successor_seal": r.successor_seal,
        })
    })
}

/// Public swap view: `preimage_hex` always null; hash and seals ok.
pub fn public_swap_view(s: &SwapSession, lq_explorer: &str, btc_explorer: &str) -> Value {
    let tx_link = |ex: &str, tx: &Option<String>| {
        tx.as_ref()
            .map(|t| format!("{}/tx/{}", ex.trim_end_matches('/'), t))
    };
    json!({
        "id": s.id,
        "version": s.version,
        "phase": s.phase,
        "csv_delay": s.csv_delay,
        "hash_hex": s.hash_hex,
        "preimage_hex": null,
        "preimage_redacted": true,
        "rgb_wrap": s.rgb_wrap,
        "alice_btc_wallet": s.alice_btc_wallet,
        "bob_lq_wallet": s.bob_lq_wallet,
        "btc_contract_id": s.btc_contract_id,
        "lq_contract_id": s.lq_contract_id,
        "btc_rgb": public_leg(&s.btc_rgb),
        "lq_rgb": public_leg(&s.lq_rgb),
        "htlc_btc": {
            "address": s.htlc_btc.address_btc,
            "claimer_label": s.htlc_btc.claimer_label,
            "refund_label": s.htlc_btc.refund_label,
            "csv_delay": s.htlc_btc.csv_delay,
        },
        "htlc_lq": {
            "address": s.htlc_lq.address_liquid_unconf,
            "claimer_label": s.htlc_lq.claimer_label,
            "refund_label": s.htlc_lq.refund_label,
            "csv_delay": s.htlc_lq.csv_delay,
        },
        "btc_fund_txid": s.btc_fund_txid,
        "btc_fund_sats": s.btc_fund_sats,
        "lq_fund_txid": s.lq_fund_txid,
        "lq_fund_sats": s.lq_fund_sats,
        "lq_claim_txid": s.lq_claim_txid,
        "btc_claim_txid": s.btc_claim_txid,
        "lq_refund_txid": s.lq_refund_txid,
        "btc_refund_txid": s.btc_refund_txid,
        "links": {
            "btc_fund": tx_link(btc_explorer, &s.btc_fund_txid),
            "lq_fund": tx_link(lq_explorer, &s.lq_fund_txid),
            "lq_claim": tx_link(lq_explorer, &s.lq_claim_txid),
            "btc_claim": tx_link(btc_explorer, &s.btc_claim_txid),
            "lq_refund": tx_link(lq_explorer, &s.lq_refund_txid),
            "btc_refund": tx_link(btc_explorer, &s.btc_refund_txid),
        },
        "notes": s.notes,
        "steps": [
            {"id": "created", "done": true, "label": "Created"},
            {"id": "funded_btc", "done": s.btc_fund_txid.is_some(), "label": "Fund BTC HTLC"},
            {"id": "funded_lq", "done": s.lq_fund_txid.is_some(), "label": "Fund Liquid HTLC"},
            {"id": "claimed_lq", "done": s.lq_claim_txid.is_some(), "label": "Alice claims LQ (reveals preimage)"},
            {"id": "claimed_btc", "done": s.btc_claim_txid.is_some(), "label": "Bob claims BTC"},
            {"id": "refunded_lq", "done": s.lq_refund_txid.is_some(), "label": "Bob refunds LQ after CSV"},
            {"id": "refunded_btc", "done": s.btc_refund_txid.is_some(), "label": "Alice refunds BTC after CSV"},
            {"id": "done", "done": matches!(s.phase, SwapPhase::Done), "label": "Done"},
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lab_rgb::swap::init_swap;

    #[test]
    fn public_view_never_exposes_preimage() {
        let keyring = lab_rgb::htlc::DemoKeyring::new([0x42; 32]).unwrap();
        let s = init_swap(
            &keyring,
            "p",
            6,
            "a",
            "b",
            Some("c".into()),
            None,
            true,
        )
        .unwrap();
        assert!(!s.preimage_hex.is_empty());
        let v = public_swap_view(&s, "https://lq.example", "https://btc.example");
        assert!(v.get("preimage_hex").unwrap().is_null());
        assert_eq!(v.get("preimage_redacted"), Some(&json!(true)));
        // Ensure raw preimage string is not embedded elsewhere.
        let dump = v.to_string();
        assert!(!dump.contains(&s.preimage_hex));
    }

    #[test]
    fn public_view_exposes_partial_refund_progress_without_secrets() {
        let keyring = lab_rgb::htlc::DemoKeyring::new([0x43; 32]).unwrap();
        let mut s = init_swap(&keyring, "partial", 6, "a", "b", None, None, false).unwrap();
        s.btc_fund_txid = Some("btc-fund".into());
        s.lq_fund_txid = Some("lq-fund".into());
        lab_rgb::swap::record_lq_refund(&mut s, "lq-refund".into());

        let v = public_swap_view(&s, "https://lq.example", "https://btc.example");
        assert_eq!(v.get("phase"), Some(&json!("refunding")));
        assert_eq!(v.get("lq_refund_txid"), Some(&json!("lq-refund")));
        assert!(v.get("btc_refund_txid").unwrap().is_null());
        assert_eq!(
            v.pointer("/links/lq_refund"),
            Some(&json!("https://lq.example/tx/lq-refund"))
        );
        assert!(v.get("preimage_hex").unwrap().is_null());
    }
}
