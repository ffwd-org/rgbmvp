//! HTTP `/v1` surface helpers, public views, and application services.
//!
//! CLI and labd should share these modules so validation is not forked into UI JS.

pub mod s3;
mod services;
mod swap_view;

pub use s3::{
    claim_btc, claim_btc_rgb, claim_btc_value, claim_lq, claim_lq_rgb, claim_lq_value,
    extract_preimage, fund_wrap_btc, fund_wrap_lq, resolve_preimage_from_lq_claim, LQ_POLICY_ASSET,
};
pub use services::SwapService;
pub use swap_view::{public_leg, public_swap_view};

use lab_core::HealthReport;
use serde_json::{json, Value};

/// Wrap a health report as a `/v1/health`-shaped document.
pub fn health_json(report: &HealthReport) -> Value {
    json!({
        "api": lab_core::API_VERSION,
        "path": "/v1/health",
        "body": report,
    })
}

/// Route catalog for browsers and agents (`GET /v1`).
pub fn root_json() -> Value {
    json!({
        "product": lab_core::PRODUCT,
        "api": lab_core::API_VERSION,
        "phase": "u5-axum",
        "message": "RGB Liquid Testnet Lab — U5 labd on Axum; arbitrary public mutations are locked and optional demo flows are fixed, gated, and quota-limited.",
        "security": {
            "browser_seeds": false,
            "preimage_redacted_on_swap_get": true,
            "model": "u4-public-read-only-or-operator-loopback",
            "public_read_only_env": "LABD_PUBLIC_READ_ONLY",
            "api_token_env": "LABD_API_TOKEN",
            "cors_env": "LABD_CORS_ORIGINS",
            "doc": "docs/U4_PUBLIC_HOSTING.md"
        },
        "endpoints": {
            "catalog": "GET /v1",
            "health": "GET /v1/health",
            "security": "GET /v1/security",
            "networks": "GET /v1/networks",
            "verify": "POST /v1/rgb/verify",
            "issue": "POST /v1/rgb/issue",
            "transfer": "POST /v1/rgb/transfer",
            "contracts": "GET /v1/rgb/contracts",
            "plans": "GET /v1/rgb/plans/{id}",
            "proofs": "GET /v1/proofs/{id}",
            "swap": "GET /v1/swap/{id}",
            "swaps": "GET /v1/swaps",
            "swap_init": "POST /v1/swap/init",
            "swap_action": "POST /v1/swap/{id}/action",
            "demo_wallets": "GET /v1/demo/wallets",
            "demo_activity": "GET /v1/demo/activity",
            "demo_rgb_quota": "GET /v1/demo/rgb/quota",
            "demo_rgb_run": "POST /v1/demo/rgb/run (optional; Turnstile + fixed parameters)",
            "audit_bfa": "POST /v1/audit/bfa",
            "audit_bfa_samples": "GET /v1/audit/bfa/samples",
            "phases": "GET /v1/phases"
        },
        "pages": {
            "console": "/",
            "demo_board": "/demo",
            "audit": "/audit",
            "docs_u4": "docs/U4_PUBLIC_HOSTING.md",
            "docs_c3": "docs/C3_CLOSED.md",
            "docs_p3": "docs/P3_PLAN.md"
        },
        "cli": [
            "rgbmvp net status",
            "rgbmvp wallet address|balance",
            "rgbmvp rgb issue|transfer|verify",
            "rgbmvp swap init|status|fund-*|claim-*",
            "rgbmvp bfa audit --history …",
            "rgbmvp covenant demo|demo-c1|demo-c2|demo-c4",
            "rgbmvp serve"
        ],
        "roadmap": {
            "next": "docs/ROADMAP_NEXT.md",
            "ladder": ["S3-negatives", "services", "U5-axum", "S3-http", "S5", "C5"]
        }
    })
}

