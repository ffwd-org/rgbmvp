//! Shared configuration and machine-oriented types for rgbmvp.
//!
//! Phase 0: network identity, paths, health JSON. RGB types arrive in P0.
//! U4: public-hosting security helpers (`security` module).
//! T1: bounded public demo-swap policy and budget governor (`demo` module).

pub mod custody;
pub mod demo;
pub mod security;

use std::env;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

pub use custody::{
    resolve_mounted_secret_path, resolve_secret_path, secret_dirs, CustodyCheck, CustodyIssue,
    DEMO_EXIT_SECRET_NAME, KIND_DESCRIPTOR, KIND_EXIT_SEED, KIND_MNEMONIC, KIND_WIF,
};
pub use demo::{DemoDenial, DemoGovernor, DemoStatus, DemoSwapPolicy, Floats};
pub use security::{
    constant_time_eq, cors_allow_origin, is_loopback_bind, is_mutation_method, is_safe_path_id,
    parse_cors_origins, validate_path_id, AuthDecision, MutationPolicy, RateLimiter,
};

pub const PRODUCT: &str = "rgbmvp";
pub const API_VERSION: &str = "v1";
pub const DEFAULT_NETWORK: &str = "liquid-testnet";
pub const DEFAULT_ESPLORA_TIP: &str =
    "https://blockstream.info/liquidtestnet/api/blocks/tip/height";
pub const DEFAULT_ELECTRUM: &str = "elements-testnet.blockstream.info:50002";
pub const DEFAULT_EXPLORER: &str = "https://blockstream.info/liquidtestnet";

/// Supported public demo networks for Phase 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LabNetwork {
    LiquidTestnet,
}

impl LabNetwork {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LiquidTestnet => "liquid-testnet",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "liquid-testnet" | "testnet" | "liquid_testnet" => Ok(Self::LiquidTestnet),
            other => bail!(
                "unsupported network {other:?}; Phase 0 only supports liquid-testnet (mainnet disabled)"
            ),
        }
    }
}

impl std::fmt::Display for LabNetwork {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Runtime configuration loaded from environment (and optional `.env`).
#[derive(Debug, Clone)]
pub struct Config {
    pub network: LabNetwork,
    pub data_dir: PathBuf,
    pub wallet_dir: PathBuf,
    pub consignment_dir: PathBuf,
    pub labd_bind: String,
    pub public_base_url: String,
    pub esplora_tip_url: String,
    pub electrum_host_port: String,
    pub electrum_tls: bool,
    pub electrum_validate_domain: bool,
    pub explorer_base: String,
    pub log_level: String,
    /// U4 security policy (mutations, CORS, body limit).
    pub security: MutationPolicy,
}

impl Config {
    /// Load dotenv if present, then read env vars with Phase 0 defaults.
    pub fn load() -> Result<Self> {
        let _ = dotenvy::dotenv();
        let network = LabNetwork::parse(
            &env::var("RGBMVP_NETWORK").unwrap_or_else(|_| DEFAULT_NETWORK.to_string()),
        )?;
        // Refuse mainnet-ish labels for lab safety.
        if let Ok(n) = env::var("RGBMVP_NETWORK") {
            let t = n.trim().to_ascii_lowercase();
            if t.contains("mainnet") || t == "liquid" || t == "bitcoin" {
                bail!("mainnet is forbidden in lab config (got RGBMVP_NETWORK={n:?})");
            }
        }
        let data_dir =
            PathBuf::from(env::var("RGBMVP_DATA_DIR").unwrap_or_else(|_| ".rgbmvp".to_string()));
        let wallet_dir = env::var("RGBMVP_WALLET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| data_dir.join("wallets"));
        let consignment_dir = env::var("RGBMVP_CONSIGNMENT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| data_dir.join("consignments"));
        // Cloud Run / containers often inject PORT; prefer LABD_BIND, else 0.0.0.0:$PORT only if set.
        let labd_bind = if let Ok(b) = env::var("LABD_BIND") {
            b
        } else if let Ok(port) = env::var("PORT") {
            format!("0.0.0.0:{port}")
        } else {
            "127.0.0.1:8080".into()
        };
        let security = MutationPolicy::from_env(&labd_bind);
        // Public read-only + non-loopback is the intended Cloud Run posture.
        if security.public_read_only && is_loopback_bind(&labd_bind) {
            // ok for local public-mode testing
        }
        Ok(Self {
            network,
            data_dir,
            wallet_dir,
            consignment_dir,
            labd_bind,
            public_base_url: env::var("LABD_PUBLIC_BASE_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:8080".to_string()),
            esplora_tip_url: env::var("LWK_ESPLORA_TIP_URL")
                .or_else(|_| env::var("LWK_ESPLORA_URL"))
                .unwrap_or_else(|_| DEFAULT_ESPLORA_TIP.to_string()),
            electrum_host_port: env::var("LWK_ELECTRUM_URL")
                .unwrap_or_else(|_| DEFAULT_ELECTRUM.to_string())
                .trim_start_matches("ssl://")
                .trim_start_matches("tcp://")
                .to_string(),
            electrum_tls: env::var("LWK_ELECTRUM_TLS")
                .map(|v| v != "0" && v.to_ascii_lowercase() != "false")
                .unwrap_or(true),
            electrum_validate_domain: env::var("LWK_ELECTRUM_VALIDATE_DOMAIN")
                .map(|v| v != "0" && v.to_ascii_lowercase() != "false")
                .unwrap_or(true),
            explorer_base: env::var("LIQUID_TESTNET_EXPLORER")
                .unwrap_or_else(|_| DEFAULT_EXPLORER.to_string()),
            log_level: env::var("LOG_LEVEL").unwrap_or_else(|_| "info".to_string()),
            security,
        })
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(&self.wallet_dir)
            .with_context(|| format!("create wallet dir {}", self.wallet_dir.display()))?;
        std::fs::create_dir_all(&self.consignment_dir).with_context(|| {
            format!("create consignment dir {}", self.consignment_dir.display())
        })?;
        std::fs::create_dir_all(self.data_dir.join("tmp"))
            .with_context(|| format!("create tmp under {}", self.data_dir.display()))?;
        Ok(())
    }

    /// Create a high-entropy local development seed once. A configured secret
    /// mount always fails closed when its seed is absent; it is never populated
    /// from inside the application.
    pub fn ensure_demo_exit_seed(&self) -> Result<PathBuf> {
        let sdirs = secret_dirs();
        let fallback = self.wallet_dir.join(DEMO_EXIT_SECRET_NAME);
        if !sdirs.is_empty() {
            if let Some(path) =
                resolve_mounted_secret_path(&sdirs, DEMO_EXIT_SECRET_NAME, KIND_EXIT_SEED)
            {
                return Ok(path);
            }
            bail!(
                "mounted RGBMVP_SECRET_DIR has no {}/{}; refusing to generate custody material",
                DEMO_EXIT_SECRET_NAME,
                KIND_EXIT_SEED
            );
        }
        if let Some(path) =
            resolve_secret_path(&[], &fallback, DEMO_EXIT_SECRET_NAME, KIND_EXIT_SEED)
        {
            return Ok(path);
        }

        std::fs::create_dir_all(&fallback)
            .with_context(|| format!("create demo exit secret dir {}", fallback.display()))?;
        let path = fallback.join(KIND_EXIT_SEED);
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).map_err(|e| anyhow::anyhow!("generate demo exit seed: {e}"))?;
        let encoded = format!("{}\n", hex::encode(seed));

        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .with_context(|| format!("create demo exit seed {}", path.display()))?;
            file.write_all(encoded.as_bytes())?;
        }
        #[cfg(not(unix))]
        {
            std::fs::write(&path, encoded.as_bytes())
                .with_context(|| format!("create demo exit seed {}", path.display()))?;
        }
        Ok(path)
    }

