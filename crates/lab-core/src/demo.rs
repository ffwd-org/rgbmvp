//! Bounded public demo swaps — policy + budget governor (W1/W2).
//!
//! Testnet only. See `docs/TESTNET_PUBLIC_SWAPS.md` (ADR-T1): the public may
//! trigger a swap, but never supplies protocol parameters, keys, or amounts.
//! Everything that costs money is fixed here, server-side.
//!
//! This module is deliberately **pure logic** — no clock, no network, no
//! filesystem. Every entry point takes the current epoch seconds and the
//! observed wallet floats from the caller, so the admission rules are
//! deterministically testable and cannot silently depend on ambient state.
//!
//! Budget rationale (measured 2026-08-10): BTC testnet is the scarce side
//! (btc-alice ~33.6k sats) and faucets are slow, so the demo is sized around
//! BTC fee burn. Liquid is ~14x more plentiful.

use std::collections::HashMap;
use std::env;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// Hard ceiling on demo leg size, regardless of environment. A misconfigured
/// `LABD_DEMO_LEG_SATS` can never push the demo above this.
pub const DEMO_MAX_LEG_SATS: u64 = 5_000;

/// The public demo always runs the **value-only** HTLC path. RGB-wrapped swaps
/// cost an extra tapret commitment dust output per leg plus additional
/// transactions, so they stay operator-only / token-gated.
pub const DEMO_RGB_WRAP: bool = false;

// Defaults — see the quota table in docs/TESTNET_PUBLIC_SWAPS.md §1a.
pub const DEFAULT_LEG_SATS: u64 = 1_000;
pub const DEFAULT_MAX_FEE_PER_SWAP_SATS: u64 = 1_800;
pub const DEFAULT_FEE_BUDGET_SATS: u64 = 28_000;
pub const DEFAULT_DAILY_CAP: u32 = 6;
pub const DEFAULT_MAX_CONCURRENT: u32 = 1;
pub const DEFAULT_GLOBAL_MIN_INTERVAL_SECS: u64 = 600;
pub const DEFAULT_PER_IP_HOURLY: u32 = 1;
pub const DEFAULT_PER_IP_DAILY: u32 = 2;
pub const DEFAULT_BTC_FLOOR_SATS: u64 = 5_000;
pub const DEFAULT_LQ_FLOOR_SATS: u64 = 20_000;
pub const DEFAULT_CSV_DELAY: u32 = 6;

const SECS_PER_DAY: u64 = 86_400;
const SECS_PER_HOUR: u64 = 3_600;

/// Server-fixed parameters and quotas for the public demo swap endpoint.
///
/// Contains no secrets: the Turnstile secret is read at verification time by
/// the HTTP layer and never stored here (this struct is `Debug`).
#[derive(Debug, Clone)]
pub struct DemoSwapPolicy {
    /// Master switch (`LABD_DEMO_SWAPS`). Off by default — independent of
    /// `LABD_PUBLIC_READ_ONLY`, so the read-only freeze stays the safe default.
    pub enabled: bool,
    /// Require a Turnstile token. Fail-closed: on by default.
    pub turnstile_required: bool,
    /// Sats per HTLC leg (server-fixed, clamped to [`DEMO_MAX_LEG_SATS`]).
    pub leg_sats: u64,
    /// Worst-case BTC fee reserved per swap for budget accounting.
    pub max_fee_per_swap_sats: u64,
    /// Total BTC fee budget for the whole demo run.
    pub fee_budget_sats: u64,
    /// Max swaps started per UTC day.
    pub daily_cap: u32,
    /// Max swaps in flight at once.
    pub max_concurrent: u32,
    /// Minimum seconds between two swap starts (global).
    pub global_min_interval_secs: u64,
    /// Per-IP starts allowed per rolling hour.
    pub per_ip_hourly: u32,
    /// Per-IP starts allowed per rolling day.
    pub per_ip_daily: u32,
    /// Pause the demo when the BTC swap wallet drops below this.
    pub btc_floor_sats: u64,
    /// Pause the demo when the Liquid swap wallet drops below this.
    pub lq_floor_sats: u64,
    /// HTLC CSV delay (blocks) for demo swaps.
    pub csv_delay: u32,
}

impl Default for DemoSwapPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            turnstile_required: true,
            leg_sats: DEFAULT_LEG_SATS,
            max_fee_per_swap_sats: DEFAULT_MAX_FEE_PER_SWAP_SATS,
            fee_budget_sats: DEFAULT_FEE_BUDGET_SATS,
            daily_cap: DEFAULT_DAILY_CAP,
            max_concurrent: DEFAULT_MAX_CONCURRENT,
            global_min_interval_secs: DEFAULT_GLOBAL_MIN_INTERVAL_SECS,
            per_ip_hourly: DEFAULT_PER_IP_HOURLY,
            per_ip_daily: DEFAULT_PER_IP_DAILY,
            btc_floor_sats: DEFAULT_BTC_FLOOR_SATS,
            lq_floor_sats: DEFAULT_LQ_FLOOR_SATS,
            csv_delay: DEFAULT_CSV_DELAY,
        }
    }
}

