//! W3 — custody: where signing material comes from, and refusing unsafe setups.
//!
//! NOTE: this file is `custody.rs`, not `secrets.rs`, because `.gitignore` has a
//! deliberate `secrets*` rule for a repo that handles keys — a source file with
//! that name would be silently excluded from commits.
//!
//! Local development keeps wallet secrets beside the wallet metadata under
//! `RGBMVP_WALLET_DIR`. A public deployment must NOT do that: the image is
//! built in CI and scanned, so anything baked into it is effectively published.
//! Instead the runtime mounts Secret Manager entries at `RGBMVP_SECRET_DIR`
//! and this module resolves signing material from there first.
//!
//! Resolution order for wallet `<name>`, secret `<kind>` (`mnemonic`, `wif`,
//! or the T1-only `seed`):
//!   1. `<dir>/<name>/<kind>` for each dir in `RGBMVP_SECRET_DIR`
//!   2. `<dir>/<name>.<kind>` for each dir (flat mount layout)
//!   3. `$RGBMVP_WALLET_DIR/<name>/<kind>` (local development)
//!
//! `RGBMVP_SECRET_DIR` is a colon-separated LIST, because Cloud Run mounts each
//! Secret Manager entry as its own volume at its own path — one directory
//! cannot hold two secrets.
//!
//! Nothing here ever logs a secret value — only paths and verdicts.

use std::env;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

/// Secret kinds this project stores per wallet.
pub const KIND_MNEMONIC: &str = "mnemonic";
pub const KIND_WIF: &str = "wif";
/// T1 root seed used only for hardened derivation of demo HTLC exit keys.
pub const DEMO_EXIT_SECRET_NAME: &str = "demo-exits";
pub const KIND_EXIT_SEED: &str = "seed";

/// Directories where the runtime mounts Secret Manager entries.
///
/// Colon-separated; empty when unconfigured (local development).
pub fn secret_dirs() -> Vec<PathBuf> {
    env::var("RGBMVP_SECRET_DIR")
        .ok()
        .map(|raw| {
            raw.split(':')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .collect()
        })
        .unwrap_or_default()
}

/// Resolve the file holding a wallet's signing material.
///
/// `wallet_fallback` is the per-wallet directory under `RGBMVP_WALLET_DIR`.
/// Returns `None` when no candidate exists, so callers can report a precise
/// "missing secret" error rather than a confusing read failure.
pub fn resolve_secret_path(
    secret_dirs: &[PathBuf],
    wallet_fallback: &Path,
    name: &str,
    kind: &str,
) -> Option<PathBuf> {
    if let Some(path) = resolve_mounted_secret_path(secret_dirs, name, kind) {
        return Some(path);
    }
    let local = wallet_fallback.join(kind);
    if local.is_file() {
        return Some(local);
    }
    None
}

/// Resolve only from configured runtime mounts, never from local wallet state.
pub fn resolve_mounted_secret_path(
    secret_dirs: &[PathBuf],
    name: &str,
    kind: &str,
) -> Option<PathBuf> {
    for dir in secret_dirs {
        let nested = dir.join(name).join(kind);
        if nested.is_file() {
            return Some(nested);
        }
        let flat = dir.join(format!("{name}.{kind}"));
        if flat.is_file() {
            return Some(flat);
        }
    }
    None
}

/// True when the path is readable by group or others (unix).
///
/// Cloud Run mounts secrets read-only for the runtime user; a broader mode on a
/// self-managed host means any local process can lift the key.
#[cfg(unix)]
pub fn is_world_or_group_readable(path: &Path) -> Result<bool> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)?.permissions().mode();
    Ok(mode & 0o077 != 0)
}

#[cfg(not(unix))]
pub fn is_world_or_group_readable(_path: &Path) -> Result<bool> {
    Ok(false)
}

/// One finding from the custody preflight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CustodyIssue {
    /// Public deployment with signing material not coming from a secret mount.
    SecretsNotMounted,
    /// Secret file is readable beyond the owner.
    LoosePermissions { path: String },
    /// Secret sits inside the application/image directory, i.e. likely baked in.
    BakedIntoImage { path: String },
    /// A wallet the demo needs has no signing material at all.
    MissingSecret { wallet: String, kind: String },
}

