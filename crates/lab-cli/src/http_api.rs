//! HTTP handler helpers shared by Axum labd and legacy TCP server.
use std::fs;

use anyhow::{Context, Result};
use lab_core::Config;
use lab_rgb::storage::RgbStore;
use lab_rgb::swap::{self, SwapStore};
use lab_rgb::{
    issue_nia, plan_transfer, verify_against_witness, IssueRequest, DEMO_INTERNAL_XONLY_HEX,
};
use lab_rgb::htlc;

/// Public swap JSON: never expose preimage (shared `lab_api::public_swap_view`).
pub(crate) fn public_swap_view(s: &lab_rgb::swap::SwapSession, cfg: &Config) -> serde_json::Value {
    let btc_ex = std::env::var("BTC_TESTNET_EXPLORER")
        .unwrap_or_else(|_| "https://blockstream.info/testnet".into());
    let mut v = lab_api::public_swap_view(s, &cfg.explorer_base, &btc_ex);
    if let Some(obj) = v.as_object_mut() {
        obj.insert("next_actions".into(), serde_json::json!(swap_next_actions(s)));
        obj.insert("guide".into(), serde_json::json!(swap_guide(s)));
        obj.insert(
            "not_done_reason".into(),
            serde_json::json!(not_done_reason(s)),
        );
        obj.insert(
            "mode".into(),
            serde_json::json!(if s.rgb_wrap {
                "rgb_wrap"
            } else {
                "value_htlc"
            }),
        );
    }
    v
}

fn leg_wrapped(leg: &Option<lab_rgb::swap::SwapLegRgb>) -> bool {
    leg.as_ref()
        .and_then(|r| r.fund_transition_opid_hex.as_ref())
        .map(|s| !s.is_empty())
        .unwrap_or(false)
}

fn leg_claim_valid(leg: &Option<lab_rgb::swap::SwapLegRgb>) -> bool {
    leg.as_ref()
        .and_then(|r| r.claim_verify.as_deref())
        .map(|s| s == "valid")
        .unwrap_or(false)
}

/// Why phase is not yet `done` (browser-facing; never mentions preimage bytes).
pub(crate) fn not_done_reason(s: &lab_rgb::swap::SwapSession) -> Option<String> {
    use lab_rgb::swap::SwapPhase;
    if matches!(s.phase, SwapPhase::Done | SwapPhase::Refunded) {
        return None;
    }
    if s.btc_fund_txid.is_none() {
        return Some("BTC HTLC not funded yet".into());
    }
    if s.lq_fund_txid.is_none() {
        return Some("Liquid HTLC not funded yet".into());
    }
    if s.rgb_wrap {
        if s.btc_contract_id.is_some() && !leg_wrapped(&s.btc_rgb) {
            return Some("BTC RGB fund-wrap missing (run fund_btc with rgb_wrap)".into());
        }
        if s.lq_contract_id.is_some() && !leg_wrapped(&s.lq_rgb) {
            return Some("Liquid RGB fund-wrap missing (run fund_lq with rgb_wrap)".into());
        }
    }
    if s.lq_claim_txid.is_none() {
        return Some(if s.rgb_wrap {
            "Liquid claim not done (Alice: claim_lq re-anchors RGB + publishes preimage on-chain)"
                .into()
        } else {
            "Liquid claim not done (Alice: claim_lq publishes preimage on-chain)".into()
        });
    }
    if s.btc_claim_txid.is_none() {
        return Some(if s.rgb_wrap {
            "BTC claim not done (Bob: claim_btc with from_witness — server extracts preimage)"
                .into()
        } else {
            "BTC claim not done (Bob: claim_btc)".into()
        });
    }
    if s.rgb_wrap {
        if s.btc_contract_id.is_some() && !leg_claim_valid(&s.btc_rgb) {
            return Some(format!(
                "BTC RGB claim_verify={:?} (need valid for done)",
                s.btc_rgb.as_ref().and_then(|r| r.claim_verify.clone())
            ));
        }
        if s.lq_contract_id.is_some() && !leg_claim_valid(&s.lq_rgb) {
            return Some(format!(
                "Liquid RGB claim_verify={:?} (need valid for done)",
                s.lq_rgb.as_ref().and_then(|r| r.claim_verify.clone())
            ));
        }
    }
    Some("Value claims recorded; recompute phase or refresh".into())
}

