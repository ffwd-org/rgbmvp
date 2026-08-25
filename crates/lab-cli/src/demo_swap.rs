//! W1 — bounded public demo swap trigger (`POST /v1/demo/swap`).
//!
//! The public may *start* a swap; it may not shape one. Every protocol
//! parameter (amounts, fees, CSV delay, wallet names, RGB wrap) is fixed
//! server-side here. The granular `/v1/swap/{id}/action` endpoint accepts
//! arbitrary amounts and wallets and therefore stays token-gated and is never
//! reachable from this path.
//!
//! Admission and spending limits live in `lab_core::demo` (pure, tested).
//! This module supplies the three things that governor cannot do itself:
//! bot verification, on-chain float observation, and driving the swap.
//!
//! See `docs/TESTNET_PUBLIC_SWAPS.md` (ADR-T1).

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use lab_core::{Config, DemoGovernor, Floats};
use serde_json::{json, Value};

/// How long an observed balance stays usable before a refresh is forced.
const FLOAT_TTL: Duration = Duration::from_secs(120);

/// Ceiling on how long the driver will babysit one swap before giving up.
const DRIVER_MAX_WALL: Duration = Duration::from_secs(60 * 90);

/// Delay between driver attempts while waiting for confirmations.
const DRIVER_POLL: Duration = Duration::from_secs(60);

/// Cloudflare Turnstile server-side verification endpoint.
const TURNSTILE_VERIFY_URL: &str = "https://challenges.cloudflare.com/turnstile/v0/siteverify";

/// Context bound into every public demo token. The browser requests this
/// action and Siteverify must echo it exactly before admission can continue.
pub const TURNSTILE_ACTION: &str = "rgbmvp_demo_swap";
pub const RGB_LAB_TURNSTILE_ACTION: &str = "rgbmvp_rgb_lab";
const TURNSTILE_TOKEN_MAX_BYTES: usize = 2_048;

const TURNSTILE_HOSTNAMES_ENV: &str = "LABD_DEMO_TURNSTILE_HOSTNAMES";

pub fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Wallet names the demo swaps between. Never visitor-supplied.
#[derive(Debug, Clone)]
pub struct DemoWallets {
    pub alice_btc: String,
    pub bob_lq: String,
}

impl DemoWallets {
    pub fn from_env() -> Self {
        Self {
            alice_btc: std::env::var("LABD_DEMO_BTC_WALLET").unwrap_or_else(|_| "btc-alice".into()),
            bob_lq: std::env::var("LABD_DEMO_LQ_WALLET").unwrap_or_else(|_| "bob".into()),
        }
    }
}

/// Per-transaction fee the demo is willing to pay on each chain.
///
/// Defaults match the repo's own long-standing action defaults (fund_btc 800,
/// claim_btc 500), which were chosen from real runs. An earlier T1 draft used
/// ~200 sats derived from vbyte arithmetic; that is ~1.4 sat/vB and risks
/// sitting unconfirmed. Measure on a live run before lowering these.
///
/// The BTC leg spends funding and claim/refund transactions, then the watcher
/// sweeps the controlled exit. Keep `LABD_DEMO_MAX_FEE_SATS` at or above all
/// three fees.
#[derive(Debug, Clone, Copy)]
pub struct DemoFees {
    /// Fee for the BTC funding transaction.
    pub btc_fee_sats: u64,
    /// Fee for a Liquid demo-exit sweep transaction.
    pub lq_sweep_fee_sats: u64,
    /// Fee for the BTC claim/refund transaction.
    pub btc_claim_fee_sats: u64,
    /// Fee retained in the budget for the later BTC demo-exit sweep.
    pub btc_sweep_fee_sats: u64,
    pub lq_fee_sats: u64,
}

impl DemoFees {
    pub fn from_env() -> Self {
        Self {
            btc_fee_sats: env_u64("LABD_DEMO_BTC_FEE_SATS", 800),
            btc_claim_fee_sats: env_u64("LABD_DEMO_BTC_CLAIM_FEE_SATS", 500),
            btc_sweep_fee_sats: env_u64("LABD_DEMO_BTC_SWEEP_FEE_SATS", 500),
            lq_fee_sats: env_u64("LABD_DEMO_LQ_FEE_SATS", 300),
            lq_sweep_fee_sats: env_u64("LABD_DEMO_LQ_SWEEP_FEE_SATS", 400),
        }
    }

    /// Refuse T1 when its admission reservation cannot cover every configured
    /// BTC transaction fee controlled by one swap.
    pub fn validate_reservation(&self, max_fee_per_swap_sats: u64) -> Result<()> {
        let required = u128::from(self.btc_fee_sats)
            + u128::from(self.btc_claim_fee_sats)
            + u128::from(self.btc_sweep_fee_sats);
        anyhow::ensure!(
            required <= u128::from(max_fee_per_swap_sats),
            "configured BTC fees ({required} = {} fund + {} claim/refund + {} sweep) \
             exceed LABD_DEMO_MAX_FEE_SATS ({max_fee_per_swap_sats})",
            self.btc_fee_sats,
            self.btc_claim_fee_sats,
            self.btc_sweep_fee_sats
        );
        Ok(())
    }

    /// Refuse a BTC leg whose individual claim/refund exit cannot be swept
    /// with the configured fee while retaining a standard P2WPKH output.
    pub fn validate_recyclable_btc_exit(&self, leg_sats: u64) -> Result<()> {
        let required = u128::from(self.btc_claim_fee_sats)
            + u128::from(self.btc_sweep_fee_sats)
            + u128::from(lab_btc::DEMO_EXIT_DUST_THRESHOLD_SATS);
        anyhow::ensure!(
            u128::from(leg_sats) > required,
            "LABD_DEMO_LEG_SATS ({leg_sats}) must exceed {required} sats \
             ({} claim/refund fee + {} sweep fee + {} dust) so one BTC exit is recyclable",
            self.btc_claim_fee_sats,
            self.btc_sweep_fee_sats,
            lab_btc::DEMO_EXIT_DUST_THRESHOLD_SATS
        );
        Ok(())
    }
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(default)
}

/// Caches observed wallet balances so a public request cannot trigger a slow
/// Electrum full-scan on every call.
///
/// Deliberately fail-closed: if the balances are missing or stale and a refresh
/// fails, callers receive `None` and the governor denies the swap rather than
/// spending against unknown funds.
#[derive(Debug)]
pub struct FloatCache {
    inner: Mutex<Option<(Floats, Instant)>>,
}

impl Default for FloatCache {
    fn default() -> Self {
        Self::new()
    }
}