impl DemoSwapPolicy {
    pub fn from_env() -> Self {
        let d = Self::default();
        Self {
            enabled: env_truthy("LABD_DEMO_SWAPS"),
            // Fail-closed: only an explicit falsy value disables the bot check.
            turnstile_required: env::var("LABD_DEMO_TURNSTILE_REQUIRED")
                .map(|v| {
                    let t = v.trim().to_ascii_lowercase();
                    !(t == "0" || t == "false" || t == "no" || t == "off")
                })
                .unwrap_or(true),
            // Clamped: env can lower the leg size but never raise it past the cap.
            leg_sats: env_u64("LABD_DEMO_LEG_SATS", d.leg_sats)
                .clamp(1, DEMO_MAX_LEG_SATS),
            max_fee_per_swap_sats: env_u64(
                "LABD_DEMO_MAX_FEE_SATS",
                d.max_fee_per_swap_sats,
            )
            .max(1),
            fee_budget_sats: env_u64("LABD_DEMO_FEE_BUDGET_SATS", d.fee_budget_sats),
            daily_cap: env_u32("LABD_DEMO_DAILY_CAP", d.daily_cap),
            max_concurrent: env_u32("LABD_DEMO_MAX_CONCURRENT", d.max_concurrent).max(1),
            global_min_interval_secs: env_u64(
                "LABD_DEMO_MIN_INTERVAL_SECS",
                d.global_min_interval_secs,
            ),
            per_ip_hourly: env_u32("LABD_DEMO_PER_IP_HOURLY", d.per_ip_hourly),
            per_ip_daily: env_u32("LABD_DEMO_PER_IP_DAILY", d.per_ip_daily),
            btc_floor_sats: env_u64("LABD_DEMO_BTC_FLOOR_SATS", d.btc_floor_sats),
            lq_floor_sats: env_u64("LABD_DEMO_LQ_FLOOR_SATS", d.lq_floor_sats),
            csv_delay: env_u32("LABD_DEMO_CSV_DELAY", d.csv_delay).clamp(2, 144),
        }
    }
}

/// Observed on-chain floats at admission time. Supplied by the caller so this
/// module stays I/O-free; the HTTP layer is responsible for freshness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Floats {
    pub btc_sats: u64,
    pub lq_sats: u64,
}

/// Why a demo swap request was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DemoDenial {
    /// Feature flag off — reported as 404 so a disabled demo is not advertised.
    Disabled,
    TurnstileRequired,
    TurnstileFailed,
    /// Not enough time since the last swap started.
    GlobalInterval { retry_after_secs: u64 },
    PerIpQuota,
    DailyCap,
    Concurrency,
    FeeBudgetExhausted,
    /// A funding wallet dropped below its floor — demo paused, not broken.
    LowFloat { chain: &'static str },
    /// Floats could not be established; fail closed rather than overspend.
    FloatUnknown,
}

impl DemoDenial {
    pub fn status(&self) -> u16 {
        match self {
            DemoDenial::Disabled => 404,
            DemoDenial::TurnstileRequired | DemoDenial::TurnstileFailed => 403,
            DemoDenial::GlobalInterval { .. }
            | DemoDenial::PerIpQuota
            | DemoDenial::DailyCap
            | DemoDenial::Concurrency => 429,
            // "Paused", not "broken": the operator refills and it resumes.
            DemoDenial::FeeBudgetExhausted
            | DemoDenial::LowFloat { .. }
            | DemoDenial::FloatUnknown => 503,
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            DemoDenial::Disabled => "demo_disabled",
            DemoDenial::TurnstileRequired => "turnstile_required",
            DemoDenial::TurnstileFailed => "turnstile_failed",
            DemoDenial::GlobalInterval { .. } => "demo_cooldown",
            DemoDenial::PerIpQuota => "demo_per_ip_quota",
            DemoDenial::DailyCap => "demo_daily_cap",
            DemoDenial::Concurrency => "demo_busy",
            DemoDenial::FeeBudgetExhausted => "demo_budget_exhausted",
            DemoDenial::LowFloat { .. } => "demo_low_float",
            DemoDenial::FloatUnknown => "demo_float_unknown",
        }
    }