/// Which mutations the lab console should offer (server-side keys).
pub(crate) fn swap_next_actions(s: &lab_rgb::swap::SwapSession) -> Vec<serde_json::Value> {
    use lab_rgb::swap::SwapPhase;
    let mut out = Vec::new();
    if matches!(s.phase, SwapPhase::Refunded | SwapPhase::Done) {
        return out;
    }
    let wrap = s.rgb_wrap;
    // Value fund missing, or S3 wrap still needed after value fund.
    let need_btc_fund = s.btc_fund_txid.is_none();
    let need_btc_wrap = wrap && s.btc_contract_id.is_some() && !leg_wrapped(&s.btc_rgb);
    if need_btc_fund || need_btc_wrap {
        let mut defaults = serde_json::json!({
            "amount_sats": 10000,
            "fee_sats": 800,
            "commitment_sats": 330,
            "entropy": 1,
        });
        if wrap {
            defaults
                .as_object_mut()
                .unwrap()
                .insert("rgb_wrap".into(), serde_json::json!(true));
        }
        out.push(serde_json::json!({
            "action": "fund_btc",
            "label": if need_btc_fund && wrap {
                "1. Fund BTC HTLC + RGB wrap"
            } else if need_btc_wrap {
                "1b. RGB wrap BTC onto HTLC"
            } else {
                "1. Fund BTC HTLC"
            },
            "defaults": defaults,
            "role": "alice (btc-alice)"
        }));
    }
    let need_lq_fund = s.lq_fund_txid.is_none();
    let need_lq_wrap = wrap && s.lq_contract_id.is_some() && !leg_wrapped(&s.lq_rgb);
    if need_lq_fund || need_lq_wrap {
        let mut defaults = serde_json::json!({
            "amount_sats": 5000,
            "commitment_sats": 330,
            "entropy": 1,
        });
        if wrap {
            defaults
                .as_object_mut()
                .unwrap()
                .insert("rgb_wrap".into(), serde_json::json!(true));
        }
        out.push(serde_json::json!({
            "action": "fund_lq",
            "label": if need_lq_fund && wrap {
                "2. Fund Liquid HTLC + RGB wrap"
            } else if need_lq_wrap {
                "2b. RGB wrap Liquid onto HTLC"
            } else {
                "2. Fund Liquid HTLC"
            },
            "defaults": defaults,
            "role": "bob"
        }));
    }
    if s.btc_fund_txid.is_some() && s.lq_fund_txid.is_some() && s.lq_claim_txid.is_none() {
        // Wait for wraps before offering claim on S3
        let wraps_ok = (!wrap || s.btc_contract_id.is_none() || leg_wrapped(&s.btc_rgb))
            && (!wrap || s.lq_contract_id.is_none() || leg_wrapped(&s.lq_rgb));
        if wraps_ok {
            let defaults = serde_json::json!({
                "fee_sats": 300,
                "commitment_sats": 330,
                "entropy": 1,
            });
            out.push(serde_json::json!({
                "action": "claim_lq",
                "label": if wrap {
                    "3. Claim Liquid (RGB re-anchor + preimage on-chain)"
                } else {
                    "3. Claim Liquid (Alice reveals preimage)"
                },
                "defaults": defaults,
                "role": "alice"
            }));
        }
    }
    if s.lq_claim_txid.is_some() && s.btc_claim_txid.is_none() {
        let defaults = serde_json::json!({
            "fee_sats": 500,
            "commitment_sats": 330,
            "entropy": 1,
            "from_witness": wrap,
        });
        out.push(serde_json::json!({
            "action": "claim_btc",
            "label": if wrap {
                "4. Claim BTC (from_witness + RGB re-anchor)"
            } else {
                "4. Claim BTC (Bob uses preimage)"
            },
            "defaults": defaults,
            "role": "bob"
        }));
    }
    if s.btc_fund_txid.is_some() && s.btc_claim_txid.is_none() {
        out.push(serde_json::json!({
            "action": "refund_btc",
            "label": "Refund BTC (after CSV)",
            "defaults": {"fee_sats": 500},
            "role": "alice",
            "caution": "Requires csv_delay confirmations since fund"
        }));
    }
    if s.lq_fund_txid.is_some() && s.lq_claim_txid.is_none() {
        out.push(serde_json::json!({
            "action": "refund_lq",
            "label": "Refund Liquid (after CSV)",
            "defaults": {"fee_sats": 300},
            "role": "bob",
            "caution": "Requires csv_delay confirmations since fund"
        }));
    }
    out
}

pub(crate) fn swap_guide(s: &lab_rgb::swap::SwapSession) -> String {
    if matches!(s.phase, lab_rgb::swap::SwapPhase::Done) {
        return if s.rgb_wrap {
            "S3 complete: both value claims and required RGB claim_verify=valid. Preimage never shown in this UI."
                .into()
        } else {
            "Swap complete. Preimage was revealed on Liquid claim; never shown in this UI.".into()
        };
    }
    if matches!(s.phase, lab_rgb::swap::SwapPhase::Refunded) {
        return "Refund path used. Happy-path claim is no longer available.".into();
    }
    if let Some(why) = not_done_reason(s) {
        return why;
    }
    "Refresh status.".into()
}

