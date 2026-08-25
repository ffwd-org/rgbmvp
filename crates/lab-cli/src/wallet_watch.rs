//! Read-only Liquid wallet synchronization for the public `/demo` board.
//!
//! A watch descriptor can contain SLIP77 blinding material, which reveals the
//! amounts of controlled confidential outputs, but it must never contain a
//! spending private key. This module is deliberately separate from custody and
//! signing resolution: it only constructs LWK wollets through lab-chain's
//! read-only descriptor APIs.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use lab_core::Config;
use serde::{Deserialize, Serialize};

pub const WATCH_BUNDLE_ENV: &str = "RGBMVP_LIQUID_WATCH_BUNDLE";
const DEFAULT_TTL_SECS: u64 = 120;
const DEFAULT_MAX_STALE_SECS: u64 = 900;
const MAX_BUNDLE_BYTES: u64 = 64 * 1024;
const MAX_DESCRIPTOR_BYTES: usize = 4096;
const WATCH_NAMES: [&str; 5] = ["alice", "bob", "carol", "lab0", "maker"];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WatchBundleFile {
    version: u32,
    network: String,
    wallets: Vec<WatchBundleEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WatchBundleEntry {
    name: String,
    descriptor: String,
}

#[derive(Deserialize)]
struct AddressRegistry {
    wallets: Vec<AddressRegistryEntry>,
}

#[derive(Deserialize)]
struct AddressRegistryEntry {
    name: String,
    address_0: String,
}

#[derive(Clone)]
struct WatchWallet {
    name: String,
    descriptor: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct WalletBalanceView {
    pub lbtc_sats: Option<u64>,
    pub balance_source: &'static str,
    pub balance_scope: &'static str,
    pub balance_status: &'static str,
    pub balance_as_of_epoch: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct WalletBoardSnapshot {
    pub wallets: BTreeMap<String, WalletBalanceView>,
    pub source: &'static str,
    pub refresh_secs: u64,
    pub max_stale_secs: u64,
}

impl WalletBoardSnapshot {
    pub fn wallet(&self, name: &str) -> WalletBalanceView {
        self.wallets
            .get(name)
            .cloned()
            .unwrap_or(WalletBalanceView {
                lbtc_sats: None,
                balance_source: "address-registry",
                balance_scope: "whole-wallet",
                balance_status: "unavailable",
                balance_as_of_epoch: None,
            })
    }
}

#[derive(Clone, Copy)]
struct Observation {
    lbtc_sats: u64,
    observed_at: Instant,
    observed_epoch: u64,
}

#[derive(Default)]
struct CacheState {
    observations: BTreeMap<String, Observation>,
    failed_last_refresh: BTreeSet<String>,
    last_attempt: Option<Instant>,
    refreshing: bool,
}

/// Shared, single-flight cache for the public wallet board.
///
/// This cache is display-only. T1 admission uses its separate fail-closed
/// `FloatCache` and never consumes stale values from here.
pub struct WalletBalanceBoard {
    wallets: Vec<WatchWallet>,
    source: &'static str,
    ttl: Duration,
    max_stale: Duration,
    state: Mutex<CacheState>,
}

impl WalletBalanceBoard {
    /// Load a strict Secret Manager watch bundle when configured. Local
    /// development instead reuses descriptor files already under the wallet
    /// directory; an addresses-only deployment remains a safe fallback.
    pub fn from_config(cfg: &Config) -> Result<Self> {
        let ttl_secs = env_secs("LABD_WALLET_BALANCE_TTL_SECS", DEFAULT_TTL_SECS, 30, 600);
        let max_stale_secs = env_secs(
            "LABD_WALLET_BALANCE_MAX_STALE_SECS",
            DEFAULT_MAX_STALE_SECS,
            ttl_secs,
            3600,
        );
        let registry = load_registry(cfg)?;

        let (wallets, source) = match std::env::var(WATCH_BUNDLE_ENV)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
        {
            Some(path) => {
                let registry = registry.ok_or_else(|| {
                    anyhow::anyhow!(
                        "wallet watch bundle refused: public address registry is missing"
                    )
                })?;
                (
                    load_bundle(Path::new(&path), cfg, &registry)?,
                    "secret-bundle",
                )
            }
            None => (
                load_local_descriptors(cfg, registry.as_ref())?,
                "local-descriptors",
            ),
        };

        Ok(Self {
            wallets,
            source,
            ttl: Duration::from_secs(ttl_secs),
            max_stale: Duration::from_secs(max_stale_secs),
            state: Mutex::new(CacheState::default()),
        })
    }

    #[cfg(test)]
    pub fn empty() -> Self {
        Self {
            wallets: Vec::new(),
            source: "address-registry",
            ttl: Duration::from_secs(DEFAULT_TTL_SECS),
            max_stale: Duration::from_secs(DEFAULT_MAX_STALE_SECS),
            state: Mutex::new(CacheState::default()),
        }
    }

    pub fn configured_wallets(&self) -> usize {
        self.wallets.len()
    }

    pub fn source(&self) -> &'static str {
        if self.wallets.is_empty() {
            "address-registry"
        } else {
            self.source
        }
    }

    pub fn refresh_secs(&self) -> u64 {
        self.ttl.as_secs()
    }

    pub fn snapshot_blocking(&self, cfg: &Config) -> WalletBoardSnapshot {
        self.snapshot_with(|wallet| {
            lab_chain::wallet_balance_from_descriptor(cfg, &wallet.name, &wallet.descriptor)
                .map(|balance| balance.lbtc_sats)
        })
    }

    fn snapshot_with<F>(&self, scan: F) -> WalletBoardSnapshot
    where
        F: Fn(&WatchWallet) -> Result<u64> + Sync,
    {
        let should_refresh = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let due = state
                .last_attempt
                .map(|at| at.elapsed() >= self.ttl)
                .unwrap_or(true);
            if self.wallets.is_empty() || state.refreshing || !due {
                false
            } else {
                state.refreshing = true;
                true
            }
        };

        if should_refresh {
            // A whole-wallet LWK scan can take tens of seconds. The bundle is
            // deliberately fixed to five wallets, so scan them concurrently
            // while the board-level `refreshing` flag preserves single-flight
            // behavior across requests.
            let results = std::thread::scope(|scope| {
                let handles: Vec<_> = self
                    .wallets
                    .iter()
                    .map(|wallet| {
                        let scan = &scan;
                        (wallet.name.clone(), scope.spawn(move || scan(wallet)))
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|(name, handle)| {
                        let result = handle.join().unwrap_or_else(|_| {
                            Err(anyhow::anyhow!("wallet scan worker panicked"))
                        });
                        (name, result)
                    })
                    .collect::<Vec<_>>()
            });
            let observed_at = Instant::now();
            let observed_epoch = now_epoch();
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.failed_last_refresh.clear();
            for (name, result) in results {
                match result {
                    Ok(lbtc_sats) => {
                        state.observations.insert(
                            name,
                            Observation {
                                lbtc_sats,
                                observed_at,
                                observed_epoch,
                            },
                        );
                    }
                    Err(_) => {
                        // Never log the underlying error: parser errors may
                        // echo descriptor text. The wallet name is sufficient
                        // for an operator to reproduce the read-only scan.
                        eprintln!("demo wallet balance refresh failed for {name}");
                        state.failed_last_refresh.insert(name);
                    }
                }
            }
            state.last_attempt = Some(observed_at);
            state.refreshing = false;
        }

        self.render_snapshot()
    }

    fn render_snapshot(&self) -> WalletBoardSnapshot {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut wallets = BTreeMap::new();
        for wallet in &self.wallets {
            let view = match state.observations.get(&wallet.name).copied() {
                Some(observation) if observation.observed_at.elapsed() <= self.max_stale => {
                    let stale = state.failed_last_refresh.contains(&wallet.name)
                        || observation.observed_at.elapsed() > self.ttl;
                    WalletBalanceView {
                        lbtc_sats: Some(observation.lbtc_sats),
                        balance_source: "lwk-electrum",
                        balance_scope: "whole-wallet",
                        balance_status: if stale { "stale" } else { "live" },
                        balance_as_of_epoch: Some(observation.observed_epoch),
                    }
                }
                _ => WalletBalanceView {
                    lbtc_sats: None,
                    balance_source: "lwk-electrum",
                    balance_scope: "whole-wallet",
                    balance_status: "unavailable",
                    balance_as_of_epoch: None,
                },
            };
            wallets.insert(wallet.name.clone(), view);
        }
        WalletBoardSnapshot {
            wallets,
            source: self.source(),
            refresh_secs: self.ttl.as_secs(),
            max_stale_secs: self.max_stale.as_secs(),
        }
    }
}

fn env_secs(name: &str, default: u64, min: u64, max: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(default)
        .clamp(min, max)
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn load_registry(cfg: &Config) -> Result<Option<BTreeMap<String, String>>> {
    let path = cfg.data_dir.join("wallet_registry.json");
    if !path.is_file() {
        return Ok(None);
    }
    let raw =
        fs::read(&path).with_context(|| format!("read wallet registry {}", path.display()))?;
    let registry: AddressRegistry =
        serde_json::from_slice(&raw).context("parse wallet address registry")?;
    let mut out = BTreeMap::new();
    for entry in registry.wallets {
        if WATCH_NAMES.contains(&entry.name.as_str()) {
            if out.insert(entry.name.clone(), entry.address_0).is_some() {
                bail!("wallet address registry contains duplicate {}", entry.name);
            }
        }
    }
    Ok(Some(out))
}

fn load_bundle(
    path: &Path,
    cfg: &Config,
    registry: &BTreeMap<String, String>,
) -> Result<Vec<WatchWallet>> {
    let metadata = fs::metadata(path).context("wallet watch bundle metadata")?;
    if metadata.len() > MAX_BUNDLE_BYTES {
        bail!("wallet watch bundle exceeds {MAX_BUNDLE_BYTES} bytes");
    }
    let raw = fs::read(path).context("read wallet watch bundle")?;
    let bundle: WatchBundleFile =
        serde_json::from_slice(&raw).context("parse wallet watch bundle")?;
    validate_bundle(bundle, cfg, registry)
}

fn validate_bundle(
    bundle: WatchBundleFile,
    cfg: &Config,
    registry: &BTreeMap<String, String>,
) -> Result<Vec<WatchWallet>> {
    if bundle.version != 1 {
        bail!("wallet watch bundle version must be 1");
    }
    if bundle.network != "liquid-testnet" {
        bail!("wallet watch bundle must target liquid-testnet");
    }
    if bundle.wallets.len() != WATCH_NAMES.len() {
        bail!("wallet watch bundle must contain exactly five predefined wallets");
    }

    let mut seen = BTreeSet::new();
    let mut seen_addresses = BTreeSet::new();
    let mut wallets = Vec::with_capacity(WATCH_NAMES.len());
    for entry in bundle.wallets {
        if !WATCH_NAMES.contains(&entry.name.as_str()) {
            bail!("wallet watch bundle contains an unknown wallet name");
        }
        if !seen.insert(entry.name.clone()) {
            bail!("wallet watch bundle contains a duplicate wallet name");
        }
        validate_descriptor_shape(&entry.descriptor)?;
        let derived =
            lab_chain::wallet_address_from_descriptor(cfg, &entry.name, &entry.descriptor, Some(0))
                .map_err(|_| anyhow::anyhow!("watch descriptor for {} is invalid", entry.name))?;
        if !derived.address.starts_with("tlq1") {
            bail!("watch descriptor for {} is not Liquid Testnet", entry.name);
        }
        let expected = registry
            .get(&entry.name)
            .ok_or_else(|| anyhow::anyhow!("wallet address registry is missing {}", entry.name))?;
        if expected != &derived.address {
            bail!("watch descriptor address mismatch for {}", entry.name);
        }
        if !seen_addresses.insert(derived.address.clone()) {
            bail!("wallet watch bundle contains duplicate derived addresses");
        }
        wallets.push(WatchWallet {
            name: entry.name,
            descriptor: entry.descriptor,
        });
    }
    wallets.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(wallets)
}

fn load_local_descriptors(
    cfg: &Config,
    registry: Option<&BTreeMap<String, String>>,
) -> Result<Vec<WatchWallet>> {
    let mut wallets = Vec::new();
    for name in WATCH_NAMES {
        let path = cfg.wallet_path(name).join("descriptor");
        if !path.is_file() {
            continue;
        }
        let descriptor = fs::read_to_string(&path)
            .with_context(|| format!("read local watch descriptor for {name}"))?;
        let descriptor = descriptor.trim().to_string();
        validate_descriptor_shape(&descriptor)?;
        let derived = lab_chain::wallet_address_from_descriptor(cfg, name, &descriptor, Some(0))
            .map_err(|_| anyhow::anyhow!("local watch descriptor for {name} is invalid"))?;
        if !derived.address.starts_with("tlq1") {
            bail!("local watch descriptor for {name} is not Liquid Testnet");
        }
        if let Some(expected) = registry.and_then(|r| r.get(name)) {
            if expected != &derived.address {
                bail!("local watch descriptor address mismatch for {name}");
            }
        }
        wallets.push(WatchWallet {
            name: name.to_string(),
            descriptor,
        });
    }
    Ok(wallets)
}

fn validate_descriptor_shape(descriptor: &str) -> Result<()> {
    if descriptor.is_empty()
        || descriptor.len() > MAX_DESCRIPTOR_BYTES
        || !descriptor.is_ascii()
        || descriptor.contains(['\r', '\n'])
    {
        bail!("watch descriptor has an invalid shape");
    }
    let lower = descriptor.to_ascii_lowercase();
    // LWK parses the inner descriptor as `DescriptorPublicKey`, which rejects
    // raw private keys. Keep this explicit extended-private-key guard as an
    // early, stable failure before the parser is invoked.
    for forbidden in ["xprv", "tprv", "uprv", "vprv"] {
        if lower.contains(forbidden) {
            bail!("watch descriptor contains spending material");
        }
    }
    if !lower.contains("slip77(") {
        bail!("watch descriptor must contain SLIP77 blinding material");
    }
    if !["xpub", "tpub", "upub", "vpub"]
        .iter()
        .any(|prefix| lower.contains(prefix))
    {
        bail!("watch descriptor must contain a public derivation key");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn fake_board(ttl: Duration) -> WalletBalanceBoard {
        WalletBalanceBoard {
            wallets: vec![WatchWallet {
                name: "alice".into(),
                descriptor: "watch-only".into(),
            }],
            source: "secret-bundle",
            ttl,
            max_stale: Duration::from_secs(900),
            state: Mutex::new(CacheState::default()),
        }
    }

    #[test]
    fn rejects_spending_and_non_blinding_descriptors() {
        for descriptor in [
            "ct(slip77(00),elwpkh(tprv123/*))",
            "ct(slip77(00),elwpkh(xprv123/*))",
            "elwpkh(tpub123/*)",
            "ct(slip77(00),elwpkh(not-a-public-key))",
        ] {
            assert!(validate_descriptor_shape(descriptor).is_err());
        }
        assert!(validate_descriptor_shape("ct(slip77(00),elwpkh(tpub123/*))").is_ok());
    }

    #[test]
    fn accepts_real_watch_only_descriptors_for_the_exact_wallet_set() {
        let mut cfg = Config::load().expect("test config");
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "rgbmvp-wallet-watch-test-{}-{nonce}",
            std::process::id()
        ));
        cfg.data_dir = root.clone();
        cfg.wallet_dir = root.join("wallets");
        cfg.consignment_dir = root.join("consignments");

        let fixture_path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/testnet_wallets.json");
        let fixture: serde_json::Value =
            serde_json::from_slice(&fs::read(fixture_path).unwrap()).unwrap();
        let mut entries = Vec::new();
        let mut registry = BTreeMap::new();
        for name in ["alice", "bob", "carol", "maker"] {
            let mnemonic = fixture["wallets"]
                .as_array()
                .unwrap()
                .iter()
                .find(|wallet| wallet["name"] == name)
                .and_then(|wallet| wallet["mnemonic"].as_str())
                .unwrap();
            let created = lab_chain::wallet_import(&cfg, name, mnemonic, true, Some(name)).unwrap();
            registry.insert(name.to_string(), created.address);
            entries.push(WatchBundleEntry {
                name: name.to_string(),
                descriptor: created.descriptor,
            });
        }
        let lab0 = lab_chain::wallet_create(&cfg, "lab0", true).unwrap();
        registry.insert("lab0".into(), lab0.address);
        entries.push(WatchBundleEntry {
            name: "lab0".into(),
            descriptor: lab0.descriptor,
        });

        let validated = validate_bundle(
            WatchBundleFile {
                version: 1,
                network: "liquid-testnet".into(),
                wallets: entries,
            },
            &cfg,
            &registry,
        )
        .unwrap();
        assert_eq!(validated.len(), 5);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fresh_cache_is_reused_without_rescanning() {
        let board = fake_board(Duration::from_secs(120));
        let calls = AtomicUsize::new(0);
        let first = board.snapshot_with(|_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(42)
        });
        let second = board.snapshot_with(|_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(99)
        });
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(first.wallet("alice").lbtc_sats, Some(42));
        assert_eq!(second.wallet("alice").lbtc_sats, Some(42));
        assert_eq!(second.wallet("alice").balance_status, "live");
    }

    #[test]
    fn failed_refresh_retains_last_value_as_stale() {
        let board = fake_board(Duration::ZERO);
        let first = board.snapshot_with(|_| Ok(42));
        assert_eq!(first.wallet("alice").balance_status, "stale");
        let second = board.snapshot_with(|_| bail!("offline"));
        assert_eq!(second.wallet("alice").lbtc_sats, Some(42));
        assert_eq!(second.wallet("alice").balance_status, "stale");
    }
}