impl FloatCache {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(None),
        }
    }

    fn get_fresh(&self) -> Option<Floats> {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        match *guard {
            Some((f, at)) if at.elapsed() < FLOAT_TTL => Some(f),
            _ => None,
        }
    }

    fn store(&self, f: Floats) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some((f, Instant::now()));
    }

    /// Last observed floats regardless of age — for status display only, never
    /// for admission decisions.
    pub fn peek(&self) -> Option<Floats> {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.map(|(f, _)| f)
    }

    /// Return fresh floats, refreshing from chain if the cache has expired.
    ///
    /// **Blocking** (Electrum full-scan + esplora): call inside `spawn_blocking`.
    pub fn observe_blocking(&self, cfg: &Config, wallets: &DemoWallets) -> Option<Floats> {
        if let Some(f) = self.get_fresh() {
            return Some(f);
        }
        match read_floats_blocking(cfg, wallets) {
            Ok(f) => {
                self.store(f);
                Some(f)
            }
            Err(e) => {
                eprintln!("demo: float refresh failed: {e}");
                None
            }
        }
    }
}

/// Read both funding wallet balances from chain.
fn read_floats_blocking(cfg: &Config, wallets: &DemoWallets) -> Result<Floats> {
    let btc_cfg = lab_btc::BtcConfig::from_env();
    btc_cfg
        .ensure_testnet()
        .context("demo swaps are testnet-only")?;
    let btc = lab_btc::balance(cfg, &btc_cfg, &wallets.alice_btc)
        .with_context(|| format!("btc balance for {}", wallets.alice_btc))?;
    let lq = lab_chain::wallet_balance(cfg, &wallets.bob_lq)
        .with_context(|| format!("liquid balance for {}", wallets.bob_lq))?;
    Ok(Floats {
        btc_sats: btc.balance_sats,
        lq_sats: lq.lbtc_sats,
    })
}

/// Outcome of a Turnstile check.
#[derive(Debug, PartialEq, Eq)]
pub enum BotCheck {
    /// Verified, or verification is not required on this deployment.
    Pass,
    /// No token supplied.
    Missing,
    /// Token rejected, or the check could not be completed (fail-closed).
    Failed,
}

/// Read the Turnstile secret from the environment.
///
/// Kept as a function rather than a stored field so the secret never lands in a
/// `Debug`-printable struct.
fn turnstile_secret() -> Option<String> {
    std::env::var("LABD_DEMO_TURNSTILE_SECRET")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn valid_hostname(hostname: &str) -> bool {
    if hostname.is_empty() || hostname.len() > 253 || !hostname.is_ascii() {
        return false;
    }
    hostname.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
            && label
                .as_bytes()
                .last()
                .is_some_and(u8::is_ascii_alphanumeric)
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    })
}

fn parse_turnstile_hostnames(raw: Option<&str>) -> Result<Vec<String>> {
    let raw = raw
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .context("LABD_DEMO_TURNSTILE_HOSTNAMES is unset or empty")?;
    let mut hostnames = Vec::new();
    for item in raw.split(',') {
        let hostname = item.trim().to_ascii_lowercase();
        if !valid_hostname(&hostname) {
            anyhow::bail!("invalid hostname in LABD_DEMO_TURNSTILE_HOSTNAMES");
        }
        if !hostnames.contains(&hostname) {
            hostnames.push(hostname);
        }
    }
    anyhow::ensure!(
        !hostnames.is_empty(),
        "LABD_DEMO_TURNSTILE_HOSTNAMES contains no hostnames"
    );
    Ok(hostnames)
}

fn turnstile_hostnames() -> Result<Vec<String>> {
    let raw = std::env::var(TURNSTILE_HOSTNAMES_ENV).ok();
    parse_turnstile_hostnames(raw.as_deref())
}

/// Refuse to start a protected T1 service unless both the server secret and
/// the exact hostname allowlist are configured.
pub fn validate_turnstile_config() -> Result<()> {
    anyhow::ensure!(
        turnstile_secret().is_some(),
        "LABD_DEMO_TURNSTILE_SECRET is unset or empty"
    );
    turnstile_hostnames()?;
    Ok(())
}

fn turnstile_response_matches(
    v: &Value,
    expected_action: &str,
    allowed_hostnames: &[String],
) -> bool {
    if v.get("success").and_then(Value::as_bool) != Some(true) {
        return false;
    }
    if v.get("action").and_then(Value::as_str) != Some(expected_action) {
        return false;
    }
    let Some(hostname) = v.get("hostname").and_then(Value::as_str) else {
        return false;
    };
    let hostname = hostname.trim().to_ascii_lowercase();
    valid_hostname(&hostname) && allowed_hostnames.iter().any(|allowed| allowed == &hostname)
}

/// Verify a Cloudflare Turnstile token server-side.
///
/// **Blocking**: call inside `spawn_blocking`.
pub fn verify_turnstile_blocking(token: Option<&str>, remote_ip: Option<&str>) -> BotCheck {
    verify_turnstile_action_blocking(token, remote_ip, TURNSTILE_ACTION)
}

/// Verify a token bound to one exact public action. The widget action is
/// client-controlled metadata, so admission must compare Siteverify's echo to
/// the server-selected value rather than trusting the request body.
pub fn verify_turnstile_action_blocking(
    token: Option<&str>,
    remote_ip: Option<&str>,
    expected_action: &str,
) -> BotCheck {
    let hostnames = match turnstile_hostnames() {
        Ok(hostnames) => hostnames,
        Err(e) => {
            eprintln!("demo: turnstile hostname configuration invalid: {e}");
            return BotCheck::Failed;
        }
    };
    verify_turnstile_with_action(
        turnstile_secret().as_deref(),
        token,
        remote_ip,
        expected_action,
        &hostnames,
    )
}

/// Verification with an explicit secret, so callers (and tests) never depend on
/// ambient environment state.
#[cfg(test)]
pub fn verify_turnstile_with(
    secret: Option<&str>,
    token: Option<&str>,
    remote_ip: Option<&str>,
    allowed_hostnames: &[String],
) -> BotCheck {
    verify_turnstile_with_action(
        secret,
        token,
        remote_ip,
        TURNSTILE_ACTION,
        allowed_hostnames,
    )
}

pub fn verify_turnstile_with_action(
    secret: Option<&str>,
    token: Option<&str>,
    remote_ip: Option<&str>,
    expected_action: &str,
    allowed_hostnames: &[String],
) -> BotCheck {
    // Required but unconfigured: refuse rather than silently allow.
    let secret = match secret.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => s,
        None => {
            eprintln!("demo: turnstile required but LABD_DEMO_TURNSTILE_SECRET is unset");
            return BotCheck::Failed;
        }
    };
    let token = match token.map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) => t,
        None => return BotCheck::Missing,
    };
    if token.len() > TURNSTILE_TOKEN_MAX_BYTES {
        return BotCheck::Failed;
    }

    let client = match reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("demo: turnstile client build failed: {e}");
            return BotCheck::Failed;
        }
    };
    let mut form = vec![("secret", secret), ("response", token)];
    if let Some(ip) = remote_ip {
        form.push(("remoteip", ip));
    }
    match client
        .post(TURNSTILE_VERIFY_URL)
        .form(&form)
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
    {
        Ok(resp) => match resp.json::<Value>() {
            Ok(v) if turnstile_response_matches(&v, expected_action, allowed_hostnames) => {
                BotCheck::Pass
            }
            Ok(_) => {
                eprintln!("demo: turnstile response failed success/action/hostname validation");
                BotCheck::Failed
            }
            Err(e) => {
                eprintln!("demo: turnstile response parse failed: {e}");
                BotCheck::Failed
            }
        },
        Err(e) => {
            eprintln!("demo: turnstile request failed: {e}");
            BotCheck::Failed
        }
    }
}