/// Map a pasted address to the lab wallet *name* when possible.
pub(crate) fn resolve_btc_wallet_name(s: &str) -> String {
    let t = s.trim();
    if t == "btc-alice"
        || t.eq_ignore_ascii_case("alice-btc")
        || t == "tb1q85aadpqgzjgrgp69gf2ejf0883yx7s9wy85h4p"
    {
        return "btc-alice".into();
    }
    // If user pasted a bech32 address, they almost always meant btc-alice in this lab.
    if t.starts_with("tb1") || t.starts_with("bc1") {
        return "btc-alice".into();
    }
    t.to_string()
}

fn resolve_lq_wallet_name(s: &str) -> String {
    let t = s.trim();
    if t.starts_with("tlq1") || t.starts_with("el1") || t.starts_with("lq1") {
        // Liquid addresses are not wallet names — default counterparty is bob.
        return "bob".into();
    }
    t.to_string()
}

pub(crate) fn handle_swap_init_post(
    cfg: &Config,
    store: &SwapStore,
    body: &str,
) -> Result<serde_json::Value> {
    let v: serde_json::Value = serde_json::from_str(body).context("json body")?;
    let id = v
        .get("id")
        .and_then(|x| x.as_str())
        .context("id required")?
        .to_string();
    let csv_delay = v.get("csv_delay").and_then(|x| x.as_u64()).unwrap_or(6) as u32;
    let alice_btc = resolve_btc_wallet_name(
        v.get("alice_btc")
            .and_then(|x| x.as_str())
            .unwrap_or("btc-alice"),
    );
    let bob_lq = resolve_lq_wallet_name(
        v.get("bob_lq")
            .and_then(|x| x.as_str())
            .unwrap_or("bob"),
    );
    let btc_contract = v
        .get("btc_contract")
        .or_else(|| v.get("btc_contract_id"))
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let lq_contract = v
        .get("lq_contract")
        .or_else(|| v.get("lq_contract_id"))
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    // refuse overwrite of existing without force
    if store.path_exists(&id)
        && !v.get("force").and_then(|x| x.as_bool()).unwrap_or(false)
    {
        anyhow::bail!("swap {id} already exists; pass force:true to overwrite");
    }
    let rgb_wrap = v.get("rgb_wrap").and_then(|x| x.as_bool()).unwrap_or(false);
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
    Ok(serde_json::json!({
        "status": "created",
        "stored": path.display().to_string(),
        "rgb_wrap": session.rgb_wrap,
        "swap": public_swap_view(&session, cfg),
        "note": "Preimage stored only under .rgbmvp/swaps/ (mode 600). Never returned by GET /v1/swap/*.",
    }))
}