    pub fn message(&self) -> String {
        match self {
            DemoDenial::Disabled => "demo swaps are not enabled on this deployment".into(),
            DemoDenial::TurnstileRequired => "bot check required".into(),
            DemoDenial::TurnstileFailed => "bot check failed; reload and try again".into(),
            DemoDenial::GlobalInterval { retry_after_secs } => format!(
                "a demo swap started recently; retry in about {retry_after_secs}s"
            ),
            DemoDenial::PerIpQuota => {
                "you have reached the per-visitor demo swap limit; try again later".into()
            }
            DemoDenial::DailyCap => {
                "the daily demo swap budget is used up; try again tomorrow".into()
            }
            DemoDenial::Concurrency => {
                "a demo swap is already running; watch it or retry shortly".into()
            }
            DemoDenial::FeeBudgetExhausted => {
                "the testnet fee budget for this demo run is exhausted".into()
            }
            DemoDenial::LowFloat { chain } => {
                format!("demo paused: {chain} testnet float is low, awaiting faucet refill")
            }
            DemoDenial::FloatUnknown => {
                "demo paused: wallet balances are unavailable right now".into()
            }
        }
    }

    pub fn retry_after_secs(&self) -> Option<u64> {
        match self {
            DemoDenial::GlobalInterval { retry_after_secs } => Some(*retry_after_secs),
            _ => None,
        }
    }
}

/// Snapshot of governor state, for `GET /v1/demo/quota` and for persistence
/// (W4) so the fee budget survives restarts.
///
/// Contains only counters — no keys, addresses, or swap contents — so it is
/// safe to write to disk beside the session store.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DemoStatus {
    pub in_flight: u32,
    pub swaps_today: u32,
    pub swaps_total: u32,
    pub fee_spent_sats: u64,
    pub fee_reserved_sats: u64,
    /// Conservative charge for recovered reservations, unknown execution
    /// outcomes, and known future fees such as exit sweeps.
    pub fee_committed_sats: u64,
    pub day_index: u64,
    pub last_start_epoch: Option<u64>,
}

#[derive(Debug, Default)]
struct GovernorState {
    in_flight: u32,
    day_index: u64,
    swaps_today: u32,
    swaps_total: u32,
    fee_spent_sats: u64,
    /// Worst-case fees reserved by in-flight swaps. Reserving up front is what
    /// stops concurrent swaps from collectively overshooting the budget.
    fee_reserved_sats: u64,
    /// Fees that may already have reached the network or are still required to
    /// recover controlled outputs. They permanently consume budget.
    fee_committed_sats: u64,
    last_start_epoch: Option<u64>,
    /// IP -> epoch seconds of recent starts (pruned to 24h).
    per_ip: HashMap<String, Vec<u64>>,
}

/// Enforces every spending and rate limit for the public demo endpoint.
///
/// Admission reserves the worst-case fee; [`DemoGovernor::finish`] settles the
/// actual amount. Counters are intentionally **not** rolled back on failure, so
/// a caller cannot retry-spam by forcing swaps to fail early.
#[derive(Debug)]
pub struct DemoGovernor {
    policy: DemoSwapPolicy,
    state: Mutex<GovernorState>,
}

impl DemoGovernor {
    pub fn new(policy: DemoSwapPolicy) -> Self {
        Self {
            policy,
            state: Mutex::new(GovernorState::default()),
        }
    }

    pub fn from_env() -> Self {
        Self::new(DemoSwapPolicy::from_env())
    }

    pub fn policy(&self) -> &DemoSwapPolicy {
        &self.policy
    }

    pub fn enabled(&self) -> bool {
        self.policy.enabled
    }