impl CustodyIssue {
    /// Whether this must block startup rather than merely warn.
    pub fn is_fatal(&self) -> bool {
        match self {
            // Publishing a key is unrecoverable; refuse to run.
            CustodyIssue::BakedIntoImage { .. } => true,
            CustodyIssue::LoosePermissions { .. } => true,
            // Fatal only in public mode — see `preflight`'s `public` flag.
            CustodyIssue::SecretsNotMounted => true,
            // The demo simply cannot operate without its keys.
            CustodyIssue::MissingSecret { .. } => true,
        }
    }

    pub fn message(&self) -> String {
        match self {
            CustodyIssue::SecretsNotMounted => {
                "demo swaps are enabled on a public bind but RGBMVP_SECRET_DIR is not set; \
                 mount wallet secrets from Secret Manager instead of shipping them in the image"
                    .into()
            }
            CustodyIssue::LoosePermissions { path } => format!(
                "secret {path} is readable by group/other; tighten to 0400/0600"
            ),
            CustodyIssue::BakedIntoImage { path } => format!(
                "secret {path} lives inside the application directory and would be baked into \
                 the published image; mount it from Secret Manager instead"
            ),
            CustodyIssue::MissingSecret { wallet, kind } => {
                format!("wallet {wallet} has no {kind}; the demo cannot sign for it")
            }
        }
    }
}

/// Directories that indicate a secret would ship inside the container image.
fn looks_like_image_path(path: &Path) -> bool {
    let p = path.to_string_lossy();
    p.starts_with("/app/") || p == "/app"
}

/// Inputs for the custody preflight.
#[derive(Debug, Clone)]
pub struct CustodyCheck<'a> {
    /// Wallets that must be signable, as `(name, kind)`.
    pub required: &'a [(String, String)],
    /// Per-wallet fallback directory resolver.
    pub wallet_dir: &'a Path,
    /// True when labd is exposed publicly (non-loopback bind or read-only mode).
    pub public: bool,
}

/// Verify custody before the demo is allowed to sign anything.
///
/// Returns every issue found; the caller decides how to react (labd refuses to
/// start on any fatal issue when demo swaps are enabled).
pub fn preflight(check: &CustodyCheck<'_>) -> Vec<CustodyIssue> {
    let mut issues = Vec::new();
    let sdirs = secret_dirs();

    if check.public && sdirs.is_empty() {
        issues.push(CustodyIssue::SecretsNotMounted);
    }

    for (name, kind) in check.required {
        let fallback = check.wallet_dir.join(name);
        let path = if check.public {
            resolve_mounted_secret_path(&sdirs, name, kind)
        } else {
            resolve_secret_path(&sdirs, &fallback, name, kind)
        };
        match path {
            None => issues.push(CustodyIssue::MissingSecret {
                wallet: name.clone(),
                kind: kind.clone(),
            }),
            Some(p) => {
                if check.public && looks_like_image_path(&p) {
                    issues.push(CustodyIssue::BakedIntoImage {
                        path: p.display().to_string(),
                    });
                }
                match is_world_or_group_readable(&p) {
                    Ok(true) => issues.push(CustodyIssue::LoosePermissions {
                        path: p.display().to_string(),
                    }),
                    Ok(false) => {}
                    Err(e) => {
                        // Unreadable metadata is itself a missing-secret signal.
                        eprintln!("custody: cannot stat {}: {e}", p.display());
                        issues.push(CustodyIssue::MissingSecret {
                            wallet: name.clone(),
                            kind: kind.clone(),
                        });
                    }
                }
            }
        }
    }
    issues
}