/// Deterministic, collision-resistant-enough demo swap id.
///
/// Uses epoch seconds plus a counter so ids stay sortable and match the
/// `[A-Za-z0-9._~-]` path-id rules enforced by `lab_core::is_safe_path_id`.
pub fn new_demo_swap_id(seq: u64) -> String {
    format!("demo-{}-{}", now_epoch(), seq)
}

/// Create the swap session with fully server-fixed parameters.
///
/// Returns the new swap id. Blocking (writes session state).
pub fn create_demo_session(cfg: &Config, wallets: &DemoWallets, seq: u64) -> Result<String> {
    let policy = lab_core::demo::DemoSwapPolicy::from_env();
    let svc = lab_api::SwapService::new(&cfg.data_dir);
    let id = new_demo_swap_id(seq);
    lab_core::validate_path_id(&id).context("generated demo swap id must be path-safe")?;
    svc.init(
        cfg,
        &id,
        policy.csv_delay,
        &wallets.alice_btc,
        &wallets.bob_lq,
        // No RGB contracts on the public demo: value-only path keeps the
        // footprint (and the dust) minimal.
        None,
        None,
        lab_core::demo::DEMO_RGB_WRAP,
    )
    .context("init demo swap session")?;
    Ok(id)
}

/// One step of the demo swap, with the exact payload the action handler needs.
fn driver_steps(leg_sats: u64, fees: DemoFees) -> Vec<(&'static str, Value)> {
    vec![
        (
            "fund_btc",
            json!({
                "action": "fund_btc",
                "amount_sats": leg_sats,
                "fee_sats": fees.btc_fee_sats,
                "rgb_wrap": false,
            }),
        ),
        (
            "fund_lq",
            json!({
                "action": "fund_lq",
                "amount_sats": leg_sats,
                "fee_sats": fees.lq_fee_sats,
                "rgb_wrap": false,
            }),
        ),
        (
            "claim_lq",
            json!({
                "action": "claim_lq",
                "fee_sats": fees.lq_fee_sats,
                "rgb_wrap": false,
            }),
        ),
        (
            "claim_btc",
            json!({
                "action": "claim_btc",
                "fee_sats": fees.btc_claim_fee_sats,
                "from_witness": true,
                "rgb_wrap": false,
            }),
        ),
    ]
}

/// Drive one demo swap to completion.
///
/// **Blocking and long-running** (waits on testnet confirmations): run on a
/// dedicated blocking task. Returns the BTC fee actually committed so the
/// governor can settle the budget.
///
/// Steps are idempotent in the underlying action handler (it refuses to
/// double-fund), so a retry after a transient failure resumes rather than
/// duplicates.
pub fn drive_demo_swap_blocking(
    cfg: &Config,
    swap_id: &str,
    leg_sats: u64,
    fees: DemoFees,
) -> Result<u64> {
    let store = lab_rgb::swap::SwapStore::new(&cfg.data_dir);
    let started = Instant::now();
    let mut btc_fee_committed = 0u64;

    for (name, payload) in driver_steps(leg_sats, fees) {
        let body = payload.to_string();
        loop {
            if started.elapsed() > DRIVER_MAX_WALL {
                anyhow::bail!("demo swap {swap_id} timed out at step {name}");
            }
            match crate::http_api::handle_swap_action_post(cfg, &store, swap_id, &body) {
                Ok(_) => {
                    match name {
                        "fund_btc" => btc_fee_committed += fees.btc_fee_sats,
                        "claim_btc" => btc_fee_committed += fees.btc_claim_fee_sats,
                        _ => {}
                    }
                    break;
                }
                Err(e) => {
                    // Most failures here are "not confirmed yet"; wait and retry
                    // until the wall-clock ceiling.
                    eprintln!("demo: swap {swap_id} step {name} pending/failed: {e}");
                    std::thread::sleep(DRIVER_POLL);
                }
            }
        }
    }
    Ok(btc_fee_committed)
}

// ---------------------------------------------------------------------------
// W4 — budget persistence
// ---------------------------------------------------------------------------

/// Where the fee-budget counters live. Beside the swap sessions, so a single
/// persistent volume covers both.
#[cfg(test)]
pub fn budget_path(cfg: &Config) -> std::path::PathBuf {
    named_budget_path(cfg, "demo_budget")
}

#[cfg(test)]
fn budget_pending_path(cfg: &Config) -> std::path::PathBuf {
    named_budget_pending_path(cfg, "demo_budget")
}

fn valid_budget_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

fn named_budget_path(cfg: &Config, name: &str) -> std::path::PathBuf {
    assert!(valid_budget_name(name), "invalid internal budget name");
    cfg.data_dir.join(format!("{name}.json"))
}

fn named_budget_pending_path(cfg: &Config, name: &str) -> std::path::PathBuf {
    assert!(valid_budget_name(name), "invalid internal budget name");
    cfg.data_dir.join(format!("{name}.pending.json"))
}

fn budget_io_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn read_budget_file(path: &std::path::Path) -> Result<lab_core::DemoStatus> {
    let bytes =
        std::fs::read(path).with_context(|| format!("read demo budget {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse demo budget {}", path.display()))
}

#[cfg(test)]
fn load_budget_unlocked(cfg: &Config) -> Result<Option<lab_core::DemoStatus>> {
    load_named_budget_unlocked(cfg, "demo_budget")
}

fn load_named_budget_unlocked(cfg: &Config, name: &str) -> Result<Option<lab_core::DemoStatus>> {
    let pending = named_budget_pending_path(cfg, name);
    match std::fs::metadata(&pending) {
        Ok(_) => {
            // A pending record is written and synced before the primary file.
            // Its presence means the prior commit was interrupted, so it is
            // the only safe recovery source. Invalid data is fatal.
            return read_budget_file(&pending).map(Some);
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(e).with_context(|| format!("stat demo budget {}", pending.display()));
        }
    }

    let primary = named_budget_path(cfg, name);
    match std::fs::metadata(&primary) {
        Ok(_) => read_budget_file(&primary).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("stat demo budget {}", primary.display())),
    }
}