pub(crate) fn handle_swap_action_post(
    cfg: &Config,
    store: &SwapStore,
    id: &str,
    body: &str,
) -> Result<serde_json::Value> {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::json!({}));
    let action = v
        .get("action")
        .and_then(|x| x.as_str())
        .context("action required (fund_btc|fund_lq|claim_lq|claim_btc|refund_btc|refund_lq)")?;
    let mut s = store.load(id)?;

    // Repair mistaken address-as-name from older sessions
    if s.alice_btc_wallet.starts_with("tb1") || s.alice_btc_wallet.starts_with("bc1") {
        s.alice_btc_wallet = resolve_btc_wallet_name(&s.alice_btc_wallet);
        store.save(&s)?;
    }
    if s.bob_lq_wallet.starts_with("tlq1")
        || s.bob_lq_wallet.starts_with("el1")
        || s.bob_lq_wallet.starts_with("lq1")
    {
        s.bob_lq_wallet = resolve_lq_wallet_name(&s.bob_lq_wallet);
        store.save(&s)?;
    }

    let result = match action {
        "set_contracts" => {
            if let Some(c) = v
                .get("btc_contract")
                .or_else(|| v.get("btc_contract_id"))
                .and_then(|x| x.as_str())
                .filter(|s| !s.is_empty())
            {
                s.btc_contract_id = Some(c.to_string());
                // Keep leg contract_id in sync when not yet wrap-funded.
                if !leg_wrapped(&s.btc_rgb) {
                    s.btc_rgb = Some(lab_rgb::swap::SwapLegRgb {
                        contract_id: c.to_string(),
                        ..Default::default()
                    });
                }
            }
            if let Some(c) = v
                .get("lq_contract")
                .or_else(|| v.get("lq_contract_id"))
                .and_then(|x| x.as_str())
                .filter(|s| !s.is_empty())
            {
                s.lq_contract_id = Some(c.to_string());
                if !leg_wrapped(&s.lq_rgb) {
                    s.lq_rgb = Some(lab_rgb::swap::SwapLegRgb {
                        contract_id: c.to_string(),
                        ..Default::default()
                    });
                }
            }
            store.save(&s)?;
            serde_json::json!({
                "status": "contracts_updated",
                "btc_contract_id": s.btc_contract_id,
                "lq_contract_id": s.lq_contract_id,
                "rgb_wrap": s.rgb_wrap,
                "note": if s.rgb_wrap {
                    "Twin contracts stored. Prefer: value-fund HTLC first (or keep issue seal on a UTXO LWK will not coin-select), then fund_* with rgb_wrap."
                } else {
                    "Twin RGB ids stored. For S3 re-anchor, re-init with rgb_wrap:true (or use CLI)."
                },
                "swap": public_swap_view(&s, cfg),
            })
        }
        "fund_btc" => {
            let amount_sats = v
                .get("amount_sats")
                .and_then(|x| x.as_u64())
                .unwrap_or(10_000);
            let fee_sats = v.get("fee_sats").and_then(|x| x.as_u64()).unwrap_or(800);
            let commitment_sats = v
                .get("commitment_sats")
                .and_then(|x| x.as_u64())
                .unwrap_or(330);
            let entropy = v.get("entropy").and_then(|x| x.as_u64()).unwrap_or(1);
            let do_wrap = v
                .get("rgb_wrap")
                .and_then(|x| x.as_bool())
                .unwrap_or(s.rgb_wrap)
                || s.rgb_wrap;
            let btc = lab_btc::BtcConfig::from_env();
            let wallet = resolve_btc_wallet_name(&s.alice_btc_wallet);
            s.alice_btc_wallet = wallet.clone();
            let svc = lab_api::SwapService::new(&cfg.data_dir);
            let mut broadcast = serde_json::Value::Null;
            let mut reused = false;
            if s.btc_fund_txid.is_some() {
                // Idempotent: do not double-fund value HTLC.
                reused = true;
            } else {
                let bc = lab_btc::fund_address(
                    cfg,
                    &btc,
                    &wallet,
                    &s.htlc_btc.address_btc,
                    amount_sats,
                    fee_sats,
                )?;
                s.btc_fund_txid = Some(bc.txid.clone());
                s.btc_fund_vout = Some(0);
                s.btc_fund_sats = Some(amount_sats);
                broadcast = serde_json::to_value(&bc)?;
            }
            let mut rgb_meta = serde_json::Value::Null;
            if do_wrap && s.btc_contract_id.is_some() && !leg_wrapped(&s.btc_rgb) {
                rgb_meta =
                    svc.fund_wrap_btc(cfg, &btc, &mut s, commitment_sats, entropy)?;
            } else if do_wrap && leg_wrapped(&s.btc_rgb) {
                rgb_meta = serde_json::json!({"status": "already_wrapped", "idempotent": true});
            }
            svc.recompute_and_save(&mut s)?;
            serde_json::json!({
                "status": "funded_btc",
                "phase": s.phase,
                "rgb_wrap": do_wrap,
                "reused_value_fund": reused,
                "broadcast": broadcast,
                "htlc_address": s.htlc_btc.address_btc,
                "rgb": rgb_meta,
                "swap": public_swap_view(&s, cfg),
            })
        }
        "fund_lq" => {
            let amount_sats = v
                .get("amount_sats")
                .and_then(|x| x.as_u64())
                .unwrap_or(5_000);
            let commitment_sats = v
                .get("commitment_sats")
                .and_then(|x| x.as_u64())
                .unwrap_or(330);
            let entropy = v.get("entropy").and_then(|x| x.as_u64()).unwrap_or(1);
            let do_wrap = v
                .get("rgb_wrap")
                .and_then(|x| x.as_bool())
                .unwrap_or(s.rgb_wrap)
                || s.rgb_wrap;
            let svc = lab_api::SwapService::new(&cfg.data_dir);
            let mut broadcast = serde_json::Value::Null;
            let mut reused = false;
            // Prefer existing HTLC UTXO (idempotent / retry after wrap failure).
            if let Ok((tx, vo, va)) = lab_chain::find_address_utxo(
                cfg,
                &s.htlc_lq.address_liquid_unconf,
                amount_sats.saturating_sub(1),
            ) {
                s.lq_fund_txid = Some(tx.clone());
                s.lq_fund_vout = Some(vo);
                s.lq_fund_sats = Some(va);
                reused = true;
                broadcast = serde_json::json!({
                    "txid": tx,
                    "reused": true,
                    "note": "reused existing HTLC UTXO",
                });
            } else if s.lq_fund_txid.is_none() {
                if do_wrap {
                    if let Some(cid) = s.lq_contract_id.as_ref() {
                        let rgb_store = RgbStore::new(&cfg.data_dir);
                        if let Ok(issue) = rgb_store.load_issue(cid) {
                            let utxos = lab_chain::wallet_utxos(cfg, &s.bob_lq_wallet)?;
                            let large: Vec<_> = utxos
                                .iter()
                                .filter(|u| u.value >= amount_sats.saturating_add(500))
                                .collect();
                            if large.len() == 1 && large[0].outpoint == issue.seal {
                                anyhow::bail!(
                                    "S3 fund_lq: only spendable UTXO is the RGB issue seal {}. \
                                     Split funds first, then retry.",
                                    issue.seal
                                );
                            }
                        }
                    }
                }
                let bc = lab_chain::send_lbtc(
                    cfg,
                    &s.bob_lq_wallet,
                    &s.htlc_lq.address_liquid_unconf,
                    amount_sats,
                )?;
                let (tx, vo, va) = lab_chain::find_address_utxo(
                    cfg,
                    &s.htlc_lq.address_liquid_unconf,
                    amount_sats.saturating_sub(1),
                )
                .unwrap_or((bc.txid.clone(), 0, amount_sats));
                s.lq_fund_txid = Some(tx);
                s.lq_fund_vout = Some(vo);
                s.lq_fund_sats = Some(va);
                broadcast = serde_json::to_value(&bc)?;
            } else {
                reused = true;
            }
            // Persist value fund even if wrap fails.
            swap::recompute_phase(&mut s);
            store.save(&s)?;
            let mut rgb_meta = serde_json::Value::Null;
            if do_wrap && s.lq_contract_id.is_some() && !leg_wrapped(&s.lq_rgb) {
                match svc.fund_wrap_lq(cfg, &mut s, commitment_sats, entropy) {
                    Ok(m) => {
                        rgb_meta = m;
                        svc.recompute_and_save(&mut s)?;
                    }
                    Err(e) => {
                        store.save(&s)?;
                        anyhow::bail!(
                            "LQ value funded (txid {}) but RGB wrap failed: {e}. \
                             Re-run fund_lq with rgb_wrap (HTLC UTXO will be reused).",
                            s.lq_fund_txid.as_deref().unwrap_or("?")
                        );
                    }
                }
            } else if do_wrap && leg_wrapped(&s.lq_rgb) {
                rgb_meta = serde_json::json!({"status": "already_wrapped", "idempotent": true});
                svc.recompute_and_save(&mut s)?;
            } else {
                svc.recompute_and_save(&mut s)?;
            }
            serde_json::json!({
                "status": "funded_lq",
                "phase": s.phase,
                "rgb_wrap": do_wrap,
                "reused_htlc_utxo": reused,
                "broadcast": broadcast,
                "htlc_address": s.htlc_lq.address_liquid_unconf,
                "rgb": rgb_meta,
                "swap": public_swap_view(&s, cfg),
            })
        }
        "claim_lq" => {
            if s.lq_claim_txid.is_some() {
                // Idempotent success
                return Ok(serde_json::json!({
                    "status": "claimed_lq",
                    "phase": s.phase,
                    "txid": s.lq_claim_txid,
                    "idempotent": true,
                    "swap": public_swap_view(&s, cfg),
                }));
            }
            let fee_sats = v.get("fee_sats").and_then(|x| x.as_u64()).unwrap_or(300);
            let commitment_sats = v
                .get("commitment_sats")
                .and_then(|x| x.as_u64())
                .unwrap_or(330);
            let entropy = v.get("entropy").and_then(|x| x.as_u64()).unwrap_or(1);
            let svc = lab_api::SwapService::new(&cfg.data_dir);
            let mut out = svc.claim_lq(cfg, &mut s, fee_sats, commitment_sats, entropy)?;
            svc.recompute_and_save(&mut s)?;
            out["note"] = serde_json::json!(
                "Preimage is public on Liquid; Bob can claim BTC. Not returned in API JSON."
            );
            out["swap"] = public_swap_view(&s, cfg);
            out
        }
        "claim_btc" => {
            if s.btc_claim_txid.is_some() {
                return Ok(serde_json::json!({
                    "status": "claimed_btc",
                    "phase": s.phase,
                    "txid": s.btc_claim_txid,
                    "idempotent": true,
                    "rgb_wrap": s.rgb_wrap,
                    "swap": public_swap_view(&s, cfg),
                }));
            }
            let fee_sats = v.get("fee_sats").and_then(|x| x.as_u64()).unwrap_or(500);
            let commitment_sats = v
                .get("commitment_sats")
                .and_then(|x| x.as_u64())
                .unwrap_or(330);
            let entropy = v.get("entropy").and_then(|x| x.as_u64()).unwrap_or(1);
            // S3 default: extract preimage from Liquid claim witness (never from browser).
            let from_witness = v
                .get("from_witness")
                .and_then(|x| x.as_bool())
                .unwrap_or(s.rgb_wrap);
            let svc = lab_api::SwapService::new(&cfg.data_dir);
            svc.claim_btc(
                cfg,
                &mut s,
                fee_sats,
                commitment_sats,
                entropy,
                from_witness,
            )?;
            svc.recompute_and_save(&mut s)?;
            serde_json::json!({
                "status": "claimed_btc",
                "phase": s.phase,
                "txid": s.btc_claim_txid,
                "rgb_wrap": s.rgb_wrap,
                "from_witness": from_witness,
                "swap": public_swap_view(&s, cfg),
            })
        }
        "refund_btc" => {
            let fee_sats = v.get("fee_sats").and_then(|x| x.as_u64()).unwrap_or(500);
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
            serde_json::json!({
                "status": "refunded_btc",
                "txid": txid,
                "explorer": format!("{}/tx/{}", btc.explorer_base, txid),
                "note": "Requires CSV maturity (nSequence = csv_delay blocks) since fund.",
            })
        }
        "refund_lq" => {
            let fee_sats = v.get("fee_sats").and_then(|x| x.as_u64()).unwrap_or(300);
            if s.lq_claim_txid.is_some() {
                anyhow::bail!("Liquid already claimed; cannot refund");
            }
            let amount = s.lq_fund_sats.context("lq not funded")?;
            let (txid, vout, value) = lab_chain::find_address_utxo(
                cfg,
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
            let claim_txid = lab_chain::broadcast_raw_hex(cfg, &raw)?;
            s.notes.push(format!("lq_refund_txid={claim_txid}"));
            s.phase = lab_rgb::swap::SwapPhase::Refunded;
            store.save(&s)?;
            serde_json::json!({
                "status": "refunded_lq",
                "txid": claim_txid,
                "explorer": format!("{}/tx/{}", cfg.explorer_base, claim_txid),
                "note": "Requires CSV maturity since fund.",
            })
        }
        other => anyhow::bail!("unknown action {other:?}"),
    };

    // reload for public view
    let s2 = store.load(id)?;
    Ok(serde_json::json!({
        "action": action,
        "result": result,
        "swap": public_swap_view(&s2, cfg),
    }))
}

