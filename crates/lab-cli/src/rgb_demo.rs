//! Bounded anonymous RGB lab: Issue -> Transfer -> Verify on Liquid Testnet.
//!
//! The visitor supplies only a Turnstile token. Wallets, recipient, asset,
//! amounts, broadcast choice, chain, and entropy are server constants.

use anyhow::{Context, Result};
use lab_core::{Config, DemoSwapPolicy, Floats};
use lab_rgb::storage::RgbStore;
use serde::Serialize;
use serde_json::{json, Value};

use crate::http_api::{handle_rgb_issue_post, handle_rgb_transfer_post, handle_verify_post};

pub const BUDGET_NAME: &str = "rgb_demo_budget";
pub const SENDER_WALLET: &str = "bob";
pub const RECEIVER_WALLET: &str = "alice";
pub const ASSET_NAME: &str = "SwapLab RGB Test Asset";
pub const ASSET_TICKER: &str = "SLAB";
pub const ASSET_SUPPLY: u64 = 1_000;
pub const TRANSFER_AMOUNT: u64 = 1;
pub const COMMITMENT_SATS: u64 = 500;
pub const RECEIVER_SATS: u64 = 500;
pub const STATIC_ENTROPY: u64 = 2_024;
pub const REBALANCE_TRIGGER_SATS: u64 = 50_000;
pub const REBALANCE_TARGET_SATS: u64 = 100_000;
pub const REBALANCE_SOURCE_FLOOR_SATS: u64 = 20_000;
pub const REBALANCE_MAX_TRANSFER_SATS: u64 = 25_000;
pub const MIN_LBTC_SEND_SATS: u64 = 500;

#[derive(Debug, Clone, Copy, Serialize)]
pub struct RebalancePolicy {
    pub trigger_below_sats: u64,
    pub target_sats: u64,
    pub source_floor_sats: u64,
    pub max_transfer_sats: u64,
}

impl Default for RebalancePolicy {
    fn default() -> Self {
        Self {
            trigger_below_sats: REBALANCE_TRIGGER_SATS,
            target_sats: REBALANCE_TARGET_SATS,
            source_floor_sats: REBALANCE_SOURCE_FLOOR_SATS,
            max_transfer_sats: REBALANCE_MAX_TRANSFER_SATS,
        }
    }
}