/// Fail fast with a combined report when any fatal issue is present.
pub fn enforce(issues: &[CustodyIssue]) -> Result<()> {
    let fatal: Vec<&CustodyIssue> = issues.iter().filter(|i| i.is_fatal()).collect();
    if fatal.is_empty() {
        return Ok(());
    }
    let mut msg = String::from("custody preflight failed:");
    for i in &fatal {
        msg.push_str("\n  - ");
        msg.push_str(&i.message());
    }
    bail!(msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "rgbmvp-secrets-{}-{}-{tag}",
            std::process::id(),
            tag
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[cfg(unix)]
    fn write_mode(path: &Path, contents: &str, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        if let Some(p) = path.parent() {
            fs::create_dir_all(p).unwrap();
        }
        fs::write(path, contents).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    /// A mounted secret must win over the local wallet copy.
    #[test]
    fn secret_mount_takes_precedence_over_wallet_dir() {
        let root = tmpdir("prec");
        let sdir = root.join("secrets");
        let wdir = root.join("wallets");
        fs::create_dir_all(sdir.join("bob")).unwrap();
        fs::create_dir_all(wdir.join("bob")).unwrap();
        fs::write(sdir.join("bob").join("mnemonic"), "from-mount").unwrap();
        fs::write(wdir.join("bob").join("mnemonic"), "from-disk").unwrap();

        let got =
            resolve_secret_path(&[sdir.clone()], &wdir.join("bob"), "bob", KIND_MNEMONIC)
                .unwrap();
        assert_eq!(fs::read_to_string(got).unwrap(), "from-mount");
        let _ = fs::remove_dir_all(&root);
    }

    /// Cloud Run gives each Secret Manager entry its own mountPath, so a real
    /// deployment always has the two wallet keys in DIFFERENT directories.
    #[test]
    fn secrets_resolve_across_multiple_mount_dirs() {
        let root = tmpdir("multidir");
        let btc_mount = root.join("secrets");
        let lq_mount = root.join("secrets-lq");
        fs::create_dir_all(btc_mount.join("btc-alice")).unwrap();
        fs::create_dir_all(lq_mount.join("bob")).unwrap();
        fs::write(btc_mount.join("btc-alice").join("wif"), "wif").unwrap();
        fs::write(lq_mount.join("bob").join("mnemonic"), "mn").unwrap();

        let dirs = vec![btc_mount.clone(), lq_mount.clone()];
        // Each key is found even though it lives in only one of the mounts.
        assert!(
            resolve_secret_path(&dirs, &root.join("none"), "btc-alice", KIND_WIF).is_some(),
            "BTC key must resolve from the first mount"
        );
        assert!(
            resolve_secret_path(&dirs, &root.join("none"), "bob", KIND_MNEMONIC).is_some(),
            "Liquid key must resolve from the SECOND mount"
        );
        // A single-dir configuration would have missed the second key entirely.
        assert!(
            resolve_secret_path(&[btc_mount], &root.join("none"), "bob", KIND_MNEMONIC).is_none()
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn mounted_only_resolution_never_uses_local_fallback() {
        let root = tmpdir("mounted-only");
        let sdir = root.join("secrets");
        let fallback = root.join("wallets").join("demo-exits");
        fs::create_dir_all(&sdir).unwrap();
        fs::create_dir_all(&fallback).unwrap();
        fs::write(fallback.join(KIND_EXIT_SEED), "local").unwrap();

        assert!(resolve_secret_path(
            std::slice::from_ref(&sdir),
            &fallback,
            DEMO_EXIT_SECRET_NAME,
            KIND_EXIT_SEED,
        )
        .is_some());
        assert!(resolve_mounted_secret_path(
            &[sdir],
            DEMO_EXIT_SECRET_NAME,
            KIND_EXIT_SEED,
        )
        .is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn secret_dir_env_parses_colon_separated_list() {
        std::env::set_var("RGBMVP_SECRET_DIR", "/secrets:/secrets-lq");
        let d = secret_dirs();
        assert_eq!(d.len(), 2);
        assert_eq!(d[0], PathBuf::from("/secrets"));
        assert_eq!(d[1], PathBuf::from("/secrets-lq"));

        std::env::set_var("RGBMVP_SECRET_DIR", "  ");
        assert!(secret_dirs().is_empty(), "blank must mean unconfigured");
        std::env::remove_var("RGBMVP_SECRET_DIR");
        assert!(secret_dirs().is_empty());
    }

    #[test]
    fn flat_mount_layout_is_supported() {
        let root = tmpdir("flat");
        let sdir = root.join("secrets");
        fs::create_dir_all(&sdir).unwrap();
        fs::write(sdir.join("bob.mnemonic"), "flat").unwrap();
        let got = resolve_secret_path(&[sdir.clone()], &root.join("nope"), "bob", KIND_MNEMONIC);
        assert!(got.is_some(), "flat <name>.<kind> layout must resolve");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn falls_back_to_wallet_dir_for_local_dev() {
        let root = tmpdir("fallback");
        let wdir = root.join("wallets").join("alice");
        fs::create_dir_all(&wdir).unwrap();
        fs::write(wdir.join("wif"), "local").unwrap();
        let got = resolve_secret_path(&[], &wdir, "alice", KIND_WIF);
        assert!(got.is_some());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_secret_is_reported_not_guessed() {
        let root = tmpdir("missing");
        let got = resolve_secret_path(&[], &root.join("ghost"), "ghost", KIND_MNEMONIC);
        assert!(got.is_none());
        let _ = fs::remove_dir_all(&root);
    }

    /// Public deployment without a secret mount must refuse to start.
    #[test]
    fn public_without_secret_mount_is_fatal() {
        let root = tmpdir("nomount");
        let wdir = root.join("wallets");
        fs::create_dir_all(wdir.join("bob")).unwrap();
        fs::write(wdir.join("bob").join("mnemonic"), "x").unwrap();
        std::env::remove_var("RGBMVP_SECRET_DIR");

        let required = vec![("bob".to_string(), KIND_MNEMONIC.to_string())];
        let issues = preflight(&CustodyCheck {
            required: &required,
            wallet_dir: &wdir,
            public: true,
        });
        assert!(issues.contains(&CustodyIssue::SecretsNotMounted));
        assert!(enforce(&issues).is_err(), "must block startup");
        let _ = fs::remove_dir_all(&root);
    }

    /// The same setup is fine for local (loopback) development.
    #[test]
    fn local_dev_without_secret_mount_is_allowed() {
        let root = tmpdir("localok");
        let wdir = root.join("wallets");
        fs::create_dir_all(wdir.join("bob")).unwrap();
        #[cfg(unix)]
        write_mode(&wdir.join("bob").join("mnemonic"), "x", 0o600);
        #[cfg(not(unix))]
        fs::write(wdir.join("bob").join("mnemonic"), "x").unwrap();
        std::env::remove_var("RGBMVP_SECRET_DIR");

        let required = vec![("bob".to_string(), KIND_MNEMONIC.to_string())];
        let issues = preflight(&CustodyCheck {
            required: &required,
            wallet_dir: &wdir,
            public: false,
        });
        assert!(
            enforce(&issues).is_ok(),
            "loopback dev must not require a secret mount, got {issues:?}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A group/world-readable key blocks startup.
    #[cfg(unix)]
    #[test]
    fn loose_permissions_are_fatal() {
        let root = tmpdir("perms");
        let wdir = root.join("wallets");
        write_mode(&wdir.join("bob").join("mnemonic"), "x", 0o644);
        std::env::remove_var("RGBMVP_SECRET_DIR");

        let required = vec![("bob".to_string(), KIND_MNEMONIC.to_string())];
        let issues = preflight(&CustodyCheck {
            required: &required,
            wallet_dir: &wdir,
            public: false,
        });
        assert!(
            issues
                .iter()
                .any(|i| matches!(i, CustodyIssue::LoosePermissions { .. })),
            "0644 secret must be flagged, got {issues:?}"
        );
        assert!(enforce(&issues).is_err());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn image_paths_are_detected() {
        assert!(looks_like_image_path(Path::new("/app/wallets/bob/mnemonic")));
        assert!(!looks_like_image_path(Path::new("/secrets/bob/mnemonic")));
        assert!(!looks_like_image_path(Path::new("/tmp/app/x")));
    }

    #[test]
    fn missing_secret_issue_is_fatal_and_named() {
        let i = CustodyIssue::MissingSecret {
            wallet: "bob".into(),
            kind: "mnemonic".into(),
        };
        assert!(i.is_fatal());
        assert!(i.message().contains("bob"));
    }
}