pub(crate) fn list_swap_ids(data_dir: &std::path::Path) -> Result<Vec<String>> {
    let dir = data_dir.join("swaps");
    if !dir.exists() {
        return Ok(vec![]);
    }
    let mut ids: Vec<String> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.strip_suffix(".json").map(|s| s.to_string())
        })
        .collect();
    ids.sort();
    Ok(ids)
}

/// Read-only demo board: Liquid + BTC lab wallets and balances.
pub(crate) fn demo_wallets(cfg: &Config) -> Result<serde_json::Value> {
    let btc = lab_btc::BtcConfig::from_env();
    let mut wallets = Vec::new();

    for name in ["alice", "bob", "carol", "maker", "lab0"] {
        if !cfg.wallet_path(name).join("descriptor").exists() {
            continue;
        }
        let addr = lab_chain::wallet_address(cfg, name, Some(0)).ok();
        let bal = lab_chain::wallet_balance(cfg, name).ok();
        let role = std::fs::read_to_string(cfg.wallet_path(name).join("meta.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| v.get("role").and_then(|r| r.as_str().map(|x| x.to_string())));
        wallets.push(serde_json::json!({
            "name": name,
            "chain": "liquid-testnet",
            "role": role,
            "address": addr.as_ref().map(|a| &a.address),
            "lbtc_sats": bal.as_ref().map(|b| b.lbtc_sats),
            "balances_sats": bal.as_ref().map(|b| &b.balances_sats),
            "explorer": addr.as_ref().map(|a| format!(
                "{}/address/{}",
                cfg.explorer_base.trim_end_matches('/'),
                a.address
            )),
            "error": if addr.is_none() { Some("load failed") } else { None::<&str> },
        }));
    }

    if lab_btc::wallet_exists(cfg, "btc-alice") {
        let info = lab_btc::load_wallet_address(cfg, &btc, "btc-alice").ok();
        let bal = lab_btc::balance(cfg, &btc, "btc-alice").ok();
        wallets.push(serde_json::json!({
            "name": "btc-alice",
            "chain": "bitcoin-testnet",
            "role": "btc-alice",
            "address": info.as_ref().map(|i| &i.address),
            "btc_sats": bal.as_ref().map(|b| b.balance_sats),
            "utxo_count": bal.as_ref().map(|b| b.utxo_count),
            "explorer": info.as_ref().map(|i| &i.explorer_url),
        }));
    }

    Ok(serde_json::json!({
        "updated": true,
        "note": "Read-only demo board. No send/swap actions from the browser.",
        "wallets": wallets,
    }))
}

/// Recent swaps, RGB transfers, proofs (ids + paths only).
pub(crate) fn demo_activity(cfg: &Config) -> Result<serde_json::Value> {
    let swap_ids = list_swap_ids(&cfg.data_dir)?;
    let mut swaps = Vec::new();
    let ss = SwapStore::new(&cfg.data_dir);
    for id in &swap_ids {
        if let Ok(s) = ss.load(id) {
            swaps.push(serde_json::json!({
                "id": s.id,
                "phase": s.phase,
                "btc_fund_txid": s.btc_fund_txid,
                "lq_fund_txid": s.lq_fund_txid,
                "lq_claim_txid": s.lq_claim_txid,
                "btc_claim_txid": s.btc_claim_txid,
                "status_url": format!("/v1/swap/{}", s.id),
                "ui_url": format!("/?swap={}", s.id),
            }));
        }
    }

    let mut transfers = Vec::new();
    let tdir = cfg.data_dir.join("rgb/transfers");
    if tdir.exists() {
        for e in std::fs::read_dir(&tdir)?.filter_map(|e| e.ok()) {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.ends_with(".json") && !n.contains("broadcast") {
                transfers.push(n.trim_end_matches(".json").to_string());
            }
        }
        transfers.sort();
        transfers.reverse();
        transfers.truncate(20);
    }

    let mut proofs = Vec::new();
    let pdir = cfg.data_dir.join("rgb/proofs");
    if pdir.exists() {
        for e in std::fs::read_dir(&pdir)?.filter_map(|e| e.ok()) {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.ends_with(".json") {
                proofs.push(n.trim_end_matches(".json").to_string());
            }
        }
        proofs.sort();
        proofs.reverse();
        proofs.truncate(20);
    }

    Ok(serde_json::json!({
        "swaps": swaps,
        "rgb_transfer_plans": transfers,
        "rgb_proofs": proofs,
    }))
}

pub(crate) fn handle_verify_post(
    cfg: &Config,
    store: &RgbStore,
    body: &str,
) -> Result<serde_json::Value> {
    let v: serde_json::Value = serde_json::from_str(body).context("json body")?;
    let plan_id = v
        .get("plan_id")
        .or_else(|| v.get("plan"))
        .and_then(|x| x.as_str())
        .context("plan_id required")?;
    let txid = v
        .get("txid")
        .and_then(|x| x.as_str())
        .context("txid required")?;
    let plan = store.load_transfer(plan_id)?;
    let api = lab_chain::esplora_api_base(cfg);
    let witness = lab_chain::fetch_witness_esplora(&api, txid)?;
    let result = verify_against_witness(&plan, &witness, &cfg.explorer_base)?;
    let proof_id = format!("proof-{}", &txid[..16.min(txid.len())]);
    let path = store.save_proof(&proof_id, &result)?;
    Ok(serde_json::json!({
        "proof_id": proof_id,
        "stored": path.display().to_string(),
        "result": result,
    }))
}

pub(crate) fn list_rgb_contracts(cfg: &Config) -> Result<serde_json::Value> {
    let dir = cfg.data_dir.join("rgb/contracts");
    let mut contracts = Vec::new();
    if dir.exists() {
        for e in fs::read_dir(&dir)?.filter_map(|e| e.ok()) {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            if let Ok(raw) = fs::read_to_string(&p) {
                if let Ok(v) = serde_json::from_str::<lab_rgb::IssueResult>(&raw) {
                    contracts.push(v);
                }
            }
        }
    }
    contracts.sort_by(|a, b| a.contract_id.cmp(&b.contract_id));
    Ok(serde_json::json!({ "contracts": contracts, "count": contracts.len() }))
}

/// POST /v1/rgb/issue — server-side keys (lab fixtures). JSON:
/// `{ "wallet":"alice", "name":"…", "ticker":"tRGB", "supply":1000000, "chain":"liquid-testnet", "seal":null }`
pub(crate) fn handle_rgb_issue_post(
    cfg: &Config,
    store: &RgbStore,
    body: &str,
) -> Result<serde_json::Value> {
    let v: serde_json::Value = serde_json::from_str(body).context("json body")?;
    let wallet = v
        .get("wallet")
        .and_then(|x| x.as_str())
        .unwrap_or("alice");
    let name = v
        .get("name")
        .and_then(|x| x.as_str())
        .unwrap_or("Test RGB")
        .to_string();
    let ticker = v
        .get("ticker")
        .and_then(|x| x.as_str())
        .unwrap_or("tRGB")
        .to_string();
    let supply = v
        .get("supply")
        .and_then(|x| x.as_u64())
        .unwrap_or(1_000_000);
    let chain = v
        .get("chain")
        .and_then(|x| x.as_str())
        .unwrap_or("liquid-testnet")
        .to_string();
    let seal = if let Some(s) = v.get("seal").and_then(|x| x.as_str()) {
        s.to_string()
    } else if chain.starts_with("bitcoin") || chain == "testnet" || chain == "testnet3" {
        let btc = lab_btc::BtcConfig::from_env();
        lab_btc::pick_largest_utxo(cfg, &btc, wallet)?.outpoint
    } else {
        lab_chain::pick_lbtc_seal(cfg, wallet)?.outpoint
    };
    let issue = issue_nia(&IssueRequest {
        name,
        ticker,
        supply,
        seal: seal.clone(),
        chain: chain.clone(),
    })?;
    let path = store.save_issue(&issue)?;
    Ok(serde_json::json!({
        "status": "issued",
        "issue": issue,
        "stored": path.display().to_string(),
        "note": "Genesis is off-chain; seal UTXO must be closed by a transfer witness tx. Keys never left labd.",
    }))
}

/// POST /v1/rgb/transfer — plan (+ optional broadcast). JSON:
/// `{ "contract":"rgb:…"|ticker path, "wallet":"alice", "amount":600000, "broadcast":false, … }`
pub(crate) fn handle_rgb_transfer_post(
    cfg: &Config,
    store: &RgbStore,
    body: &str,
) -> Result<serde_json::Value> {
    let v: serde_json::Value = serde_json::from_str(body).context("json body")?;
    let contract = v
        .get("contract")
        .or_else(|| v.get("contract_id"))
        .and_then(|x| x.as_str())
        .context("contract required")?;
    let wallet = v
        .get("wallet")
        .and_then(|x| x.as_str())
        .unwrap_or("alice");
    let amount = v
        .get("amount")
        .and_then(|x| x.as_u64())
        .unwrap_or(600_000);
    let broadcast = v
        .get("broadcast")
        .and_then(|x| x.as_bool())
        .unwrap_or(false);
    let entropy = v.get("entropy").and_then(|x| x.as_u64()).unwrap_or(42);
    let bob_sats = v.get("bob_sats").and_then(|x| x.as_u64()).unwrap_or(1000);
    let commitment_sats = v
        .get("commitment_sats")
        .and_then(|x| x.as_u64())
        .unwrap_or(500);
    let bob_address = v
        .get("bob_address")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());

    let issue = store
        .load_issue(contract)
        .or_else(|_| {
            // try load by scanning contracts for matching contract_id
            let data = &cfg.data_dir;
            let dir = data.join("rgb/contracts");
            if dir.exists() {
                for e in fs::read_dir(&dir)?.filter_map(|e| e.ok()) {
                    let p = e.path();
                    if p.extension().and_then(|x| x.to_str()) != Some("json") {
                        continue;
                    }
                    if let Ok(raw) = fs::read_to_string(&p) {
                        if let Ok(iss) = serde_json::from_str::<lab_rgb::IssueResult>(&raw) {
                            if iss.contract_id == contract || p.file_stem().map(|s| s.to_string_lossy()) == Some(contract.into()) {
                                return Ok(iss);
                            }
                        }
                    }
                }
            }
            anyhow::bail!("contract not found: {contract}");
        })?;

    let chain = v
        .get("chain")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            if issue.chain_net.starts_with("bitcoin") {
                issue.chain_net.clone()
            } else {
                "liquid-testnet".into()
            }
        });

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
        "verify_hint": {
            "plan_id": plan_id,
            "next": "After broadcast, POST /v1/rgb/verify with plan_id + txid"
        }
    });

    if broadcast {
        let is_btc = chain.starts_with("bitcoin") || chain.contains("testnet3");
        let bc_val = if is_btc {
            let btc = lab_btc::BtcConfig::from_env();
            let utxos = lab_btc::utxos(cfg, &btc, wallet)?;
            let seal_val = utxos
                .iter()
                .find(|u| u.outpoint == issue.seal)
                .map(|u| u.value_sats)
                .context("seal UTXO not found in btc wallet")?;
            let fee = 800u64;
            let bc = lab_btc::broadcast_commitment_tx(
                cfg,
                &btc,
                wallet,
                &issue.seal,
                seal_val,
                &plan.tapret_address,
                commitment_sats,
                fee,
            )?;
            serde_json::to_value(bc)?
        } else {
            let bc = lab_chain::broadcast_commitment_tx(
                cfg,
                wallet,
                &issue.seal,
                &plan.tapret_address,
                bob_address.as_deref(),
                commitment_sats,
                bob_sats,
            )?;
            serde_json::to_value(bc)?
        };
        out["status"] = serde_json::json!("broadcast");
        out["broadcast"] = bc_val;
    }
    Ok(out)
}

/// POST /v1/audit/bfa — body is a BfaHistory JSON document (see docs/C3_CLOSED.md).
pub(crate) fn handle_bfa_audit_post(body: &str) -> Result<lab_rgb::bfa::BfaAuditResult> {
    let hist: lab_rgb::bfa::BfaHistory =
        serde_json::from_str(body).context("BFA history JSON")?;
    let fetch = |txid: &str| -> Result<String> {
        // Prefer embedded witness_tx_hex; if missing, try Elements regtest RPC helper.
        let out = std::process::Command::new("./scripts/regtest_simplicity.sh")
            .args(["cli", "getrawtransaction", txid])
            .output();
        match out {
            Ok(o) if o.status.success() => Ok(String::from_utf8_lossy(&o.stdout).trim().to_string()),
            _ => anyhow::bail!(
                "no witness_tx_hex for {txid} and regtest fetch failed (embed hex in history)"
            ),
        }
    };
    lab_rgb::bfa::audit_history(&hist, &fetch)
}