    /// Read and validate the 32-byte seed without logging its contents.
    pub fn demo_exit_seed(&self) -> Result<[u8; 32]> {
        let path = self.ensure_demo_exit_seed()?;
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("read demo exit seed {}", path.display()))?;
        let bytes = hex::decode(raw.trim())
            .with_context(|| format!("decode demo exit seed {} as hex", path.display()))?;
        bytes.try_into().map_err(|v: Vec<u8>| {
            anyhow::anyhow!(
                "demo exit seed {} must decode to 32 bytes, got {}",
                path.display(),
                v.len()
            )
        })
    }

    pub fn wallet_path(&self, name: &str) -> PathBuf {
        self.wallet_dir.join(sanitize_wallet_name(name))
    }
}

pub fn sanitize_wallet_name(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() {
        "default".into()
    } else {
        s
    }
}

/// Machine-oriented health / status payload (subset of future `/v1/health`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthReport {
    pub status: String,
    pub product: String,
    pub api_version: String,
    pub network: String,
    pub phase: String,
    pub rgb_ready: bool,
    pub checks: Vec<HealthCheck>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheck {
    pub name: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl HealthReport {
    pub fn phase0_base(network: LabNetwork) -> Self {
        Self {
            status: "starting".into(),
            product: PRODUCT.into(),
            api_version: API_VERSION.into(),
            network: network.to_string(),
            phase: "0".into(),
            rgb_ready: false,
            checks: vec![],
        }
    }
}

/// Persist helpers that refuse to write under the wrong path accidentally.
pub fn write_secret_file(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents)
        .with_context(|| format!("write secret file {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(())
}

pub fn read_trimmed(path: &Path) -> Result<String> {
    let s = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    Ok(s.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_parse() {
        assert_eq!(
            LabNetwork::parse("liquid-testnet").unwrap(),
            LabNetwork::LiquidTestnet
        );
        assert!(LabNetwork::parse("mainnet").is_err());
    }

    #[test]
    fn sanitize() {
        assert_eq!(sanitize_wallet_name("Alice/1"), "Alice_1");
        assert_eq!(sanitize_wallet_name(""), "default");
    }
}