/// Public security posture (`GET /v1/security`).
pub fn security_json(public_read_only: bool, loopback_bind: bool, token_configured: bool) -> Value {
    json!({
        "api": lab_core::API_VERSION,
        "path": "/v1/security",
        "u4": true,
        "public_read_only": public_read_only,
        "loopback_bind": loopback_bind,
        "api_token_configured": token_configured,
        "mutations": if public_read_only {
            "require_bearer_token_except_enabled_exact_demo_paths"
        } else if loopback_bind {
            "open_on_loopback_unless_token_set"
        } else {
            "require_bearer_token"
        },
        "compute_only_posts": [
            "POST /v1/audit/bfa (embedded witness_tx_hex required on public hosts)"
        ],
        "public_surface": [
            "GET /",
            "GET /demo",
            "GET /audit",
            "GET /v1",
            "GET /v1/health",
            "GET /v1/phases",
            "GET /v1/networks",
            "GET /v1/security",
            "GET /v1/proofs/{id}",
            "GET /v1/swaps",
            "GET /v1/swap/{id}",
            "GET /v1/rgb/contracts",
            "GET /v1/rgb/plans/{id}",
            "GET /v1/demo/*",
            "POST /v1/demo/rgb/run (optional; exact flag-gated fixed flow)",
            "GET /v1/audit/bfa/samples",
            "POST /v1/audit/bfa",
            "GET /artifacts/public/bfa/*"
        ],
        "doc": "docs/U4_PUBLIC_HOSTING.md"
    })
}

/// Ladder phase chips for the demo board / console.
pub fn phases_json() -> Value {
    json!({
        "phases": [
            {"id": "0", "name": "Foundations", "status": "done"},
            {"id": "P0", "name": "RGB on Liquid", "status": "done"},
            {"id": "P1", "name": "HTLC twin swap", "status": "closed", "doc": "docs/P1_CLOSED.md"},
            {"id": "P2", "name": "Simplicity + BFA", "status": "closed", "doc": "docs/P2_CLOSED.md",
             "slices": ["C0", "C1", "C2", "C3", "C4"]},
            {"id": "P3", "name": "Browser lab console", "status": "closed", "doc": "docs/P3_CLOSED.md",
             "slices": ["U0", "U1", "U2", "audit"]},
            {"id": "C3", "name": "BFA audit", "status": "closed", "doc": "docs/C3_CLOSED.md",
             "samples": "GET /v1/audit/bfa/samples"},
            {"id": "S3", "name": "RGB-wrapped claim", "status": "done", "doc": "docs/S3_RGB_WRAP.md",
             "surfaces": ["CLI", "HTTP", "browser"],
             "negatives": "partial-ci",
             "evidence": "artifacts/public/s3-browser-20260724.json"},
            {"id": "U4", "name": "Public hosting security", "status": "implemented", "doc": "docs/U4_PUBLIC_HOSTING.md"},
            {"id": "U5", "name": "labd Axum platform", "status": "implemented", "doc": "docs/U5_AXUM.md"},
            {"id": "S5", "name": "Round-trip swap", "status": "open", "doc": "docs/ROADMAP_NEXT.md"},
            {"id": "C5", "name": "LiquiDEX comparison", "status": "docs-skeleton", "doc": "docs/C5_LIQUIDEX_COMPARISON.md"}
        ]
    })
}

/// Catalog of static BFA history samples for the /audit demo (fallback if index.json missing).
pub fn bfa_samples_json() -> Value {
    json!({
        "product": lab_core::PRODUCT,
        "api": lab_core::API_VERSION,
        "path": "/v1/audit/bfa/samples",
        "doc": "docs/C3_CLOSED.md",
        "note": "Static fixtures with embedded witness_tx_hex (regtest origin). GET-only public surface.",
        "samples": [
            {
                "id": "honest",
                "title": "Honest two-mint history",
                "expect": "ok",
                "path": "bfa/honest.json",
                "url": "/artifacts/public/bfa/honest.json",
                "summary": "Two chained mints with correct seal, anchor, and vault backing."
            },
            {
                "id": "overmint",
                "title": "Over-mint (backing fail)",
                "expect": "fail",
                "path": "bfa/overmint.json",
                "url": "/artifacts/public/bfa/overmint.json",
                "summary": "Mint exceeds locked vault backing — audit fails backing check."
            },
            {
                "id": "lie",
                "title": "Lie about mint size (anchor fail)",
                "expect": "fail",
                "path": "bfa/lie.json",
                "url": "/artifacts/public/bfa/lie.json",
                "summary": "History claims a different mint size than the anchored transition — audit fails anchor."
            }
        ]
    })
}