/// Load persisted budget counters. Missing state is allowed only for initial
/// creation; unreadable or malformed state is a startup-blocking error.
#[cfg(test)]
pub fn load_budget(cfg: &Config) -> Result<Option<lab_core::DemoStatus>> {
    let _guard = budget_io_lock().lock().unwrap_or_else(|e| e.into_inner());
    load_budget_unlocked(cfg)
}

fn write_budget_file(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
        .with_context(|| format!("open demo budget {}", path.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("write demo budget {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("sync demo budget {}", path.display()))
}

fn save_budget_unlocked(cfg: &Config, st: &lab_core::DemoStatus) -> Result<()> {
    save_named_budget_unlocked(cfg, "demo_budget", st)
}

fn save_named_budget_unlocked(cfg: &Config, name: &str, st: &lab_core::DemoStatus) -> Result<()> {
    let p = named_budget_path(cfg, name);
    let dir = p.parent().context("demo budget path has no parent")?;
    std::fs::create_dir_all(dir).context("create data dir for demo budget")?;
    let pending = named_budget_pending_path(cfg, name);
    let bytes = serde_json::to_vec_pretty(st).context("serialize demo budget")?;

    // Write-ahead protocol: pending is durable before primary is touched. A
    // crash at any point leaves either the old primary, a valid newer pending,
    // or an invalid pending that blocks startup instead of resetting to zero.
    write_budget_file(&pending, &bytes)?;
    write_budget_file(&p, &bytes)?;
    std::fs::remove_file(&pending).context("remove committed demo budget pending record")?;
    #[cfg(unix)]
    std::fs::File::open(dir)
        .context("open demo budget directory for sync")?
        .sync_all()
        .context("sync demo budget directory")?;
    Ok(())
}

/// Persist one explicit snapshot using the serialized write-ahead protocol.
#[cfg(test)]
pub fn save_budget(cfg: &Config, st: &lab_core::DemoStatus) -> Result<()> {
    let _guard = budget_io_lock().lock().unwrap_or_else(|e| e.into_inner());
    save_budget_unlocked(cfg, st)
}

/// Snapshot the governor while holding the I/O serialization lock, so an older
/// snapshot can never overwrite a newer concurrent admission or settlement.
pub fn persist_budget(cfg: &Config, gov: &lab_core::DemoGovernor) -> Result<()> {
    let _guard = budget_io_lock().lock().unwrap_or_else(|e| e.into_inner());
    let st = gov.status(now_epoch());
    save_budget_unlocked(cfg, &st)
}

/// Restore persisted counters into a fresh governor at startup. Recovered
/// reservations become conservative commitments, then the normalized state is
/// durably committed before the public endpoint can accept traffic.
pub fn restore_budget(cfg: &Config, gov: &lab_core::DemoGovernor) -> Result<()> {
    restore_named_budget(cfg, gov, "demo_budget", "T1 demo")
}

/// Persist an independent governor ledger. Names are internal constants only;
/// callers cannot turn this into a path traversal primitive.
pub fn persist_named_budget(cfg: &Config, gov: &lab_core::DemoGovernor, name: &str) -> Result<()> {
    let _guard = budget_io_lock().lock().unwrap_or_else(|e| e.into_inner());
    let st = gov.status(now_epoch());
    save_named_budget_unlocked(cfg, name, &st)
}

pub fn restore_named_budget(
    cfg: &Config,
    gov: &lab_core::DemoGovernor,
    name: &str,
    label: &str,
) -> Result<()> {
    let _guard = budget_io_lock().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(st) = load_named_budget_unlocked(cfg, name)? {
        gov.restore(&st);
        let recovered = gov.status(now_epoch());
        save_named_budget_unlocked(cfg, name, &recovered)?;
        eprintln!(
            "  {label} budget restored: spent={}sats committed={}sats runs_total={} today={}",
            recovered.fee_spent_sats,
            recovered.fee_committed_sats,
            recovered.swaps_total,
            recovered.swaps_today
        );
    } else {
        // Establish the durable zero state before serving the first request.
        save_named_budget_unlocked(cfg, name, &gov.status(now_epoch()))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// W5 — refund / recycle watcher
// ---------------------------------------------------------------------------

/// Default minimum age before a stuck swap is swept.
///
/// The HTLC refund path is consensus-gated on `csv_delay` confirmations, so
/// sweeping earlier just wastes a rejected broadcast. At ~10 min/block on BTC
/// testnet, csv=6 is ~60 min; 90 min leaves margin for slow blocks.
pub const DEFAULT_SWEEP_MIN_AGE_SECS: u64 = 90 * 60;

/// How often the watcher runs.
pub const DEFAULT_SWEEP_INTERVAL_SECS: u64 = 15 * 60;

/// Recover the start time encoded in a demo swap id (`demo-<epoch>-<seq>`).
///
/// `Some` also means "this id was minted by the demo endpoint". The sweeper
/// moves real funds, so it must never touch an operator's own swap session;
/// only ids matching this exact shape are eligible.
pub fn demo_swap_started_at(id: &str) -> Option<u64> {
    let rest = id.strip_prefix("demo-")?;
    let (epoch, seq) = rest.split_once('-')?;
    if epoch.is_empty() || seq.is_empty() {
        return None;
    }
    if !seq.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    epoch.parse::<u64>().ok()
}

/// `(interval_secs, min_age_secs)` for the refund watcher.
pub fn sweep_config_from_env() -> (u64, u64) {
    (
        env_u64("LABD_DEMO_SWEEP_INTERVAL_SECS", DEFAULT_SWEEP_INTERVAL_SECS).max(60),
        env_u64("LABD_DEMO_SWEEP_MIN_AGE_SECS", DEFAULT_SWEEP_MIN_AGE_SECS),
    )
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SweepReport {
    pub scanned: usize,
    pub eligible: usize,
    pub refunded_btc: usize,
    pub refunded_lq: usize,
    pub skipped_young: usize,
    pub errors: usize,
    /// BTC sats recovered from demo exit addresses back into the funding wallet.
    pub recycled_sats: u64,
    /// L-BTC sats recovered from the Liquid demo exit addresses.
    pub recycled_lq_sats: u64,
}

/// Sweep the BTC demo exit addresses back into the funding wallet.
///
/// Runs after the refund pass: refunds land at `alice-refund` and completed
/// swaps at `bob-claimer`, neither of which is the funding wallet. Skips
/// silently when there is nothing to recover.
fn recycle_lq_exits_blocking(cfg: &Config, wallets: &DemoWallets, fee_sats: u64) -> u64 {
    match lab_chain::sweep_all_demo_exits_lq(cfg, &wallets.bob_lq, fee_sats) {
        Ok(results) => {
            let mut total = 0;
            for r in results {
                if let Some(txid) = &r.txid {
                    total += r.swept_sats;
                    eprintln!(
                        "demo recycle: swept {} L-BTC sats from {} -> {} ({txid})",
                        r.swept_sats, r.label, wallets.bob_lq
                    );
                }
            }
            total
        }
        Err(e) => {
            eprintln!("demo recycle: liquid sweep failed: {e:#}");
            0
        }
    }
}

fn recycle_btc_exits_blocking(cfg: &Config, wallets: &DemoWallets, fee_sats: u64) -> u64 {
    let btc = lab_btc::BtcConfig::from_env();
    if btc.ensure_testnet().is_err() {
        return 0;
    }
    match lab_btc::sweep_all_demo_exits(cfg, &btc, &wallets.alice_btc, fee_sats) {
        Ok(results) => {
            let mut total = 0;
            for r in results {
                if let Some(txid) = &r.txid {
                    total += r.swept_sats;
                    eprintln!(
                        "demo recycle: swept {} sats from {} -> {} ({txid})",
                        r.swept_sats, r.label, wallets.alice_btc
                    );
                }
            }
            total
        }
        Err(e) => {
            eprintln!("demo recycle: sweep failed: {e:#}");
            0
        }
    }
}

/// True when a session still has value parked in an HTLC.
fn needs_btc_refund(s: &lab_rgb::swap::SwapSession) -> bool {
    s.btc_fund_txid.is_some() && s.btc_claim_txid.is_none() && s.btc_refund_txid.is_none()
}

fn needs_lq_refund(s: &lab_rgb::swap::SwapSession) -> bool {
    s.lq_fund_txid.is_some() && s.lq_claim_txid.is_none() && s.lq_refund_txid.is_none()
}

/// Refund stuck demo swaps, then sweep the recovered value back to the funder.
///
/// IMPORTANT: an HTLC refund does **not** pay the funding wallet. Both refund
/// and claim paths pay one of four P2WPKH addresses derived from the secret
/// demo-exit seed and a public role label. Without the sweep below, `btc-alice`
/// drains on every swap regardless of outcome and the value strands there.
///
/// **Blocking and network-bound**: run on a blocking task. Failures are counted
/// and retried on the next sweep — a refund rejected because the CSV window has
/// not elapsed is expected, not an error condition.
pub fn sweep_stuck_demo_swaps_blocking(
    cfg: &Config,
    wallets: &DemoWallets,
    fees: DemoFees,
    min_age_secs: u64,
) -> SweepReport {
    let mut report = SweepReport::default();
    let ids = match crate::http_api::list_swap_ids(&cfg.data_dir) {
        Ok(ids) => ids,
        Err(e) => {
            eprintln!("demo sweep: cannot list swaps: {e:#}");
            report.errors += 1;
            return report;
        }
    };
    let store = lab_rgb::swap::SwapStore::new(&cfg.data_dir);
    let now = now_epoch();

    for id in ids {
        // Never touch operator sessions — only ids this module minted.
        let started = match demo_swap_started_at(&id) {
            Some(t) => t,
            None => continue,
        };
        report.scanned += 1;

        if now.saturating_sub(started) < min_age_secs {
            report.skipped_young += 1;
            continue;
        }
        let mut s = match store.load(&id) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("demo sweep: load {id}: {e:#}");
                report.errors += 1;
                continue;
            }
        };
        if lab_rgb::swap::hydrate_legacy_refund_txids(&mut s) {
            if let Err(e) = store.save(&s) {
                eprintln!("demo sweep: save upgraded refund state {id}: {e:#}");
                report.errors += 1;
                continue;
            }
        }
        if matches!(
            s.phase,
            lab_rgb::swap::SwapPhase::Done | lab_rgb::swap::SwapPhase::Refunded
        ) {
            continue;
        }

        let btc = needs_btc_refund(&s);
        let lq = needs_lq_refund(&s);
        if !btc && !lq {
            continue;
        }
        report.eligible += 1;

        if lq {
            let body = json!({"action": "refund_lq", "fee_sats": fees.lq_fee_sats}).to_string();
            match crate::http_api::handle_swap_action_post(cfg, &store, &id, &body) {
                Ok(_) => {
                    report.refunded_lq += 1;
                    eprintln!("demo sweep: refunded liquid leg of {id}");
                }
                Err(e) => {
                    // Usually "CSV not elapsed" — retried next sweep.
                    eprintln!("demo sweep: refund_lq {id} pending/failed: {e}");
                    report.errors += 1;
                }
            }
        }
        if btc {
            let body =
                json!({"action": "refund_btc", "fee_sats": fees.btc_claim_fee_sats}).to_string();
            match crate::http_api::handle_swap_action_post(cfg, &store, &id, &body) {
                Ok(_) => {
                    report.refunded_btc += 1;
                    eprintln!("demo sweep: refunded bitcoin leg of {id}");
                }
                Err(e) => {
                    eprintln!("demo sweep: refund_btc {id} pending/failed: {e}");
                    report.errors += 1;
                }
            }
        }
    }

    // Recover value from the demo exit addresses back into the funding wallet.
    // Runs every sweep, not only when a refund fired: completed swaps also pay
    // out to `bob-claimer` and would otherwise strand there.
    report.recycled_sats = recycle_btc_exits_blocking(cfg, wallets, fees.btc_sweep_fee_sats);
    report.recycled_lq_sats = recycle_lq_exits_blocking(cfg, wallets, fees.lq_sweep_fee_sats);
    report
}

/// Cloudflare Turnstile **sitekey**. Public by design (it ships in the HTML),
/// unlike the secret, which never leaves the server.
pub fn turnstile_sitekey() -> Option<String> {
    std::env::var("LABD_DEMO_TURNSTILE_SITEKEY")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Public JSON for `GET /v1/demo/quota`.
pub fn quota_json(gov: &DemoGovernor, floats: Option<Floats>) -> Value {
    let p = gov.policy();
    let st = gov.status(now_epoch());
    json!({
        "enabled": p.enabled,
        "network": "testnet",
        "leg_sats": p.leg_sats,
        "rgb_wrap": lab_core::demo::DEMO_RGB_WRAP,
        "csv_delay": p.csv_delay,
        "turnstile_required": p.turnstile_required,
        // Absent until a Cloudflare sitekey is provisioned; the page renders a
        // "bot check unavailable" state rather than a broken widget.
        "turnstile_sitekey": turnstile_sitekey(),
        "limits": {
            "daily_cap": p.daily_cap,
            "max_concurrent": p.max_concurrent,
            "min_interval_secs": p.global_min_interval_secs,
            "per_ip_hourly": p.per_ip_hourly,
            "per_ip_daily": p.per_ip_daily,
        },
        "budget": {
            "fee_budget_sats": p.fee_budget_sats,
            "fee_spent_sats": st.fee_spent_sats,
            "fee_reserved_sats": st.fee_reserved_sats,
            "fee_committed_sats": st.fee_committed_sats,
            "fee_accounted_sats": st.fee_spent_sats
                .saturating_add(st.fee_reserved_sats)
                .saturating_add(st.fee_committed_sats),
            "swaps_remaining_est": gov.swaps_remaining_in_budget(),
        },
        "usage": {
            "in_flight": st.in_flight,
            "swaps_today": st.swaps_today,
            "swaps_total": st.swaps_total,
        },
        "floats": floats.map(|f| json!({
            "btc_sats": f.btc_sats,
            "lq_sats": f.lq_sats,
            "btc_floor_sats": p.btc_floor_sats,
            "lq_floor_sats": p.lq_floor_sats,
        })),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_fees() -> DemoFees {
        DemoFees {
            btc_fee_sats: 800,
            btc_claim_fee_sats: 500,
            btc_sweep_fee_sats: 500,
            lq_fee_sats: 300,
            lq_sweep_fee_sats: 400,
        }
    }

    #[test]
    fn fee_reservation_validation_fails_closed() {
        let fees = test_fees();
        assert!(fees.validate_reservation(1_800).is_ok());
        assert!(fees.validate_reservation(1_799).is_err());

        let overflowing = DemoFees {
            btc_fee_sats: u64::MAX,
            btc_claim_fee_sats: 1,
            ..fees
        };
        assert!(overflowing.validate_reservation(u64::MAX).is_err());
    }

    #[test]
    fn btc_leg_must_be_individually_recyclable() {
        let fees = test_fees();
        assert!(fees.validate_recyclable_btc_exit(1_300).is_ok());
        assert!(fees.validate_recyclable_btc_exit(1_295).is_ok());
        assert!(fees.validate_recyclable_btc_exit(1_294).is_err());
        assert!(fees.validate_recyclable_btc_exit(1_000).is_err());
    }

    #[test]
    fn demo_swap_ids_are_path_safe() {
        for seq in [0u64, 1, 42, 99_999] {
            let id = new_demo_swap_id(seq);
            assert!(
                lab_core::is_safe_path_id(&id),
                "generated id must satisfy the path-id rules: {id}"
            );
        }
    }

    #[test]
    fn watcher_retries_only_the_unresolved_leg_after_partial_refund() {
        let mut s = lab_rgb::swap::init_swap(
            &lab_rgb::htlc::DemoKeyring::new([0x42; 32]).unwrap(),
            "demo-partial-refund",
            6,
            "btc-alice",
            "bob",
            None,
            None,
            false,
        )
        .unwrap();
        s.btc_fund_txid = Some("btc-fund".into());
        s.lq_fund_txid = Some("lq-fund".into());
        s.notes.push("lq_refund_txid=lq-refund".into());
        s.phase = lab_rgb::swap::SwapPhase::Refunded;

        assert!(lab_rgb::swap::hydrate_legacy_refund_txids(&mut s));

        assert!(needs_btc_refund(&s));
        assert!(!needs_lq_refund(&s));
        assert_eq!(s.phase, lab_rgb::swap::SwapPhase::Refunding);
    }

    /// The driver must never emit an RGB-wrapped or oversized leg.
    #[test]
    fn driver_steps_are_value_only_and_fixed() {
        let steps = driver_steps(1_000, test_fees());
        assert_eq!(steps.len(), 4);
        for (name, payload) in &steps {
            assert_eq!(
                payload.get("rgb_wrap").and_then(|v| v.as_bool()),
                Some(false),
                "step {name} must stay on the value-only path"
            );
            assert!(
                payload.get("action").and_then(|v| v.as_str()).is_some(),
                "step {name} must carry an action"
            );
        }
        assert_eq!(
            steps[0].1.get("amount_sats").and_then(|v| v.as_u64()),
            Some(1_000)
        );
    }

    #[test]
    fn driver_steps_follow_htlc_order() {
        let steps = driver_steps(1_000, test_fees());
        let names: Vec<&str> = steps.iter().map(|(n, _)| *n).collect();
        // Alice must claim Liquid (revealing the preimage) before Bob claims BTC.
        assert_eq!(names, vec!["fund_btc", "fund_lq", "claim_lq", "claim_btc"]);
    }

    /// Missing token is distinguishable from a rejected one.
    #[test]
    fn turnstile_missing_token_is_reported_as_missing() {
        let secret = Some("test-secret");
        let hostnames = vec!["demo.example".to_string()];
        assert_eq!(
            verify_turnstile_with(secret, None, None, &hostnames),
            BotCheck::Missing
        );
        assert_eq!(
            verify_turnstile_with(secret, Some("  "), None, &hostnames),
            BotCheck::Missing
        );
    }

    /// Fail closed when the verification secret is not configured: an
    /// unconfigured server must never wave traffic through.
    #[test]
    fn turnstile_without_secret_fails_closed() {
        let hostnames = vec!["demo.example".to_string()];
        assert_eq!(
            verify_turnstile_with(None, Some("some-token"), None, &hostnames),
            BotCheck::Failed
        );
        assert_eq!(
            verify_turnstile_with(Some("   "), Some("some-token"), None, &hostnames),
            BotCheck::Failed
        );
        // Fails closed even when no token is supplied either.
        assert_eq!(
            verify_turnstile_with(None, None, None, &hostnames),
            BotCheck::Failed
        );
    }

    #[test]
    fn turnstile_response_requires_exact_action_and_allowed_hostname() {
        let allowed = vec!["rgbmvp-demo.example".to_string()];
        let valid = json!({
            "success": true,
            "action": TURNSTILE_ACTION,
            "hostname": "rgbmvp-demo.example",
        });
        assert!(turnstile_response_matches(
            &valid,
            TURNSTILE_ACTION,
            &allowed
        ));
        assert!(!turnstile_response_matches(
            &valid,
            RGB_LAB_TURNSTILE_ACTION,
            &allowed
        ));

        let rgb_valid = json!({
            "success": true,
            "action": RGB_LAB_TURNSTILE_ACTION,
            "hostname": "rgbmvp-demo.example",
        });
        assert!(turnstile_response_matches(
            &rgb_valid,
            RGB_LAB_TURNSTILE_ACTION,
            &allowed
        ));

        for invalid in [
            json!({"success": false, "action": TURNSTILE_ACTION, "hostname": "rgbmvp-demo.example"}),
            json!({"success": true, "hostname": "rgbmvp-demo.example"}),
            json!({"success": true, "action": "other_action", "hostname": "rgbmvp-demo.example"}),
            json!({"success": true, "action": TURNSTILE_ACTION}),
            json!({"success": true, "action": TURNSTILE_ACTION, "hostname": "attacker.example"}),
        ] {
            assert!(
                !turnstile_response_matches(&invalid, TURNSTILE_ACTION, &allowed),
                "{invalid}"
            );
        }
    }

    #[test]
    fn turnstile_rejects_oversized_token_before_network() {
        let token = "x".repeat(TURNSTILE_TOKEN_MAX_BYTES + 1);
        assert_eq!(
            verify_turnstile_with_action(
                Some("test-secret"),
                Some(&token),
                None,
                RGB_LAB_TURNSTILE_ACTION,
                &["demo.example".to_string()],
            ),
            BotCheck::Failed
        );
    }

    #[test]
    fn turnstile_hostname_config_rejects_wildcards_urls_ports_and_empty_values() {
        assert!(parse_turnstile_hostnames(None).is_err());
        assert!(parse_turnstile_hostnames(Some(" ")).is_err());
        for invalid in [
            "*.example.com",
            "https://example.com",
            "example.com:443",
            "example.com/path",
            "example..com",
            "-demo.example",
            "demo-.example",
        ] {
            assert!(
                parse_turnstile_hostnames(Some(invalid)).is_err(),
                "{invalid}"
            );
        }
        assert_eq!(
            parse_turnstile_hostnames(Some(" Demo.Example,other.example,demo.example ")).unwrap(),
            vec!["demo.example", "other.example"]
        );
    }

    #[test]
    fn float_cache_is_empty_until_observed() {
        let c = FloatCache::new();
        assert!(c.get_fresh().is_none());
        assert!(c.peek().is_none());
        c.store(Floats {
            btc_sats: 33_607,
            lq_sats: 146_633,
        });
        assert_eq!(c.get_fresh().unwrap().btc_sats, 33_607);
        assert_eq!(c.peek().unwrap().lq_sats, 146_633);
    }

    /// The sweeper refunds real funds, so its id filter is safety-critical:
    /// it must recognise only ids this module minted.
    /// Mirrors the sweeper's eligibility filter.
    fn sweepable(id: &str) -> bool {
        demo_swap_started_at(id).is_some()
    }

    #[test]
    fn sweeper_only_recognises_generated_demo_ids() {
        // Ours.
        assert_eq!(demo_swap_started_at("demo-1786400000-0"), Some(1786400000));
        assert_eq!(demo_swap_started_at("demo-1786400000-42"), Some(1786400000));
        assert!(sweepable("demo-1786400000-7"));

        // Pre-existing operator sessions in .rgbmvp/swaps must be ignored.
        for foreign in [
            "demo-u2",
            "demo-swap-1",
            "s3-demo",
            "p1-live",
            "u2-smoke",
            "s3-browser-20260724-0112",
            "s3-20260722-1251",
            "demo-",
            "demo-abc-1",
            "demo-123-",
            "demo-123-x",
        ] {
            assert!(
                !sweepable(foreign),
                "{foreign} must NOT be swept — it is not a generated demo id"
            );
        }
    }

    /// Every id the generator produces must be recognised by the sweeper,
    /// or stuck swaps would silently never be refunded.
    #[test]
    fn generated_ids_round_trip_through_the_sweeper_filter() {
        for seq in [0u64, 1, 9, 10, 12_345] {
            let id = new_demo_swap_id(seq);
            assert!(sweepable(&id), "generator/sweeper mismatch on {id}");
            assert!(lab_core::is_safe_path_id(&id));
        }
    }

    #[test]
    fn refund_eligibility_matches_htlc_state() {
        use lab_rgb::swap::init_swap;
        let keyring = lab_rgb::htlc::DemoKeyring::new([0x42; 32]).unwrap();
        let mut s = init_swap(
            &keyring,
            "demo-1-0",
            6,
            "btc-alice",
            "bob",
            None,
            None,
            false,
        )
        .unwrap();
        // Nothing funded yet: nothing to recover.
        assert!(!needs_btc_refund(&s));
        assert!(!needs_lq_refund(&s));

        s.btc_fund_txid = Some("aa".into());
        s.lq_fund_txid = Some("bb".into());
        assert!(needs_btc_refund(&s), "funded and unclaimed => refundable");
        assert!(needs_lq_refund(&s));

        // Once claimed, the value already moved; refunding would be wrong.
        s.btc_claim_txid = Some("cc".into());
        s.lq_claim_txid = Some("dd".into());
        assert!(!needs_btc_refund(&s));
        assert!(!needs_lq_refund(&s));
    }

    /// Budget must survive a restart, or a 2-week run silently overspends.
    #[test]
    fn budget_persists_across_restart() {
        let dir = std::env::temp_dir().join(format!("rgbmvp-demo-budget-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut cfg = Config::load().expect("config");
        cfg.data_dir = dir.clone();

        // Nothing persisted yet.
        assert!(load_budget(&cfg).unwrap().is_none());

        let gov = lab_core::DemoGovernor::new(lab_core::DemoSwapPolicy {
            enabled: true,
            ..Default::default()
        });
        gov.try_admit(
            "1.1.1.1",
            now_epoch(),
            Some(lab_core::Floats {
                btc_sats: 33_607,
                lq_sats: 146_633,
            }),
        )
        .expect("admit");
        gov.finish(400);
        persist_budget(&cfg, &gov).unwrap();

        // A fresh governor (simulating a restart) recovers the spend.
        let reloaded = load_budget(&cfg).unwrap().expect("budget file written");
        assert_eq!(reloaded.fee_spent_sats, 400);
        assert_eq!(reloaded.swaps_total, 1);

        let gov2 = lab_core::DemoGovernor::new(lab_core::DemoSwapPolicy {
            enabled: true,
            ..Default::default()
        });
        restore_budget(&cfg, &gov2).unwrap();
        let st = gov2.status(now_epoch());
        assert_eq!(st.fee_spent_sats, 400, "spend ceiling survived the restart");
        assert_eq!(st.in_flight, 0, "in-flight never survives a restart");
        // Derived from the constants so this cannot drift when fees change.
        let per_swap = lab_core::demo::DEFAULT_MAX_FEE_PER_SWAP_SATS;
        let budget = lab_core::demo::DEFAULT_FEE_BUDGET_SATS;
        assert_eq!(gov2.swaps_remaining_in_budget(), (budget - 400) / per_swap);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn named_budget_is_independent_from_t1_budget() {
        let dir =
            std::env::temp_dir().join(format!("rgbmvp-named-demo-budget-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut cfg = Config::load().expect("config");
        cfg.data_dir = dir.clone();
        let policy = lab_core::DemoSwapPolicy {
            enabled: true,
            ..Default::default()
        };
        let gov = lab_core::DemoGovernor::new(policy.clone());
        gov.try_admit(
            "1.1.1.1",
            now_epoch(),
            Some(lab_core::Floats {
                btc_sats: u64::MAX,
                lq_sats: u64::MAX,
            }),
        )
        .unwrap();
        gov.finish(321);
        persist_named_budget(&cfg, &gov, "rgb_demo_budget").unwrap();

        assert!(!budget_path(&cfg).exists());
        assert!(named_budget_path(&cfg, "rgb_demo_budget").exists());
        let restored = lab_core::DemoGovernor::new(policy);
        restore_named_budget(&cfg, &restored, "rgb_demo_budget", "RGB demo").unwrap();
        assert_eq!(restored.status(now_epoch()).fee_spent_sats, 321);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Corrupt primary state blocks startup instead of resetting the ceiling.
    #[test]
    fn corrupt_budget_file_fails_closed() {
        let dir = std::env::temp_dir().join(format!("rgbmvp-demo-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut cfg = Config::load().expect("config");
        cfg.data_dir = dir.clone();
        std::fs::write(budget_path(&cfg), b"{ this is not json").unwrap();
        assert!(load_budget(&cfg).is_err());
        let gov = lab_core::DemoGovernor::new(lab_core::DemoSwapPolicy {
            enabled: true,
            ..Default::default()
        });
        assert!(restore_budget(&cfg, &gov).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unavailable_budget_storage_is_an_error() {
        let root = std::env::temp_dir().join(format!(
            "rgbmvp-demo-budget-unavailable-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let not_a_directory = root.join("data");
        std::fs::write(&not_a_directory, b"file").unwrap();
        let mut cfg = Config::load().expect("config");
        cfg.data_dir = not_a_directory;
        let gov = lab_core::DemoGovernor::new(lab_core::DemoSwapPolicy {
            enabled: true,
            ..Default::default()
        });
        assert!(persist_budget(&cfg, &gov).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A synced pending record is the safe recovery point if the process dies
    /// before the primary write completes.
    #[test]
    fn pending_budget_record_wins_after_interrupted_commit() {
        let dir = std::env::temp_dir().join(format!("rgbmvp-demo-pending-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut cfg = Config::load().expect("config");
        cfg.data_dir = dir.clone();

        let old = lab_core::DemoStatus {
            fee_spent_sats: 100,
            ..Default::default()
        };
        save_budget(&cfg, &old).unwrap();
        let pending = lab_core::DemoStatus {
            fee_spent_sats: 500,
            ..Default::default()
        };
        std::fs::write(
            budget_pending_path(&cfg),
            serde_json::to_vec_pretty(&pending).unwrap(),
        )
        .unwrap();

        assert_eq!(load_budget(&cfg).unwrap().unwrap().fee_spent_sats, 500);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_pending_record_blocks_fallback_to_primary() {
        let dir =
            std::env::temp_dir().join(format!("rgbmvp-demo-pending-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut cfg = Config::load().expect("config");
        cfg.data_dir = dir.clone();
        save_budget(&cfg, &lab_core::DemoStatus::default()).unwrap();
        std::fs::write(budget_pending_path(&cfg), b"truncated").unwrap();
        assert!(load_budget(&cfg).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The exact reported defect: a crash after durable admission must consume
    /// a full reservation after restart even if completion never persisted.
    #[test]
    fn admitted_reservation_is_crash_durable() {
        let dir =
            std::env::temp_dir().join(format!("rgbmvp-demo-admit-crash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut cfg = Config::load().expect("config");
        cfg.data_dir = dir.clone();
        let policy = lab_core::DemoSwapPolicy {
            enabled: true,
            ..Default::default()
        };
        let gov = lab_core::DemoGovernor::new(policy.clone());
        gov.try_admit(
            "1.1.1.1",
            now_epoch(),
            Some(lab_core::Floats {
                btc_sats: 33_607,
                lq_sats: 146_633,
            }),
        )
        .unwrap();
        persist_budget(&cfg, &gov).unwrap();

        let restarted = lab_core::DemoGovernor::new(policy);
        restore_budget(&cfg, &restarted).unwrap();
        let st = restarted.status(now_epoch());
        assert_eq!(st.in_flight, 0);
        assert_eq!(st.fee_reserved_sats, 0);
        assert_eq!(
            st.fee_committed_sats,
            lab_core::demo::DEFAULT_MAX_FEE_PER_SWAP_SATS
        );
        let expected = (lab_core::demo::DEFAULT_FEE_BUDGET_SATS
            - lab_core::demo::DEFAULT_MAX_FEE_PER_SWAP_SATS)
            / lab_core::demo::DEFAULT_MAX_FEE_PER_SWAP_SATS;
        assert_eq!(restarted.swaps_remaining_in_budget(), expected);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// W7: these fields are what the ops alerts query. Renaming or dropping one
    /// silently blinds monitoring, so the contract is pinned here.
    #[test]
    fn quota_json_exposes_the_fields_ops_alerts_depend_on() {
        let gov = lab_core::DemoGovernor::new(lab_core::DemoSwapPolicy {
            enabled: true,
            ..Default::default()
        });
        let v = quota_json(
            &gov,
            Some(lab_core::Floats {
                btc_sats: 33_607,
                lq_sats: 146_633,
            }),
        );
        for path in [
            ("budget", "fee_spent_sats"),
            ("budget", "fee_reserved_sats"),
            ("budget", "fee_committed_sats"),
            ("budget", "fee_accounted_sats"),
            ("budget", "fee_budget_sats"),
            ("budget", "swaps_remaining_est"),
            ("usage", "in_flight"),
            ("usage", "swaps_today"),
            ("usage", "swaps_total"),
            ("floats", "btc_sats"),
            ("floats", "lq_sats"),
            ("floats", "btc_floor_sats"),
            ("floats", "lq_floor_sats"),
            ("limits", "daily_cap"),
            ("limits", "max_concurrent"),
        ] {
            assert!(
                v.get(path.0).and_then(|o| o.get(path.1)).is_some(),
                "ops alert field {}.{} is missing from /v1/demo/quota",
                path.0,
                path.1
            );
        }
    }

    /// Floats must be reported as null (not zero) when unknown, so a monitoring
    /// gap is never mistaken for an empty wallet.
    #[test]
    fn unknown_floats_report_null_not_zero() {
        let gov = lab_core::DemoGovernor::new(lab_core::DemoSwapPolicy {
            enabled: true,
            ..Default::default()
        });
        let v = quota_json(&gov, None);
        assert!(v["floats"].is_null(), "unknown floats must be null, not 0");
    }

    #[test]
    fn quota_json_reports_budget_and_flags() {
        let gov = DemoGovernor::new(lab_core::demo::DemoSwapPolicy {
            enabled: true,
            ..Default::default()
        });
        let v = quota_json(
            &gov,
            Some(Floats {
                btc_sats: 33_607,
                lq_sats: 146_633,
            }),
        );
        assert_eq!(v["enabled"], json!(true));
        assert_eq!(v["rgb_wrap"], json!(false));
        assert_eq!(v["leg_sats"], json!(1_300));
        let expected =
            lab_core::demo::DEFAULT_FEE_BUDGET_SATS / lab_core::demo::DEFAULT_MAX_FEE_PER_SWAP_SATS;
        assert_eq!(v["budget"]["swaps_remaining_est"], json!(expected));
        // Funding, claim/refund, and sweep consume 1,800 sats of accounting
        // capacity per admission, so the 28,000-sat run admits at most 15.
        assert_eq!(expected, 15);
        assert_eq!(v["floats"]["btc_sats"], json!(33_607));
    }
}