    /// Restore persisted budget counters (W4).
    ///
    /// An in-flight reservation may already have paid a funding fee when the
    /// process died. Move every recovered reservation into a permanent,
    /// conservative commitment instead of releasing it. The concurrency slot
    /// does not survive because the original driver no longer exists.
    pub fn restore(&self, snapshot: &DemoStatus) {
        let mut st = self.lock();
        st.day_index = snapshot.day_index;
        st.swaps_today = snapshot.swaps_today;
        st.swaps_total = snapshot.swaps_total;
        st.fee_spent_sats = snapshot.fee_spent_sats;
        st.fee_committed_sats = snapshot
            .fee_committed_sats
            .saturating_add(snapshot.fee_reserved_sats);
        st.last_start_epoch = snapshot.last_start_epoch;
        st.in_flight = 0;
        st.fee_reserved_sats = 0;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, GovernorState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Roll the daily counter when the UTC day changes.
    fn roll_day(st: &mut GovernorState, now_epoch: u64) {
        let day = now_epoch / SECS_PER_DAY;
        if day != st.day_index {
            st.day_index = day;
            st.swaps_today = 0;
        }
    }

    /// Check every quota and, on success, reserve capacity for one swap.
    ///
    /// The caller must later settle with [`DemoGovernor::finish`], retain an
    /// unknown outcome with [`DemoGovernor::fail_closed`], or call
    /// [`DemoGovernor::abort`] only before execution starts.
    pub fn try_admit(
        &self,
        ip: &str,
        now_epoch: u64,
        floats: Option<Floats>,
    ) -> Result<(), DemoDenial> {
        if !self.policy.enabled {
            return Err(DemoDenial::Disabled);
        }

        // Fail closed: never spend when balances are unknown.
        let floats = floats.ok_or(DemoDenial::FloatUnknown)?;
        if floats.btc_sats < self.policy.btc_floor_sats {
            return Err(DemoDenial::LowFloat { chain: "bitcoin" });
        }
        if floats.lq_sats < self.policy.lq_floor_sats {
            return Err(DemoDenial::LowFloat { chain: "liquid" });
        }

        let mut st = self.lock();
        Self::roll_day(&mut st, now_epoch);

        if st.in_flight >= self.policy.max_concurrent {
            return Err(DemoDenial::Concurrency);
        }
        if st.swaps_today >= self.policy.daily_cap {
            return Err(DemoDenial::DailyCap);
        }

        // Budget: reserve the worst case so concurrent swaps cannot overshoot.
        let projected = st
            .fee_spent_sats
            .saturating_add(st.fee_reserved_sats)
            .saturating_add(st.fee_committed_sats)
            .saturating_add(self.policy.max_fee_per_swap_sats);
        if projected > self.policy.fee_budget_sats {
            return Err(DemoDenial::FeeBudgetExhausted);
        }

        if let Some(last) = st.last_start_epoch {
            let elapsed = now_epoch.saturating_sub(last);
            if elapsed < self.policy.global_min_interval_secs {
                return Err(DemoDenial::GlobalInterval {
                    retry_after_secs: self.policy.global_min_interval_secs - elapsed,
                });
            }
        }

        // Per-IP rolling windows.
        Self::prune_ips(&mut st, now_epoch);
        let hits = st.per_ip.get(ip).cloned().unwrap_or_default();
        let last_hour = hits
            .iter()
            .filter(|t| now_epoch.saturating_sub(**t) < SECS_PER_HOUR)
            .count() as u32;
        if last_hour >= self.policy.per_ip_hourly {
            return Err(DemoDenial::PerIpQuota);
        }
        let last_day = hits
            .iter()
            .filter(|t| now_epoch.saturating_sub(**t) < SECS_PER_DAY)
            .count() as u32;
        if last_day >= self.policy.per_ip_daily {
            return Err(DemoDenial::PerIpQuota);
        }

        // Admitted — commit the reservation.
        st.in_flight += 1;
        st.swaps_today += 1;
        st.swaps_total += 1;
        st.fee_reserved_sats += self.policy.max_fee_per_swap_sats;
        st.last_start_epoch = Some(now_epoch);
        st.per_ip.entry(ip.to_string()).or_default().push(now_epoch);
        Ok(())
    }

    /// Release the in-flight slot and settle the actual fee spent.
    ///
    /// Actual fees are never clamped to the reservation. A runtime overrun may
    /// take accounted spend past the ceiling, but recording it in full ensures
    /// every later admission fails instead of hiding expenditure.
    pub fn finish(&self, actual_fee_sats: u64) {
        self.finish_with_liability(actual_fee_sats, 0);
    }

    /// Settle an admitted operation while retaining a conservative charge for
    /// a known future fee (for example, sweeping its exit output).
    pub fn finish_with_liability(&self, actual_fee_sats: u64, committed_fee_sats: u64) {
        let mut st = self.lock();
        let reserved = self.policy.max_fee_per_swap_sats.min(st.fee_reserved_sats);
        st.fee_reserved_sats -= reserved;
        st.fee_spent_sats = st.fee_spent_sats.saturating_add(actual_fee_sats);
        st.fee_committed_sats = st
            .fee_committed_sats
            .saturating_add(committed_fee_sats);
        st.in_flight = st.in_flight.saturating_sub(1);
    }

    /// Release the in-flight slot before execution starts, when the caller can
    /// prove that no transaction was broadcast.
    ///
    /// Day/IP counters are deliberately kept so a failing swap still consumes
    /// quota (anti retry-spam).
    pub fn abort(&self) {
        let mut st = self.lock();
        let reserved = self.policy.max_fee_per_swap_sats.min(st.fee_reserved_sats);
        st.fee_reserved_sats -= reserved;
        st.in_flight = st.in_flight.saturating_sub(1);
    }

    /// Release the in-flight slot after execution began but the actual fee is
    /// unknown. Conservatively charge the full reservation so partial
    /// broadcasts and crashes cannot escape the persistent ceiling.
    pub fn fail_closed(&self) {
        let mut st = self.lock();
        let reserved = self.policy.max_fee_per_swap_sats.min(st.fee_reserved_sats);
        st.fee_reserved_sats -= reserved;
        st.fee_committed_sats = st.fee_committed_sats.saturating_add(reserved);
        st.in_flight = st.in_flight.saturating_sub(1);
    }

    pub fn status(&self, now_epoch: u64) -> DemoStatus {
        let mut st = self.lock();
        Self::roll_day(&mut st, now_epoch);
        DemoStatus {
            in_flight: st.in_flight,
            swaps_today: st.swaps_today,
            swaps_total: st.swaps_total,
            fee_spent_sats: st.fee_spent_sats,
            fee_reserved_sats: st.fee_reserved_sats,
            fee_committed_sats: st.fee_committed_sats,
            day_index: st.day_index,
            last_start_epoch: st.last_start_epoch,
        }
    }

    /// Remaining whole swaps the fee budget can still fund.
    pub fn swaps_remaining_in_budget(&self) -> u64 {
        let st = self.lock();
        let used = st
            .fee_spent_sats
            .saturating_add(st.fee_reserved_sats)
            .saturating_add(st.fee_committed_sats);
        self.policy
            .fee_budget_sats
            .saturating_sub(used)
            / self.policy.max_fee_per_swap_sats.max(1)
    }

    fn prune_ips(st: &mut GovernorState, now_epoch: u64) {
        st.per_ip.retain(|_, hits| {
            hits.retain(|t| now_epoch.saturating_sub(*t) < SECS_PER_DAY);
            !hits.is_empty()
        });
    }
}

fn env_truthy(key: &str) -> bool {
    env::var(key)
        .map(|v| {
            let t = v.trim().to_ascii_lowercase();
            t == "1" || t == "true" || t == "yes" || t == "on"
        })
        .unwrap_or(false)
}

fn env_u64(key: &str, default: u64) -> u64 {
    env::var(key)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(default)
}

fn env_u32(key: &str, default: u32) -> u32 {
    env::var(key)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u64 = 1_800_000_000;

    fn policy() -> DemoSwapPolicy {
        DemoSwapPolicy {
            enabled: true,
            turnstile_required: true,
            ..DemoSwapPolicy::default()
        }
    }

    fn rich() -> Option<Floats> {
        Some(Floats {
            btc_sats: 33_607,
            lq_sats: 146_633,
        })
    }

    /// Admission must be refused outright when the feature flag is off.
    #[test]
    fn disabled_by_default() {
        let g = DemoGovernor::new(DemoSwapPolicy::default());
        assert_eq!(g.try_admit("1.1.1.1", T0, rich()), Err(DemoDenial::Disabled));
        assert_eq!(DemoDenial::Disabled.status(), 404);
    }

    #[test]
    fn admits_then_blocks_on_concurrency() {
        let g = DemoGovernor::new(policy());
        assert!(g.try_admit("1.1.1.1", T0, rich()).is_ok());
        // Second request from a different IP, well past the cooldown.
        assert_eq!(
            g.try_admit("2.2.2.2", T0 + 10_000, rich()),
            Err(DemoDenial::Concurrency)
        );
        g.finish(350);
        assert!(g.try_admit("2.2.2.2", T0 + 10_000, rich()).is_ok());
    }

    #[test]
    fn global_cooldown_enforced() {
        let g = DemoGovernor::new(policy());
        assert!(g.try_admit("1.1.1.1", T0, rich()).is_ok());
        g.finish(300);
        match g.try_admit("2.2.2.2", T0 + 60, rich()) {
            Err(DemoDenial::GlobalInterval { retry_after_secs }) => {
                assert_eq!(retry_after_secs, DEFAULT_GLOBAL_MIN_INTERVAL_SECS - 60);
            }
            other => panic!("expected cooldown, got {other:?}"),
        }
    }

    #[test]
    fn per_ip_hourly_and_daily_quota() {
        let g = DemoGovernor::new(policy());
        assert!(g.try_admit("9.9.9.9", T0, rich()).is_ok());
        g.finish(300);
        // Same IP, past global cooldown but inside its hour.
        assert_eq!(
            g.try_admit("9.9.9.9", T0 + 700, rich()),
            Err(DemoDenial::PerIpQuota)
        );
        // Next hour is allowed (daily allowance is 2).
        assert!(g.try_admit("9.9.9.9", T0 + 2 * SECS_PER_HOUR, rich()).is_ok());
        g.finish(300);
        // Third in the same day exceeds the daily allowance.
        assert_eq!(
            g.try_admit("9.9.9.9", T0 + 4 * SECS_PER_HOUR, rich()),
            Err(DemoDenial::PerIpQuota)
        );
    }

    #[test]
    fn daily_cap_and_rollover() {
        let p = DemoSwapPolicy {
            daily_cap: 2,
            global_min_interval_secs: 0,
            per_ip_hourly: 99,
            per_ip_daily: 99,
            ..policy()
        };
        let g = DemoGovernor::new(p);
        for i in 0..2 {
            assert!(g.try_admit("1.1.1.1", T0 + i, rich()).is_ok());
            g.finish(100);
        }
        assert_eq!(
            g.try_admit("1.1.1.1", T0 + 10, rich()),
            Err(DemoDenial::DailyCap)
        );
        // Next UTC day resets the counter.
        assert!(g.try_admit("1.1.1.1", T0 + SECS_PER_DAY + 10, rich()).is_ok());
    }

    /// The budget is the hard stop: reservations must prevent overshoot.
    #[test]
    fn fee_budget_is_a_hard_stop() {
        let p = DemoSwapPolicy {
            fee_budget_sats: 1_000,
            max_fee_per_swap_sats: 400,
            global_min_interval_secs: 0,
            daily_cap: 99,
            per_ip_hourly: 99,
            per_ip_daily: 99,
            ..policy()
        };
        let g = DemoGovernor::new(p);
        // 400 + 400 = 800 reserved/spent; a third would project 1200 > 1000.
        assert!(g.try_admit("1.1.1.1", T0, rich()).is_ok());
        g.finish(400);
        assert!(g.try_admit("1.1.1.1", T0 + 1, rich()).is_ok());
        g.finish(400);
        assert_eq!(
            g.try_admit("1.1.1.1", T0 + 2, rich()),
            Err(DemoDenial::FeeBudgetExhausted)
        );
        assert_eq!(DemoDenial::FeeBudgetExhausted.status(), 503);
    }

    /// Concurrent in-flight swaps must not collectively exceed the budget.
    #[test]
    fn reservation_blocks_concurrent_overshoot() {
        let p = DemoSwapPolicy {
            fee_budget_sats: 700,
            max_fee_per_swap_sats: 400,
            max_concurrent: 5,
            global_min_interval_secs: 0,
            daily_cap: 99,
            per_ip_hourly: 99,
            per_ip_daily: 99,
            ..policy()
        };
        let g = DemoGovernor::new(p);
        assert!(g.try_admit("1.1.1.1", T0, rich()).is_ok());
        // Nothing settled yet, but 400 is reserved: 400+400=800 > 700.
        assert_eq!(
            g.try_admit("1.1.1.1", T0 + 1, rich()),
            Err(DemoDenial::FeeBudgetExhausted)
        );
    }

    #[test]
    fn low_float_pauses_demo() {
        let g = DemoGovernor::new(policy());
        let low_btc = Some(Floats {
            btc_sats: 4_999,
            lq_sats: 999_999,
        });
        assert_eq!(
            g.try_admit("1.1.1.1", T0, low_btc),
            Err(DemoDenial::LowFloat { chain: "bitcoin" })
        );
        let low_lq = Some(Floats {
            btc_sats: 999_999,
            lq_sats: 19_999,
        });
        assert_eq!(
            g.try_admit("1.1.1.1", T0, low_lq),
            Err(DemoDenial::LowFloat { chain: "liquid" })
        );
    }

    /// Unknown balances must fail closed, never spend optimistically.
    #[test]
    fn unknown_floats_fail_closed() {
        let g = DemoGovernor::new(policy());
        assert_eq!(
            g.try_admit("1.1.1.1", T0, None),
            Err(DemoDenial::FloatUnknown)
        );
    }

    /// A failed swap still consumes quota, so failures cannot be spammed.
    #[test]
    fn abort_releases_slot_but_keeps_quota() {
        let g = DemoGovernor::new(policy());
        assert!(g.try_admit("5.5.5.5", T0, rich()).is_ok());
        g.abort();
        let st = g.status(T0);
        assert_eq!(st.in_flight, 0);
        assert_eq!(st.fee_reserved_sats, 0);
        assert_eq!(st.fee_spent_sats, 0, "aborted swap spends nothing");
        assert_eq!(st.swaps_today, 1, "but it still consumed daily quota");
        assert_eq!(
            g.try_admit("5.5.5.5", T0 + 700, rich()),
            Err(DemoDenial::PerIpQuota)
        );
    }

    #[test]
    fn unknown_execution_failure_charges_full_reservation() {
        let g = DemoGovernor::new(policy());
        assert!(g.try_admit("5.5.5.5", T0, rich()).is_ok());
        g.fail_closed();
        let st = g.status(T0);
        assert_eq!(st.in_flight, 0);
        assert_eq!(st.fee_reserved_sats, 0);
        assert_eq!(
            st.fee_committed_sats,
            DEFAULT_MAX_FEE_PER_SWAP_SATS
        );
        assert_eq!(st.fee_spent_sats, 0, "actual fee remains unknown");
    }

    #[test]
    fn actual_fee_overrun_is_fully_accounted_and_stops_admission() {
        let g = DemoGovernor::new(DemoSwapPolicy {
            fee_budget_sats: DEFAULT_MAX_FEE_PER_SWAP_SATS,
            global_min_interval_secs: 0,
            ..policy()
        });
        assert!(g.try_admit("1.1.1.1", T0, rich()).is_ok());
        g.finish_with_liability(2_300, 500);
        let st = g.status(T0);
        assert_eq!(st.fee_spent_sats, 2_300);
        assert_eq!(st.fee_committed_sats, 500);
        assert_eq!(st.fee_reserved_sats, 0);
        assert_eq!(g.swaps_remaining_in_budget(), 0);
        assert_eq!(
            g.try_admit("2.2.2.2", T0 + 1, rich()),
            Err(DemoDenial::FeeBudgetExhausted)
        );
    }

    #[test]
    fn future_liability_remains_charged_after_success() {
        let g = DemoGovernor::new(policy());
        assert!(g.try_admit("1.1.1.1", T0, rich()).is_ok());
        g.finish_with_liability(1_300, 500);
        let st = g.status(T0);
        assert_eq!(st.fee_spent_sats, 1_300);
        assert_eq!(st.fee_committed_sats, 500);
        assert_eq!(st.fee_reserved_sats, 0);
        assert_eq!(g.swaps_remaining_in_budget(), 14);
    }

    #[test]
    fn leg_size_is_clamped_to_hard_ceiling() {
        // Simulates a misconfigured env value far above the cap.
        let clamped = 1_000_000u64.clamp(1, DEMO_MAX_LEG_SATS);
        assert_eq!(clamped, DEMO_MAX_LEG_SATS);
        assert!(!DEMO_RGB_WRAP, "public demo must use the value-only path");
    }

    #[test]
    fn budget_headroom_reporting() {
        let p = DemoSwapPolicy {
            fee_budget_sats: 28_000,
            max_fee_per_swap_sats: 400,
            ..policy()
        };
        let g = DemoGovernor::new(p);
        assert_eq!(g.swaps_remaining_in_budget(), 70);
    }

    #[test]
    fn restore_recovers_budget_but_not_in_flight() {
        let g = DemoGovernor::new(policy());
        g.restore(&DemoStatus {
            in_flight: 3,
            swaps_today: 4,
            swaps_total: 40,
            fee_spent_sats: 16_000,
            fee_reserved_sats: 800,
            fee_committed_sats: 200,
            day_index: T0 / SECS_PER_DAY,
            last_start_epoch: Some(T0),
        });
        let st = g.status(T0);
        assert_eq!(st.in_flight, 0, "in-flight never survives a restart");
        assert_eq!(st.fee_reserved_sats, 0);
        assert_eq!(st.fee_spent_sats, 16_000, "spend budget is restored");
        assert_eq!(
            st.fee_committed_sats, 1_000,
            "persisted reservations become conservative recovery debits"
        );
        assert_eq!(st.swaps_today, 4);
    }

    // -----------------------------------------------------------------
    // W8 — abuse simulation. These assert the *invariants* that make the
    // demo safe to expose, under adversarial traffic rather than happy path.
    // -----------------------------------------------------------------

    /// A distributed burst (many IPs, no cooldown respected, a full simulated
    /// day) must never breach the daily cap, the concurrency cap, or the fee
    /// budget. This is the property that makes a public trigger acceptable.
    #[test]
    fn abuse_burst_cannot_breach_any_cap() {
        let p = DemoSwapPolicy {
            enabled: true,
            daily_cap: 6,
            max_concurrent: 1,
            global_min_interval_secs: 600,
            per_ip_hourly: 1,
            per_ip_daily: 2,
            fee_budget_sats: 28_000,
            max_fee_per_swap_sats: 400,
            ..DemoSwapPolicy::default()
        };
        let g = DemoGovernor::new(p);

        let mut admitted = 0u32;
        let mut peak_in_flight = 0u32;
        // 2000 attempts from 200 distinct IPs across one simulated day.
        for i in 0..2000u64 {
            let ip = format!("10.0.{}.{}", (i / 256) % 256, i % 256);
            let now = T0 + (i * 43); // ~24h of traffic
            if g.try_admit(&ip, now, rich()).is_ok() {
                admitted += 1;
                let st = g.status(now);
                peak_in_flight = peak_in_flight.max(st.in_flight);
                // Half the swaps "fail" — must not refund quota.
                if i % 2 == 0 {
                    g.finish(400);
                } else {
                    g.abort();
                }
            }
        }
        let st = g.status(T0 + 2000 * 43);
        assert!(
            peak_in_flight <= 1,
            "concurrency cap breached: peak={peak_in_flight}"
        );
        assert!(
            st.fee_spent_sats <= 28_000,
            "fee budget breached: {}",
            st.fee_spent_sats
        );
        // One simulated day spans ~1 daily-cap window; admissions are bounded
        // by cap * days, never by the flood size.
        assert!(admitted < 20, "daily cap ineffective: admitted={admitted}");
        assert!(admitted > 0, "legitimate traffic must still get through");
    }

    /// A single IP hammering the endpoint gets at most its per-IP allowance,
    /// no matter how many attempts it makes.
    #[test]
    fn single_ip_flood_is_bounded_by_per_ip_quota() {
        let p = DemoSwapPolicy {
            enabled: true,
            daily_cap: 100,
            global_min_interval_secs: 0,
            per_ip_hourly: 1,
            per_ip_daily: 2,
            ..DemoSwapPolicy::default()
        };
        let g = DemoGovernor::new(p);
        let mut ok = 0;
        for i in 0..500u64 {
            if g.try_admit("6.6.6.6", T0 + i * 60, rich()).is_ok() {
                ok += 1;
                g.finish(400);
            }
        }
        assert_eq!(ok, 2, "per-IP daily allowance must bound a flood, got {ok}");
    }

    /// Budget exhaustion must degrade to a clean paused state, not to
    /// overspending and not to a panic.
    #[test]
    fn budget_exhaustion_degrades_gracefully() {
        let p = DemoSwapPolicy {
            enabled: true,
            daily_cap: 1000,
            global_min_interval_secs: 0,
            per_ip_hourly: 1000,
            per_ip_daily: 1000,
            fee_budget_sats: 2_000,
            max_fee_per_swap_sats: 400,
            ..DemoSwapPolicy::default()
        };
        let g = DemoGovernor::new(p);
        let mut ok = 0;
        for i in 0..100u64 {
            match g.try_admit("1.2.3.4", T0 + i, rich()) {
                Ok(()) => {
                    ok += 1;
                    g.finish(400);
                }
                Err(DemoDenial::FeeBudgetExhausted) => {}
                Err(other) => panic!("unexpected denial {other:?}"),
            }
        }
        assert_eq!(ok, 5, "2000/400 = 5 swaps then a hard stop");
        assert_eq!(g.status(T0).fee_spent_sats, 2_000);
        assert_eq!(g.swaps_remaining_in_budget(), 0);
    }

    /// Chaos: a restart mid-flight must not lose the spend ceiling nor leave a
    /// permanently occupied concurrency slot.
    #[test]
    fn restart_midflight_recovers_without_losing_budget_or_wedging() {
        let p = DemoSwapPolicy {
            enabled: true,
            global_min_interval_secs: 0,
            ..DemoSwapPolicy::default()
        };
        let g = DemoGovernor::new(p.clone());
        g.try_admit("1.1.1.1", T0, rich()).unwrap();
        g.finish(400);
        // A swap is in flight when the process dies.
        g.try_admit("2.2.2.2", T0 + 1, rich()).unwrap();
        let crashed = g.status(T0 + 1);
        assert_eq!(crashed.in_flight, 1);

        // Restart: same persisted snapshot, fresh governor.
        let g2 = DemoGovernor::new(p);
        g2.restore(&crashed);
        let st = g2.status(T0 + 2);
        assert_eq!(st.fee_spent_sats, 400, "spend ceiling survived");
        assert_eq!(
            st.fee_committed_sats,
            DEFAULT_MAX_FEE_PER_SWAP_SATS,
            "crashed reservation remains charged"
        );
        assert_eq!(st.in_flight, 0, "slot not wedged by the crash");
        // And it can immediately admit again rather than being stuck at capacity.
        assert!(g2.try_admit("3.3.3.3", T0 + 3, rich()).is_ok());
    }

    /// Denial messages are shown to the public: they must never carry wallet
    /// names, paths, key material, or amounts held.
    #[test]
    fn denial_messages_leak_nothing_sensitive() {
        let all = [
            DemoDenial::Disabled,
            DemoDenial::TurnstileRequired,
            DemoDenial::TurnstileFailed,
            DemoDenial::GlobalInterval { retry_after_secs: 30 },
            DemoDenial::PerIpQuota,
            DemoDenial::DailyCap,
            DemoDenial::Concurrency,
            DemoDenial::FeeBudgetExhausted,
            DemoDenial::LowFloat { chain: "bitcoin" },
            DemoDenial::FloatUnknown,
        ];
        for d in &all {
            let m = d.message().to_lowercase();
            for bad in [
                "mnemonic", "wif", "preimage", "descriptor", "/secrets", ".rgbmvp", "xprv",
                "tprv", "btc-alice",
            ] {
                assert!(!m.contains(bad), "{:?} leaked {bad}: {m}", d.code());
            }
            assert!(!m.is_empty());
        }
    }

    #[test]
    fn denial_status_codes_and_retry_after() {
        assert_eq!(DemoDenial::TurnstileFailed.status(), 403);
        assert_eq!(DemoDenial::Concurrency.status(), 429);
        assert_eq!(DemoDenial::FloatUnknown.status(), 503);
        assert_eq!(
            DemoDenial::GlobalInterval {
                retry_after_secs: 42
            }
            .retry_after_secs(),
            Some(42)
        );
        assert_eq!(DemoDenial::DailyCap.retry_after_secs(), None);
    }
}