pub fn rebalance_policy_from_env() -> RebalancePolicy {
    RebalancePolicy {
        trigger_below_sats: env_u64(
            "LABD_RGB_DEMO_REBALANCE_TRIGGER_SATS",
            REBALANCE_TRIGGER_SATS,
        ),
        target_sats: env_u64("LABD_RGB_DEMO_REBALANCE_TARGET_SATS", REBALANCE_TARGET_SATS),
        source_floor_sats: env_u64(
            "LABD_RGB_DEMO_REBALANCE_SOURCE_FLOOR_SATS",
            REBALANCE_SOURCE_FLOOR_SATS,
        ),
        max_transfer_sats: env_u64(
            "LABD_RGB_DEMO_REBALANCE_MAX_TRANSFER_SATS",
            REBALANCE_MAX_TRANSFER_SATS,
        ),
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RebalancePlan {
    pub status: &'static str,
    pub from_wallet: &'static str,
    pub to_wallet: &'static str,
    pub source_balance_sats: Option<u64>,
    pub destination_balance_sats: Option<u64>,
    pub recommended_amount_sats: u64,
    pub operator_action_required: bool,
    pub reason: String,
    pub policy: RebalancePolicy,
}

pub fn rebalance_plan(
    source_balance_sats: Option<u64>,
    destination_balance_sats: Option<u64>,
    policy: RebalancePolicy,
) -> RebalancePlan {
    let base = |status, amount, required, reason| RebalancePlan {
        status,
        from_wallet: RECEIVER_WALLET,
        to_wallet: SENDER_WALLET,
        source_balance_sats,
        destination_balance_sats,
        recommended_amount_sats: amount,
        operator_action_required: required,
        reason,
        policy,
    };
    let (Some(source), Some(destination)) = (source_balance_sats, destination_balance_sats) else {
        return base(
            "unavailable",
            0,
            false,
            "Live Alice and Bob balances are required; unknown balances fail closed.".into(),
        );
    };
    if destination >= policy.trigger_below_sats {
        return base(
            "balanced",
            0,
            false,
            format!(
                "Bob is at or above the {} sat trigger.",
                policy.trigger_below_sats
            ),
        );
    }
    let desired = policy.target_sats.saturating_sub(destination);
    let available = source.saturating_sub(policy.source_floor_sats);
    let amount = desired.min(available).min(policy.max_transfer_sats);
    if amount < MIN_LBTC_SEND_SATS {
        return base(
            "blocked",
            0,
            true,
            "Rebalance is due, but the bounded transfer would be below the 500 sat minimum.".into(),
        );
    }
    base(
        "recommended",
        amount,
        true,
        format!("Move at most {amount} sats from Alice to Bob using the operator CLI."),
    )
}

#[derive(Debug)]
pub struct RunResult {
    pub public: Value,
    pub sender_debit_sats: u64,
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

fn env_u32(key: &str, default: u32) -> u32 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

fn env_truthy(key: &str) -> bool {
    std::env::var(key)
        .ok()
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

/// Independent governor policy for the public RGB run. Quotas are deploy-time
/// controls; none are accepted from the request body.
pub fn policy_from_env() -> DemoSwapPolicy {
    DemoSwapPolicy {
        enabled: env_truthy("LABD_RGB_DEMO"),
        turnstile_required: true,
        leg_sats: 1,
        max_fee_per_swap_sats: env_u64("LABD_RGB_DEMO_MAX_COST_SATS", 2_500).max(1),
        fee_budget_sats: env_u64("LABD_RGB_DEMO_BUDGET_SATS", 25_000),
        daily_cap: env_u32("LABD_RGB_DEMO_DAILY_CAP", 2),
        max_concurrent: 1,
        global_min_interval_secs: env_u64("LABD_RGB_DEMO_MIN_INTERVAL_SECS", 600),
        per_ip_hourly: env_u32("LABD_RGB_DEMO_PER_IP_HOURLY", 1),
        per_ip_daily: env_u32("LABD_RGB_DEMO_PER_IP_DAILY", 1),
        btc_floor_sats: 0,
        lq_floor_sats: env_u64("LABD_RGB_DEMO_LQ_FLOOR_SATS", 20_000),
        csv_delay: 2,
    }
}

pub fn fixed_parameters_json() -> Value {
    json!({
        "network": "liquid-testnet",
        "sender_wallet": SENDER_WALLET,
        "receiver_wallet": RECEIVER_WALLET,
        "asset": {"name": ASSET_NAME, "ticker": ASSET_TICKER, "supply": ASSET_SUPPLY},
        "transfer_amount": TRANSFER_AMOUNT,
        "commitment_sats": COMMITMENT_SATS,
        "receiver_sats": RECEIVER_SATS,
        "broadcast": true
    })
}

/// Observe only the fixed sender's L-BTC float. Unknown balance fails closed in
/// the governor; Bitcoin is deliberately irrelevant to this Liquid-only flow.
pub fn observe_floats(cfg: &Config) -> Result<Floats> {
    let balance =
        lab_chain::wallet_balance(cfg, SENDER_WALLET).context("RGB demo sender Liquid balance")?;
    Ok(Floats {
        btc_sats: 0,
        lq_sats: balance.lbtc_sats,
    })
}

fn receiver_address(cfg: &Config) -> Result<String> {
    let raw = std::fs::read_to_string(cfg.data_dir.join("wallet_registry.json"))
        .context("read public wallet registry")?;
    let registry: Value = serde_json::from_str(&raw).context("parse public wallet registry")?;
    let entry = registry
        .get("wallets")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|entry| entry.get("name").and_then(Value::as_str) == Some(RECEIVER_WALLET))
        .context("fixed receiver missing from wallet registry")?;
    let address = entry
        .get("address_0")
        .and_then(Value::as_str)
        .context("fixed receiver address missing from wallet registry")?;
    anyhow::ensure!(
        address.starts_with("tlq1"),
        "fixed receiver is not Liquid Testnet"
    );
    Ok(address.to_string())
}

/// Execute the full server-fixed workflow. Public output is selected field by
/// field so internal paths and raw transaction material never escape.
pub fn run(cfg: &Config) -> Result<RunResult> {
    let store = RgbStore::new(&cfg.data_dir);
    let receiver = receiver_address(cfg)?;

    let issue_body = json!({
        "wallet": SENDER_WALLET,
        "name": ASSET_NAME,
        "ticker": ASSET_TICKER,
        "supply": ASSET_SUPPLY,
        "chain": "liquid-testnet"
    })
    .to_string();
    let issued = handle_rgb_issue_post(cfg, &store, &issue_body)?;
    let contract_id = issued
        .pointer("/issue/contract_id")
        .and_then(Value::as_str)
        .context("issue result missing contract id")?;

    let transfer_body = json!({
        "contract": contract_id,
        "wallet": SENDER_WALLET,
        "amount": TRANSFER_AMOUNT,
        "broadcast": true,
        "entropy": STATIC_ENTROPY,
        "bob_sats": RECEIVER_SATS,
        "commitment_sats": COMMITMENT_SATS,
        "bob_address": receiver,
        "chain": "liquid-testnet"
    })
    .to_string();
    let transferred = handle_rgb_transfer_post(cfg, &store, &transfer_body)?;
    let plan_id = transferred
        .get("plan_id")
        .and_then(Value::as_str)
        .context("transfer result missing plan id")?;
    let txid = transferred
        .pointer("/broadcast/txid")
        .and_then(Value::as_str)
        .context("transfer result missing broadcast txid")?;
    let explorer_url = transferred
        .pointer("/broadcast/explorer_url")
        .and_then(Value::as_str)
        .unwrap_or("");
    let fee_sats = transferred
        .pointer("/broadcast/fee_sats")
        .and_then(Value::as_u64)
        .context("transfer result missing exact fee")?;
    let nonrecoverable_cost_sats = transferred
        .pointer("/broadcast/nonrecoverable_cost_sats")
        .and_then(Value::as_u64)
        .context("transfer result missing nonrecoverable cost")?;
    let sender_debit_sats = transferred
        .pointer("/broadcast/sender_debit_sats")
        .and_then(Value::as_u64)
        .context("transfer result missing sender debit")?;

    let verify_body = json!({"plan_id": plan_id, "txid": txid}).to_string();
    // Esplora can lag a successful broadcast briefly. Retry the read-only
    // witness fetch without ever rebroadcasting or changing the fixed plan.
    let mut verified = None;
    let mut last_error = None;
    for attempt in 0..8 {
        match handle_verify_post(cfg, &store, &verify_body) {
            Ok(value) => {
                verified = Some(value);
                break;
            }
            Err(error) => {
                last_error = Some(error);
                if attempt < 7 {
                    std::thread::sleep(std::time::Duration::from_secs(3));
                }
            }
        }
    }
    let verified = verified.ok_or_else(|| {
        last_error.unwrap_or_else(|| anyhow::anyhow!("verification witness unavailable"))
    })?;
    let proof_id = verified
        .get("proof_id")
        .and_then(Value::as_str)
        .context("verify result missing proof id")?;
    let verification = verified.get("result").cloned().unwrap_or(Value::Null);

    Ok(RunResult {
        public: json!({
            "status": "complete",
            "contract_id": contract_id,
            "plan_id": plan_id,
            "txid": txid,
            "proof_id": proof_id,
            "explorer_url": explorer_url,
            "verification": verification,
            "cost": {
                "fee_sats": fee_sats,
                "commitment_sats": COMMITMENT_SATS,
                "controlled_transfer_sats": RECEIVER_SATS,
                "nonrecoverable_cost_sats": nonrecoverable_cost_sats,
                "sender_debit_sats": sender_debit_sats
            },
            "parameters": fixed_parameters_json(),
            "note": "Testnet-only workflow between predefined lab wallets. No visitor parameters or keys are accepted."
        }),
        sender_debit_sats,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_policy_is_bounded() {
        let p = policy_from_env();
        assert!(p.turnstile_required);
        assert_eq!(p.max_concurrent, 1);
        assert_eq!(p.btc_floor_sats, 0);
        assert!(p.daily_cap <= 2);
        assert!(p.per_ip_daily <= 1);
    }

    #[test]
    fn parameters_are_fixed_and_testnet_only() {
        let v = fixed_parameters_json();
        assert_eq!(v["network"], "liquid-testnet");
        assert_eq!(v["sender_wallet"], SENDER_WALLET);
        assert_eq!(v["receiver_wallet"], RECEIVER_WALLET);
        assert_eq!(v["broadcast"], true);
        assert_eq!(v["transfer_amount"], 1);
        assert_eq!(v["receiver_sats"], 500);
    }

    #[test]
    fn rebalance_is_read_only_policy_output() {
        let p = RebalancePolicy::default();
        assert_eq!(
            rebalance_plan(Some(103_946), Some(143_545), p).status,
            "balanced"
        );
        let due = rebalance_plan(Some(100_000), Some(40_000), p);
        assert_eq!(due.status, "recommended");
        assert_eq!(due.recommended_amount_sats, 25_000);
        assert!(due.operator_action_required);
        assert_eq!(rebalance_plan(None, Some(1), p).status, "unavailable");
    }
}
