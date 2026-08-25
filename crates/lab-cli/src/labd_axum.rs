//! U5 — Axum/Hyper labd (default for `rgbmvp serve`).
//!
//! Same `/v1` shapes and U4 security as the legacy TCP server; mutations call
//! shared `http_api` handlers (often via `spawn_blocking` for LWK I/O).

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use lab_core::{
    cors_allow_origin, is_loopback_bind, is_mutation_method, validate_path_id, AuthDecision,
    Config, DemoGovernor, RateLimiter,
};

use crate::demo_swap::{self, quota_json, BotCheck, DemoFees, DemoWallets, FloatCache};
use lab_rgb::storage::RgbStore;
use lab_rgb::swap::SwapStore;
use serde_json::{json, Value};
use tower::ServiceBuilder;
use tower_http::trace::TraceLayer;

use crate::http_api::{
    demo_activity, demo_wallets, handle_bfa_audit_post, handle_bfa_audit_post_public,
    handle_rgb_issue_post, handle_rgb_transfer_post, handle_swap_action_post,
    handle_swap_init_post, handle_verify_post, list_rgb_contracts, list_swap_ids, public_swap_view,
};
use crate::wallet_watch::WalletBalanceBoard;

#[derive(Clone)]
struct AppState {
    cfg: Config,
    web_dir: PathBuf,
    artifacts_dir: PathBuf,
    verify_limiter: Arc<RateLimiter>,
    /// T1 bounded public demo swaps: admission + spend governor (off by default).
    demo: Arc<DemoGovernor>,
    /// Anonymous fixed-parameter RGB lab, with an independent durable budget.
    rgb_demo: Arc<DemoGovernor>,
    demo_floats: Arc<FloatCache>,
    /// Display-only Liquid balance cache; never used for spend admission.
    wallet_balance_board: Arc<WalletBalanceBoard>,
    demo_wallets: DemoWallets,
    demo_fees: DemoFees,
    /// Exact number of trusted proxy entries at the right edge of XFF.
    client_ip_policy: ClientIpPolicy,
    /// Monotonic counter feeding demo swap ids.
    demo_seq: Arc<std::sync::atomic::AtomicU64>,
}

/// Run labd on Axum (blocks the calling thread via Tokio runtime).
pub fn serve(cfg: &Config, bind: &str) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("tokio runtime")?;
    rt.block_on(serve_async(cfg.clone(), bind.to_string()))
}

async fn serve_async(cfg: Config, bind: String) -> Result<()> {
    let sec = cfg.security.clone();
    eprintln!("labd (axum/U5) listening on http://{bind}");
    eprintln!(
        "  U4 security: public_read_only={} loopback_bind={} token_configured={} max_body={}",
        sec.public_read_only,
        is_loopback_bind(&bind),
        sec.api_token.is_some(),
        sec.max_body_bytes
    );
    eprintln!("  GET  /  /demo  /audit  /status  /v1/*");
    if sec.public_read_only {
        eprintln!("  POST (mutations) require Authorization: Bearer <LABD_API_TOKEN>");
    } else {
        eprintln!("  POST /v1/rgb/* · /v1/swap/* · /v1/audit/bfa");
    }
    eprintln!("  (set LABD_HTTP=legacy for handwritten TCP server)");

    let web_dir = PathBuf::from(std::env::var("LABD_WEB_DIR").unwrap_or_else(|_| "web".into()));
    let artifacts_dir = PathBuf::from(
        std::env::var("LABD_ARTIFACTS_DIR").unwrap_or_else(|_| "artifacts/public".into()),
    );
    let demo = Arc::new(DemoGovernor::from_env());
    let rgb_demo = Arc::new(DemoGovernor::new(crate::rgb_demo::policy_from_env()));
    let client_ip_policy =
        ClientIpPolicy::from_env().context("invalid client-IP proxy trust configuration")?;
    if demo.enabled() {
        let p = demo.policy();
        eprintln!(
            "  T1 demo swaps: ENABLED leg={}sats rgb_wrap=false csv={} daily={} concurrent={} budget={}sats (~{} swaps)",
            p.leg_sats,
            p.csv_delay,
            p.daily_cap,
            p.max_concurrent,
            p.fee_budget_sats,
            demo.swaps_remaining_in_budget()
        );
        eprintln!("  T1 client IP: {}", client_ip_policy.description());
        if p.turnstile_required {
            demo_swap::validate_turnstile_config()
                .context("T1 refused: Turnstile context is not fail-closed")?;
        } else {
            eprintln!("  WARNING: demo swaps running WITHOUT bot protection (local/testing only)");
        }
        // Budget accounting reserves `max_fee_per_swap_sats` per swap. If the
        // fees we actually pay exceed that, the reservation under-counts and the
        // run can overshoot its ceiling.
        let fees = DemoFees::from_env();
        fees.validate_reservation(p.max_fee_per_swap_sats)
            .context("T1 refused: budget reservation would be unsound")?;
        // W3: refuse to sign with image-baked or over-permissive key material.
        // Public = anything not bound to loopback, or explicitly read-only mode.
        let wallets = DemoWallets::from_env();
        let public = !is_loopback_bind(&bind) || sec.public_read_only;
        let required = vec![
            (wallets.alice_btc.clone(), lab_core::KIND_WIF.to_string()),
            (wallets.bob_lq.clone(), lab_core::KIND_MNEMONIC.to_string()),
            (
                lab_core::DEMO_EXIT_SECRET_NAME.to_string(),
                lab_core::KIND_EXIT_SEED.to_string(),
            ),
        ];
        let issues = lab_core::custody::preflight(&lab_core::CustodyCheck {
            required: &required,
            wallet_dir: &cfg.wallet_dir,
            public,
        });
        for i in &issues {
            eprintln!("  custody: {}", i.message());
        }
        lab_core::custody::enforce(&issues)
            .context("demo swaps refused to start (W3 custody preflight)")?;
        eprintln!(
            "  T1 custody OK: secret_dir={} public={}",
            {
                let d = lab_core::secret_dirs();
                if d.is_empty() {
                    "(local wallet dir)".to_string()
                } else {
                    d.iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join(":")
                }
            },
            public
        );
        // W4: recover the spend ceiling so a restart cannot silently reset it.
        demo_swap::restore_budget(&cfg, &demo)
            .context("demo swaps refused to start (W4 budget recovery)")?;
    }
    if rgb_demo.enabled() {
        let public = !is_loopback_bind(&bind) || sec.public_read_only;
        demo_swap::validate_turnstile_config()
            .context("RGB demo refused: Turnstile context is not fail-closed")?;
        let required = vec![
            (
                crate::rgb_demo::SENDER_WALLET.to_string(),
                lab_core::KIND_MNEMONIC.to_string(),
            ),
            (
                crate::rgb_demo::SENDER_WALLET.to_string(),
                lab_core::KIND_DESCRIPTOR.to_string(),
            ),
        ];
        let issues = lab_core::custody::preflight(&lab_core::CustodyCheck {
            required: &required,
            wallet_dir: &cfg.wallet_dir,
            public,
        });
        lab_core::custody::enforce(&issues)
            .context("RGB demo refused to start (custody preflight)")?;
        demo_swap::restore_named_budget(&cfg, &rgb_demo, crate::rgb_demo::BUDGET_NAME, "RGB demo")
            .context("RGB demo refused to start (budget recovery)")?;
        eprintln!(
            "  RGB demo: ENABLED fixed {} -> {}, daily={} budget={}sats",
            crate::rgb_demo::SENDER_WALLET,
            crate::rgb_demo::RECEIVER_WALLET,
            rgb_demo.policy().daily_cap,
            rgb_demo.policy().fee_budget_sats
        );
    }
    let wallet_balance_board = Arc::new(
        WalletBalanceBoard::from_config(&cfg)
            .context("public wallet balance board configuration refused")?,
    );
    eprintln!(
        "  demo wallet balances: source={} wallets={} refresh={}s",
        wallet_balance_board.source(),
        wallet_balance_board.configured_wallets(),
        wallet_balance_board.refresh_secs()
    );
    let state = AppState {
        cfg: cfg.clone(),
        web_dir,
        artifacts_dir,
        verify_limiter: Arc::new(RateLimiter::from_env_verify()),
        demo,
        rgb_demo,
        demo_floats: Arc::new(FloatCache::new()),
        wallet_balance_board,
        demo_wallets: DemoWallets::from_env(),
        demo_fees: DemoFees::from_env(),
        client_ip_policy,
        demo_seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
    };

    // W5: periodic refund/recycle watcher. HTLC refunds return value to the
    // original funder, so a swap that stalls does not permanently drain the
    // faucet pool — only fees burn.
    if state.demo.enabled() {
        let (interval_secs, min_age_secs) = demo_swap::sweep_config_from_env();
        let sweep_cfg = state.cfg.clone();
        let sweep_wallets = state.demo_wallets.clone();
        let fees = state.demo_fees;
        eprintln!(
            "  T1 refund watcher: every {interval_secs}s, sweeping swaps older than {min_age_secs}s"
        );
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
            // The first tick fires immediately; skip it so startup stays quiet.
            tick.tick().await;
            loop {
                tick.tick().await;
                let cfg = sweep_cfg.clone();
                let wallets = sweep_wallets.clone();
                match tokio::task::spawn_blocking(move || {
                    demo_swap::sweep_stuck_demo_swaps_blocking(&cfg, &wallets, fees, min_age_secs)
                })
                .await
                {
                    Ok(rep)
                        if rep.eligible > 0
                            || rep.errors > 0
                            || rep.recycled_sats > 0
                            || rep.recycled_lq_sats > 0 =>
                    {
                        eprintln!("demo sweep: {rep:?}");
                    }
                    Ok(_) => {}
                    Err(e) => eprintln!("demo sweep task failed: {e}"),
                }
            }
        });
    }

    let app = router(state.clone()).layer(
        ServiceBuilder::new()
            .layer(TraceLayer::new_for_http())
            .layer(DefaultBodyLimit::max(sec.max_body_bytes)),
    );

    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("bind {bind}"))?;
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .context("axum serve")?;
    Ok(())
}

fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(page_index))
        .route("/index.html", get(page_index))
        .route("/demo", get(page_demo))
        .route("/demo.html", get(page_demo))
        .route("/audit", get(page_audit))
        .route("/audit.html", get(page_audit))
        .route("/status", get(page_status))
        .route("/status.html", get(page_status))
        .route("/swap", get(page_swap))
        .route("/swap.html", get(page_swap))
        .route("/manifest.json", get(artifact_manifest))
        .route("/artifacts/public/{*rest}", get(artifact_public))
        .route("/v1", get(v1_root))
        .route("/v1/phases", get(v1_phases))
        .route("/v1/health", get(v1_health))
        .route("/v1/networks", get(v1_networks))
        .route("/v1/security", get(v1_security))
        .route("/v1/proofs/{id}", get(v1_proof))
        .route("/v1/swaps", get(v1_swaps))
        .route("/v1/swap/{id}", get(v1_swap_get))
        .route("/v1/swap/init", post(v1_swap_init))
        .route("/v1/swap/{id}/action", post(v1_swap_action))
        .route("/v1/demo/wallets", get(v1_demo_wallets))
        .route("/v1/demo/activity", get(v1_demo_activity))
        .route("/v1/rgb/contracts", get(v1_rgb_contracts))
        .route("/v1/rgb/plans/{id}", get(v1_rgb_plan))
        .route("/v1/rgb/verify", post(v1_rgb_verify))
        .route("/v1/rgb/issue", post(v1_rgb_issue))
        .route("/v1/rgb/transfer", post(v1_rgb_transfer))
        .route("/v1/audit/bfa/samples", get(v1_bfa_samples))
        .route("/v1/audit/bfa", post(v1_audit_bfa))
        // T1 — bounded public demo swaps. The POST is exempt from the U4
        // mutation-token check (see `u4_middleware`) because it carries its own
        // Turnstile + quota + budget gate and accepts no protocol parameters.
        .route("/v1/demo/swap", post(v1_demo_swap))
        .route("/v1/demo/quota", get(v1_demo_quota))
        // Fixed anonymous RGB lab. The POST accepts only a Turnstile token;
        // every chain and asset parameter is selected server-side.
        .route("/v1/demo/rgb/run", post(v1_demo_rgb_run))
        .route("/v1/demo/rgb/quota", get(v1_demo_rgb_quota))
        .layer(middleware::from_fn_with_state(state.clone(), u4_middleware))
        .with_state(state)
}

/// Client IP resolved once by the middleware and carried in request extensions.
///
/// Centralized so every consumer sees the same value and the trust rules for
/// `X-Forwarded-For` live in exactly one place.
#[derive(Clone, Debug)]
struct ClientIp(String);

/// U4: CORS echo, mutation auth, OPTIONS short-circuit + security headers.
async fn u4_middleware(
    State(state): State<AppState>,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    let path = req.uri().path().to_string();
    // Resolve the peer once; absent when the server is not wired with
    // ConnectInfo (e.g. tower `oneshot` in tests) — handled conservatively.
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0);
    let resolved_ip = client_ip(req.headers(), peer, state.client_ip_policy);
    req.extensions_mut().insert(ClientIp(resolved_ip));
    let origin = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let acao = cors_allow_origin(&state.cfg.security, origin.as_deref());
    let method = req.method().clone();

    if method == Method::OPTIONS {
        return finalize_response(
            cors_response(StatusCode::NO_CONTENT, acao.as_deref(), Body::empty()),
            &path,
            acao.as_deref(),
        );
    }

    // Public mutations are an explicit exact-path allowlist. Each exemption is
    // independently flag-gated and performs Turnstile, quota, durable budget,
    // and float admission inside its handler.
    let demo_exempt = state.demo.enabled() && path == "/v1/demo/swap";
    let rgb_demo_exempt = state.rgb_demo.enabled() && path == "/v1/demo/rgb/run";
    // BFA audit is a bounded, compute-only operation. Public mode additionally
    // requires embedded witnesses, so it performs no RPC, filesystem write, or
    // subprocess invocation and is not a mutation despite using POST.
    let public_audit = path == "/v1/audit/bfa";

    if is_mutation_method(method.as_str()) && !demo_exempt && !rgb_demo_exempt && !public_audit {
        let auth = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok());
        match state.cfg.security.authorize_mutation(auth) {
            AuthDecision::Allow => {}
            AuthDecision::Deny {
                status,
                code,
                message,
            } => {
                let sc = StatusCode::from_u16(status).unwrap_or(StatusCode::FORBIDDEN);
                let body = json!({"error": message, "status": "error", "code": code});
                return finalize_response(
                    cors_json(sc, acao.as_deref(), body),
                    &path,
                    acao.as_deref(),
                );
            }
        }
    }

    let mut res = next.run(req).await;
    apply_cors(res.headers_mut(), acao.as_deref());
    apply_security_headers(res.headers_mut(), &path);
    res
}

fn finalize_response(mut res: Response, path: &str, acao: Option<&str>) -> Response {
    apply_cors(res.headers_mut(), acao);
    apply_security_headers(res.headers_mut(), path);
    res
}

fn apply_cors(headers: &mut HeaderMap, acao: Option<&str>) {
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("Content-Type, Authorization"),
    );
    if let Some(o) = acao {
        if let Ok(v) = HeaderValue::from_str(o) {
            headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, v);
            headers.insert(header::VARY, HeaderValue::from_static("Origin"));
        }
    }
}

/// Origin Cloudflare Turnstile serves its widget script and iframe from.
///
/// W9.1: the bot check cannot render without BOTH `script-src` and `frame-src`
/// allowing this exact origin — with no `frame-src` directive the iframe falls
/// back to `default-src 'self'` and is blocked. Deliberately kept to this one
/// origin; `connect-src` stays `'self'` because siteverify is server-side, so
/// the browser never talks to Cloudflare directly.
const TURNSTILE_ORIGIN: &str = "https://challenges.cloudflare.com";

/// Transport hardening for Cloud Run / public HTML+JSON (not protocol logic).
fn apply_security_headers(headers: &mut HeaderMap, path: &str) {
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    // Header name not in axum's typed constants for Permissions-Policy.
    if let Ok(v) = HeaderValue::from_str("camera=(), microphone=(), geolocation=(), payment=()") {
        headers.insert(HeaderName::from_static("permissions-policy"), v);
    }
    // Tight CSP: same-origin API, inline script/style for static lab pages.
    let csp = format!(
        "default-src 'self'; script-src 'self' 'unsafe-inline' {t}; \
         style-src 'self' 'unsafe-inline'; img-src 'self' data: https:; connect-src 'self'; \
         font-src 'self'; frame-src {t}; frame-ancestors 'none'; base-uri 'self'; \
         form-action 'self'",
        t = TURNSTILE_ORIGIN
    );
    if let Ok(v) = HeaderValue::from_str(&csp) {
        headers.insert(header::CONTENT_SECURITY_POLICY, v);
    }
    let cache = if path.starts_with("/artifacts/public/") || path == "/manifest.json" {
        "public, max-age=300"
    } else if path.starts_with("/v1/") {
        "no-store"
    } else {
        // HTML pages
        "no-cache"
    };
    if let Ok(v) = HeaderValue::from_str(cache) {
        headers.insert(header::CACHE_CONTROL, v);
    }
}

fn cors_response(status: StatusCode, acao: Option<&str>, body: Body) -> Response {
    let mut res = Response::builder().status(status).body(body).unwrap();
    apply_cors(res.headers_mut(), acao);
    res
}

fn cors_json(status: StatusCode, acao: Option<&str>, body: Value) -> Response {
    let bytes = serde_json::to_vec(&body).unwrap_or_default();
    let mut res = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(bytes))
        .unwrap();
    apply_cors(res.headers_mut(), acao);
    res
}

fn err_json(status: StatusCode, msg: impl ToString) -> Response {
    (
        status,
        Json(json!({"error": msg.to_string(), "status": "error"})),
    )
        .into_response()
}

fn err_code(status: StatusCode, code: &str, msg: impl ToString) -> Response {
    (
        status,
        Json(json!({"error": msg.to_string(), "status": "error", "code": code})),
    )
        .into_response()
}

async fn read_html(web_dir: &PathBuf, name: &str, fallback: &str) -> Html<String> {
    let path = web_dir.join(name);
    let html = tokio::fs::read_to_string(&path)
        .await
        .unwrap_or_else(|_| fallback.to_string());
    Html(html)
}

async fn page_index(State(s): State<AppState>) -> Html<String> {
    read_html(
        &s.web_dir,
        "index.html",
        "<html><body><h1>rgbmvp verifier</h1><p>missing web/index.html</p></body></html>",
    )
    .await
}

async fn page_demo(State(s): State<AppState>) -> Html<String> {
    read_html(
        &s.web_dir,
        "demo.html",
        "<html><body><h1>/demo</h1><p>missing web/demo.html</p></body></html>",
    )
    .await
}

async fn page_audit(State(s): State<AppState>) -> Html<String> {
    read_html(
        &s.web_dir,
        "audit.html",
        "<html><body><h1>/audit</h1><p>missing web/audit.html</p></body></html>",
    )
    .await
}

async fn page_status(State(s): State<AppState>) -> Html<String> {
    read_html(
        &s.web_dir,
        "status.html",
        "<html><body><h1>/status</h1><p>missing web/status.html</p></body></html>",
    )
    .await
}

/// W9 — public demo swap page. Static; all state comes from `/v1/demo/*`.
async fn page_swap(State(s): State<AppState>) -> Html<String> {
    read_html(
        &s.web_dir,
        "swap.html",
        "<html><body><h1>/swap</h1><p>missing web/swap.html</p></body></html>",
    )
    .await
}

async fn artifact_manifest(State(s): State<AppState>) -> Response {
    serve_artifact(&s, "manifest.json").await
}

async fn artifact_public(State(s): State<AppState>, Path(rest): Path<String>) -> Response {
    let name = rest.trim_start_matches('/');
    if !is_safe_public_artifact_path(name) {
        return err_code(StatusCode::BAD_REQUEST, "bad_path", "bad artifact path");
    }
    serve_artifact(&s, name).await
}

/// Relative path under `artifacts/public/`: no `..`, segments [A-Za-z0-9._-], optional one dir.
fn is_safe_public_artifact_path(name: &str) -> bool {
    if name.is_empty() || name.contains("..") || name.starts_with('/') || name.contains('\\') {
        return false;
    }
    if lab_core::is_safe_path_id(name) {
        return true;
    }
    // e.g. bfa/honest.json
    let parts: Vec<&str> = name.split('/').collect();
    if parts.len() != 2 {
        return false;
    }
    parts.iter().all(|seg| {
        !seg.is_empty()
            && *seg != "."
            && *seg != ".."
            && seg
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    })
}

async fn v1_bfa_samples(State(s): State<AppState>) -> Response {
    // Prefer catalog file; fall back to inline list if missing.
    let p = s.artifacts_dir.join("bfa/index.json");
    match tokio::fs::read(&p).await {
        Ok(b) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(b))
            .unwrap(),
        Err(_) => Json(lab_api::bfa_samples_json()).into_response(),
    }
}

async fn serve_artifact(s: &AppState, name: &str) -> Response {
    let p = s.artifacts_dir.join(name);
    match tokio::fs::read(&p).await {
        Ok(b) => {
            let ct = if name.ends_with(".json") {
                "application/json"
            } else {
                "text/plain; charset=utf-8"
            };
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, ct)
                .body(Body::from(b))
                .unwrap()
        }
        Err(_) => err_json(StatusCode::NOT_FOUND, "artifact not found"),
    }
}

async fn v1_root() -> Response {
    Json(lab_api::root_json()).into_response()
}

async fn v1_phases() -> Response {
    Json(lab_api::phases_json()).into_response()
}

async fn v1_security(State(s): State<AppState>) -> Response {
    Json(lab_api::security_json(
        s.cfg.security.public_read_only,
        is_loopback_bind(&s.cfg.labd_bind),
        s.cfg.security.api_token.is_some(),
    ))
    .into_response()
}

async fn v1_networks() -> Response {
    Json(json!({
        "networks": ["liquid-testnet", "bitcoin-testnet"],
        "default": "liquid-testnet",
        "mainnet": false
    }))
    .into_response()
}

async fn v1_health(State(s): State<AppState>) -> Response {
    let cfg = s.cfg.clone();
    let report = tokio::task::spawn_blocking(move || {
        lab_chain::network_status(&cfg).unwrap_or_else(|e| {
            let mut r = lab_core::HealthReport::phase0_base(cfg.network);
            r.status = "error".into();
            r.checks.push(lab_core::HealthCheck {
                name: "status".into(),
                ok: false,
                detail: Some(e.to_string()),
            });
            r
        })
    })
    .await
    .unwrap_or_else(|e| {
        let mut r = lab_core::HealthReport::phase0_base(s.cfg.network);
        r.status = "error".into();
        r.checks.push(lab_core::HealthCheck {
            name: "join".into(),
            ok: false,
            detail: Some(e.to_string()),
        });
        r
    });
    Json(lab_api::health_json(&report)).into_response()
}

async fn v1_proof(State(s): State<AppState>, Path(id): Path<String>) -> Response {
    if let Err(e) = validate_path_id(&id) {
        return err_code(StatusCode::BAD_REQUEST, "bad_id", e);
    }
    let store = RgbStore::new(&s.cfg.data_dir);
    match store.load_proof(&id) {
        Ok(p) => Json(p).into_response(),
        Err(e) => err_json(StatusCode::NOT_FOUND, e),
    }
}

async fn v1_swaps(State(s): State<AppState>) -> Response {
    let dir = s.cfg.data_dir.clone();
    match tokio::task::spawn_blocking(move || list_swap_ids(&dir)).await {
        Ok(Ok(ids)) => Json(json!({"swaps": ids})).into_response(),
        Ok(Err(e)) => err_json(StatusCode::INTERNAL_SERVER_ERROR, e),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

async fn v1_swap_get(State(s): State<AppState>, Path(id): Path<String>) -> Response {
    if let Err(e) = validate_path_id(&id) {
        return err_code(StatusCode::BAD_REQUEST, "bad_id", e);
    }
    let cfg = s.cfg.clone();
    let id2 = id.clone();
    match tokio::task::spawn_blocking(move || {
        let store = SwapStore::new(&cfg.data_dir);
        store.load(&id2).map(|sess| public_swap_view(&sess, &cfg))
    })
    .await
    {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => err_json(StatusCode::NOT_FOUND, e),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

async fn v1_swap_init(State(s): State<AppState>, body: bytes::Bytes) -> Response {
    let cfg = s.cfg.clone();
    let body = String::from_utf8_lossy(&body).into_owned();
    match tokio::task::spawn_blocking(move || {
        let store = SwapStore::new(&cfg.data_dir);
        handle_swap_init_post(&cfg, &store, &body)
    })
    .await
    {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => err_json(StatusCode::BAD_REQUEST, e),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

async fn v1_swap_action(
    State(s): State<AppState>,
    Path(id): Path<String>,
    body: bytes::Bytes,
) -> Response {
    if let Err(e) = validate_path_id(&id) {
        return err_code(StatusCode::BAD_REQUEST, "bad_id", e);
    }
    let cfg = s.cfg.clone();
    let body = String::from_utf8_lossy(&body).into_owned();
    match tokio::task::spawn_blocking(move || {
        let store = SwapStore::new(&cfg.data_dir);
        handle_swap_action_post(&cfg, &store, &id, &body)
    })
    .await
    {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => err_json(StatusCode::BAD_REQUEST, e),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

async fn v1_demo_wallets(State(s): State<AppState>) -> Response {
    let cfg = s.cfg.clone();
    let balance_board = s.wallet_balance_board.clone();
    match tokio::task::spawn_blocking(move || demo_wallets(&cfg, &balance_board)).await {
        Ok(Ok(v)) => {
            let mut response = Json(v).into_response();
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("no-store"),
            );
            response
        }
        Ok(Err(e)) => err_json(StatusCode::INTERNAL_SERVER_ERROR, e),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

async fn v1_demo_activity(State(s): State<AppState>) -> Response {
    let cfg = s.cfg.clone();
    match tokio::task::spawn_blocking(move || demo_activity(&cfg)).await {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => err_json(StatusCode::INTERNAL_SERVER_ERROR, e),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

async fn v1_rgb_contracts(State(s): State<AppState>) -> Response {
    let cfg = s.cfg.clone();
    match tokio::task::spawn_blocking(move || list_rgb_contracts(&cfg)).await {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => err_json(StatusCode::INTERNAL_SERVER_ERROR, e),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

async fn v1_rgb_plan(State(s): State<AppState>, Path(id): Path<String>) -> Response {
    if let Err(e) = validate_path_id(&id) {
        return err_code(StatusCode::BAD_REQUEST, "bad_id", e);
    }
    let store = RgbStore::new(&s.cfg.data_dir);
    match store.load_transfer(&id) {
        Ok(p) => Json(json!({"plan_id": id, "plan": p})).into_response(),
        Err(e) => err_json(StatusCode::NOT_FOUND, e),
    }
}

async fn v1_rgb_verify(
    State(s): State<AppState>,
    axum::extract::Extension(ClientIp(ip)): axum::extract::Extension<ClientIp>,
    body: bytes::Bytes,
) -> Response {
    if !s.verify_limiter.check(&ip) {
        return err_code(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "verify rate limit exceeded; retry later",
        );
    }
    let cfg = s.cfg.clone();
    let body = String::from_utf8_lossy(&body).into_owned();
    match tokio::task::spawn_blocking(move || {
        let store = RgbStore::new(&cfg.data_dir);
        handle_verify_post(&cfg, &store, &body)
    })
    .await
    {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => err_json(StatusCode::BAD_REQUEST, e),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

async fn v1_rgb_issue(State(s): State<AppState>, body: bytes::Bytes) -> Response {
    let cfg = s.cfg.clone();
    let body = String::from_utf8_lossy(&body).into_owned();
    match tokio::task::spawn_blocking(move || {
        let store = RgbStore::new(&cfg.data_dir);
        handle_rgb_issue_post(&cfg, &store, &body)
    })
    .await
    {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => err_json(StatusCode::BAD_REQUEST, e),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

async fn v1_rgb_transfer(State(s): State<AppState>, body: bytes::Bytes) -> Response {
    let cfg = s.cfg.clone();
    let body = String::from_utf8_lossy(&body).into_owned();
    match tokio::task::spawn_blocking(move || {
        let store = RgbStore::new(&cfg.data_dir);
        handle_rgb_transfer_post(&cfg, &store, &body)
    })
    .await
    {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => err_json(StatusCode::BAD_REQUEST, e),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

const MAX_XFF_ENTRIES: usize = 16;
const MAX_TRUSTED_PROXY_HOPS: usize = 8;

/// Client-IP trust is topology, not a Boolean. `trusted_proxy_hops` is the
/// exact number of proxy-created XFF entries to discard from the right.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ClientIpPolicy {
    trusted_proxy_hops: usize,
}

impl ClientIpPolicy {
    fn from_env() -> Result<Self> {
        Self::from_values(
            std::env::var("LABD_XFF_TRUSTED_HOPS").ok().as_deref(),
            std::env::var("LABD_TRUST_XFF").ok().as_deref(),
        )
    }

    fn from_values(hops: Option<&str>, legacy: Option<&str>) -> Result<Self> {
        let legacy_enabled = legacy
            .map(|v| {
                matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(false);
        anyhow::ensure!(
            !legacy_enabled,
            "LABD_TRUST_XFF is unsafe and no longer supported; set the exact trusted suffix length with LABD_XFF_TRUSTED_HOPS"
        );
        let trusted_proxy_hops = match hops.map(str::trim).filter(|v| !v.is_empty()) {
            Some(v) => v
                .parse::<usize>()
                .with_context(|| format!("LABD_XFF_TRUSTED_HOPS must be an integer, got {v:?}"))?,
            None => 0,
        };
        anyhow::ensure!(
            trusted_proxy_hops <= MAX_TRUSTED_PROXY_HOPS,
            "LABD_XFF_TRUSTED_HOPS must be between 0 and {MAX_TRUSTED_PROXY_HOPS}"
        );
        Ok(Self { trusted_proxy_hops })
    }

    fn description(self) -> String {
        if self.trusted_proxy_hops == 0 {
            "socket peer (X-Forwarded-For ignored)".into()
        } else {
            format!(
                "X-Forwarded-For with {} trusted right-edge hop(s)",
                self.trusted_proxy_hops
            )
        }
    }
}

/// Resolve the client IP used for per-IP demo quotas.
///
/// Without an explicit topology, XFF is ignored and the unspoofable socket peer
/// is used. With `N` trusted proxy hops, discard exactly `N` validated IPs from
/// the right and select the next validated IP. For Google External Application
/// Load Balancers, the suffix is normally `client-ip, load-balancer-ip`, so
/// `N=1` selects the client instead of collapsing quotas onto the balancer.
///
/// Multiple header fields, invalid/non-IP entries, oversized chains, or too few
/// entries fall back to the socket peer. If peer information is unavailable,
/// all such requests share `"unknown"`, which is stricter than minting identities.
fn client_ip(headers: &HeaderMap, peer: Option<SocketAddr>, policy: ClientIpPolicy) -> String {
    let fallback = || {
        peer.map(|p| p.ip().to_string())
            .unwrap_or_else(|| "unknown".to_string())
    };
    if policy.trusted_proxy_hops == 0 {
        return fallback();
    }

    let mut values = headers.get_all("x-forwarded-for").iter();
    let Some(value) = values.next() else {
        return fallback();
    };
    if values.next().is_some() {
        return fallback();
    }
    let Ok(value) = value.to_str() else {
        return fallback();
    };
    let raw: Vec<&str> = value.split(',').map(str::trim).collect();
    if raw.is_empty() || raw.len() > MAX_XFF_ENTRIES || raw.iter().any(|entry| entry.is_empty()) {
        return fallback();
    }
    let Some(index) = raw.len().checked_sub(policy.trusted_proxy_hops + 1) else {
        return fallback();
    };
    // Values left of `index` are outside the trusted suffix and Google warns
    // they may be attacker supplied, including non-IP text. Validate only the
    // selected client and every trusted right-edge proxy entry.
    let trusted: Option<Vec<IpAddr>> = raw[index..]
        .iter()
        .map(|entry| entry.parse().ok())
        .collect();
    let Some(trusted) = trusted else {
        return fallback();
    };
    trusted[0].to_string()
}

fn demo_denial_response(d: &lab_core::DemoDenial) -> Response {
    let sc = StatusCode::from_u16(d.status()).unwrap_or(StatusCode::FORBIDDEN);
    let mut res = (
        sc,
        Json(json!({
            "status": "error",
            "code": d.code(),
            "error": d.message(),
        })),
    )
        .into_response();
    if let Some(secs) = d.retry_after_secs() {
        if let Ok(v) = HeaderValue::from_str(&secs.to_string()) {
            res.headers_mut().insert(header::RETRY_AFTER, v);
        }
    }
    res
}

/// `POST /v1/demo/swap` — bounded public trigger (T1).
///
/// Accepts only a Turnstile token. Every protocol parameter is server-fixed;
/// the swap runs between the lab's own wallets. Returns immediately with the
/// swap id so the UI can poll `GET /v1/swap/{id}`.
async fn v1_demo_swap(
    State(s): State<AppState>,
    axum::extract::Extension(ClientIp(ip)): axum::extract::Extension<ClientIp>,
    body: bytes::Bytes,
) -> Response {
    use lab_core::DemoDenial;

    if !s.demo.enabled() {
        return demo_denial_response(&DemoDenial::Disabled);
    }

    // Bot check first: cheapest rejection, and it must gate the chain reads.
    if s.demo.policy().turnstile_required {
        let token = serde_json::from_slice::<Value>(&body).ok().and_then(|v| {
            v.get("turnstile_token")
                .or_else(|| v.get("token"))
                .and_then(|t| t.as_str())
                .map(|t| t.to_string())
        });
        let ip_for_check = ip.clone();
        let check = tokio::task::spawn_blocking(move || {
            demo_swap::verify_turnstile_blocking(token.as_deref(), Some(&ip_for_check))
        })
        .await
        .unwrap_or(BotCheck::Failed);
        match check {
            BotCheck::Pass => {}
            BotCheck::Missing => return demo_denial_response(&DemoDenial::TurnstileRequired),
            BotCheck::Failed => return demo_denial_response(&DemoDenial::TurnstileFailed),
        }
    }

    // Observe floats (cached; fail-closed when unavailable).
    let cfg = s.cfg.clone();
    let wallets = s.demo_wallets.clone();
    let floats_cache = s.demo_floats.clone();
    let floats = tokio::task::spawn_blocking(move || floats_cache.observe_blocking(&cfg, &wallets))
        .await
        .unwrap_or(None);

    // Admission: quotas, daily cap, concurrency, cooldown, budget, floors.
    if let Err(d) = s.demo.try_admit(&ip, demo_swap::now_epoch(), floats) {
        return demo_denial_response(&d);
    }

    // The worst-case fee reservation must reach persistent storage before any
    // session is created or transaction can be broadcast. If storage is
    // unavailable, release only the unspent in-memory reservation and refuse.
    let persist_cfg = s.cfg.clone();
    let persist_gov = s.demo.clone();
    let persisted =
        tokio::task::spawn_blocking(move || demo_swap::persist_budget(&persist_cfg, &persist_gov))
            .await;
    match persisted {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            s.demo.abort();
            return err_json(StatusCode::SERVICE_UNAVAILABLE, e);
        }
        Err(e) => {
            s.demo.abort();
            return err_json(StatusCode::SERVICE_UNAVAILABLE, e);
        }
    }

    // Admitted — create the session with server-fixed parameters.
    let seq = s
        .demo_seq
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let cfg = s.cfg.clone();
    let wallets = s.demo_wallets.clone();
    let created =
        tokio::task::spawn_blocking(move || demo_swap::create_demo_session(&cfg, &wallets, seq))
            .await;

    let swap_id = match created {
        Ok(Ok(id)) => id,
        Ok(Err(e)) => {
            s.demo.abort();
            if let Err(save_err) = demo_swap::persist_budget(&s.cfg, &s.demo) {
                eprintln!("demo: failed to persist unspent admission abort: {save_err:#}");
            }
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, e);
        }
        Err(e) => {
            s.demo.abort();
            if let Err(save_err) = demo_swap::persist_budget(&s.cfg, &s.demo) {
                eprintln!("demo: failed to persist unspent admission abort: {save_err:#}");
            }
            return err_json(StatusCode::INTERNAL_SERVER_ERROR, e);
        }
    };

    // Drive the swap in the background; the visitor polls GET /v1/swap/{id}.
    {
        let cfg = s.cfg.clone();
        let gov = s.demo.clone();
        let id = swap_id.clone();
        let leg = s.demo.policy().leg_sats;
        let fees = s.demo_fees;
        tokio::task::spawn_blocking(move || {
            match demo_swap::drive_demo_swap_blocking(&cfg, &id, leg, fees) {
                Ok(fee) => gov.finish_with_liability(fee, fees.btc_sweep_fee_sats),
                Err(e) => {
                    eprintln!("demo: swap {id} failed: {e}");
                    // Execution may already have broadcast a funding tx. The
                    // exact fee is unknown, so charge the full reservation.
                    gov.fail_closed();
                }
            }
            // The prior durable state still holds the full reservation, so a
            // failed settlement write remains conservative across restart.
            if let Err(e) = demo_swap::persist_budget(&cfg, &gov) {
                eprintln!("demo: failed to persist budget settlement: {e:#}");
            }
        });
    }

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "accepted",
            "swap_id": swap_id,
            "poll": format!("/v1/swap/{swap_id}"),
            "leg_sats": s.demo.policy().leg_sats,
            "rgb_wrap": lab_core::demo::DEMO_RGB_WRAP,
            "note": "Testnet demo swap between lab wallets. No real value.",
        })),
    )
        .into_response()
}

/// `GET /v1/demo/quota` — public budget/limit visibility.
async fn v1_demo_quota(State(s): State<AppState>) -> Response {
    let floats = s.demo_floats.peek();
    Json(quota_json(&s.demo, floats)).into_response()
}

/// `POST /v1/demo/rgb/run` — anonymous, fixed Issue -> Transfer -> Verify.
async fn v1_demo_rgb_run(
    State(s): State<AppState>,
    axum::extract::Extension(ClientIp(ip)): axum::extract::Extension<ClientIp>,
    body: bytes::Bytes,
) -> Response {
    use lab_core::DemoDenial;

    if !s.rgb_demo.enabled() {
        return demo_denial_response(&DemoDenial::Disabled);
    }

    let parsed: Value = match serde_json::from_slice(&body) {
        Ok(Value::Object(map)) => Value::Object(map),
        _ => {
            return err_code(
                StatusCode::BAD_REQUEST,
                "bad_request",
                "JSON object required",
            )
        }
    };
    let Some(fields) = parsed.as_object() else {
        return err_code(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "JSON object required",
        );
    };
    if fields.keys().any(|key| key != "turnstile_token") {
        return err_code(
            StatusCode::BAD_REQUEST,
            "fixed_parameters",
            "only turnstile_token is accepted; wallets and parameters are server-fixed",
        );
    }
    let token = fields
        .get("turnstile_token")
        .and_then(Value::as_str)
        .map(str::to_string);

    let ip_for_check = ip.clone();
    let check = tokio::task::spawn_blocking(move || {
        demo_swap::verify_turnstile_action_blocking(
            token.as_deref(),
            Some(&ip_for_check),
            demo_swap::RGB_LAB_TURNSTILE_ACTION,
        )
    })
    .await
    .unwrap_or(BotCheck::Failed);
    match check {
        BotCheck::Pass => {}
        BotCheck::Missing => return demo_denial_response(&DemoDenial::TurnstileRequired),
        BotCheck::Failed => return demo_denial_response(&DemoDenial::TurnstileFailed),
    }

    let float_cfg = s.cfg.clone();
    let floats = tokio::task::spawn_blocking(move || crate::rgb_demo::observe_floats(&float_cfg))
        .await
        .ok()
        .and_then(Result::ok);
    if let Err(denial) = s.rgb_demo.try_admit(&ip, demo_swap::now_epoch(), floats) {
        return demo_denial_response(&denial);
    }

    // The full cost reservation is durable before issue writes or broadcast.
    let persist_cfg = s.cfg.clone();
    let persist_gov = s.rgb_demo.clone();
    let persisted = tokio::task::spawn_blocking(move || {
        demo_swap::persist_named_budget(&persist_cfg, &persist_gov, crate::rgb_demo::BUDGET_NAME)
    })
    .await;
    if !matches!(persisted, Ok(Ok(()))) {
        s.rgb_demo.abort();
        return err_code(
            StatusCode::SERVICE_UNAVAILABLE,
            "budget_unavailable",
            "RGB demo budget could not be reserved",
        );
    }

    let run_cfg = s.cfg.clone();
    let result = tokio::task::spawn_blocking(move || crate::rgb_demo::run(&run_cfg)).await;
    let response = match result {
        Ok(Ok(value)) => {
            // The chain helper does not yet expose an exact fee. Charge the full
            // conservative reservation rather than under-accounting a run.
            s.rgb_demo.finish(s.rgb_demo.policy().max_fee_per_swap_sats);
            (StatusCode::OK, Json(value)).into_response()
        }
        Ok(Err(error)) => {
            // A broadcast may already have happened. Unknown outcomes stay
            // charged and are visible on the board for operator follow-up.
            s.rgb_demo.fail_closed();
            err_json(StatusCode::BAD_GATEWAY, error)
        }
        Err(error) => {
            s.rgb_demo.fail_closed();
            err_json(StatusCode::INTERNAL_SERVER_ERROR, error)
        }
    };
    if let Err(error) =
        demo_swap::persist_named_budget(&s.cfg, &s.rgb_demo, crate::rgb_demo::BUDGET_NAME)
    {
        eprintln!("RGB demo: failed to persist settlement: {error:#}");
    }
    response
}

async fn v1_demo_rgb_quota(State(s): State<AppState>) -> Response {
    let p = s.rgb_demo.policy();
    let st = s.rgb_demo.status(demo_swap::now_epoch());
    Json(json!({
        "enabled": p.enabled,
        "turnstile_required": true,
        "turnstile_sitekey": demo_swap::turnstile_sitekey(),
        "turnstile_action": demo_swap::RGB_LAB_TURNSTILE_ACTION,
        "parameters": crate::rgb_demo::fixed_parameters_json(),
        "limits": {
            "daily_cap": p.daily_cap,
            "max_concurrent": p.max_concurrent,
            "min_interval_secs": p.global_min_interval_secs,
            "per_ip_hourly": p.per_ip_hourly,
            "per_ip_daily": p.per_ip_daily
        },
        "budget": {
            "cost_budget_sats": p.fee_budget_sats,
            "cost_accounted_sats": st.fee_spent_sats
                .saturating_add(st.fee_reserved_sats)
                .saturating_add(st.fee_committed_sats),
            "runs_remaining_est": s.rgb_demo.swaps_remaining_in_budget()
        },
        "usage": {
            "in_flight": st.in_flight,
            "runs_today": st.swaps_today,
            "runs_total": st.swaps_total
        }
    }))
    .into_response()
}

async fn v1_audit_bfa(State(s): State<AppState>, body: bytes::Bytes) -> Response {
    let body = String::from_utf8_lossy(&body).into_owned();
    let public_read_only = s.cfg.security.public_read_only;
    match tokio::task::spawn_blocking(move || {
        if public_read_only {
            handle_bfa_audit_post_public(&body)
        } else {
            handle_bfa_audit_post(&body)
        }
    })
    .await
    {
        Ok(Ok(v)) => {
            let status = if v.ok {
                StatusCode::OK
            } else {
                StatusCode::UNPROCESSABLE_ENTITY
            };
            (status, Json(v)).into_response()
        }
        Ok(Err(e)) => err_json(StatusCode::BAD_REQUEST, e),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    /// Repo `web/` directory, resolved from the manifest.
    ///
    /// `cargo test` runs with CWD at the crate root, so a bare "web" path
    /// silently resolves to nothing and the handlers serve their fallback HTML.
    fn repo_web_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../web")
    }

    /// Build state around a config, with demo swaps off unless a test opts in.
    fn state_with(cfg: Config, demo_policy: lab_core::DemoSwapPolicy) -> AppState {
        AppState {
            cfg,
            web_dir: PathBuf::from("web"),
            artifacts_dir: PathBuf::from("artifacts/public"),
            verify_limiter: Arc::new(RateLimiter::new(100, std::time::Duration::from_secs(60))),
            demo: Arc::new(DemoGovernor::new(demo_policy)),
            rgb_demo: Arc::new(DemoGovernor::new(lab_core::DemoSwapPolicy::default())),
            demo_floats: Arc::new(FloatCache::new()),
            wallet_balance_board: Arc::new(WalletBalanceBoard::empty()),
            demo_wallets: DemoWallets {
                alice_btc: "btc-alice".into(),
                bob_lq: "bob".into(),
            },
            demo_fees: DemoFees {
                btc_fee_sats: 800,
                btc_claim_fee_sats: 500,
                btc_sweep_fee_sats: 500,
                lq_fee_sats: 300,
                lq_sweep_fee_sats: 400,
            },
            client_ip_policy: ClientIpPolicy::default(),
            demo_seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    fn test_state() -> AppState {
        let _ = dotenvy::dotenv();
        // Minimal config without full env: use load if possible
        let cfg = Config::load().unwrap_or_else(|_| {
            panic!("Config::load failed in test — set RGBMVP_NETWORK=liquid-testnet")
        });
        state_with(cfg, lab_core::DemoSwapPolicy::default())
    }

    #[tokio::test]
    async fn catalog_and_security_get() {
        let state = test_state();
        let app = router(state);
        for path in ["/v1", "/v1/security", "/v1/phases", "/v1/networks"] {
            let res = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK, "path {path}");
        }
    }

    #[tokio::test]
    async fn public_read_only_blocks_post_without_token() {
        std::env::set_var("LABD_PUBLIC_READ_ONLY", "1");
        std::env::remove_var("LABD_API_TOKEN");
        let mut cfg = Config::load().expect("config");
        cfg.security = lab_core::MutationPolicy::from_env(&cfg.labd_bind);
        let state = state_with(cfg, lab_core::DemoSwapPolicy::default());
        let app = router(state);
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/swap/init")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        std::env::remove_var("LABD_PUBLIC_READ_ONLY");
    }

    #[tokio::test]
    async fn public_bfa_audit_is_compute_only_and_needs_no_bearer() {
        std::env::set_var("LABD_PUBLIC_READ_ONLY", "1");
        std::env::remove_var("LABD_API_TOKEN");
        let mut cfg = Config::load().expect("config");
        cfg.security = lab_core::MutationPolicy::from_env(&cfg.labd_bind);
        let state = state_with(cfg, lab_core::DemoSwapPolicy::default());
        let app = router(state);
        let body = std::fs::read(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../artifacts/public/bfa/honest.json"),
        )
        .expect("public honest BFA fixture");
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/audit/bfa")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        std::env::remove_var("LABD_PUBLIC_READ_ONLY");
    }

    #[tokio::test]
    async fn public_bfa_audit_exemption_is_exact_path_only() {
        std::env::set_var("LABD_PUBLIC_READ_ONLY", "1");
        std::env::remove_var("LABD_API_TOKEN");
        let mut cfg = Config::load().expect("config");
        cfg.security = lab_core::MutationPolicy::from_env(&cfg.labd_bind);
        let state = state_with(cfg, lab_core::DemoSwapPolicy::default());
        let app = router(state);
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/audit/bfa/extra")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        std::env::remove_var("LABD_PUBLIC_READ_ONLY");
    }

    /// With the flag off, the demo trigger must not exist as a public mutation.
    #[tokio::test]
    async fn demo_swap_disabled_by_default() {
        std::env::set_var("LABD_PUBLIC_READ_ONLY", "1");
        std::env::remove_var("LABD_API_TOKEN");
        let mut cfg = Config::load().expect("config");
        cfg.security = lab_core::MutationPolicy::from_env(&cfg.labd_bind);
        // Default policy => demo disabled.
        let state = state_with(cfg, lab_core::DemoSwapPolicy::default());
        let app = router(state);
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/demo/swap")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        // The U4 middleware still guards it (demo exemption is flag-gated), so
        // an unauthenticated POST is refused rather than reaching the handler.
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        std::env::remove_var("LABD_PUBLIC_READ_ONLY");
    }

    /// W9: the public swap page is served, and must not ship key material or a
    /// preimage. It is static, so anything sensitive here would be permanent.
    #[tokio::test]
    async fn public_swap_page_is_served_and_leaks_nothing() {
        let mut state = test_state();
        state.web_dir = repo_web_dir();
        let app = router(state);
        for path in ["/swap", "/swap.html"] {
            let res = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK, "path {path}");
            let body = axum::body::to_bytes(res.into_body(), 512 * 1024)
                .await
                .unwrap();
            let html = String::from_utf8_lossy(&body).to_lowercase();
            assert!(
                html.contains("run a demo swap"),
                "{path} should render the real page, not the fallback"
            );
            // Disclaimers are the point of the page; keep them non-optional.
            assert!(
                html.contains("testnet only"),
                "{path} must state testnet-only"
            );
            for bad in [
                "preimage_hex",
                "mnemonic",
                "wif",
                "xprv",
                "tprv",
                "secret_dir",
            ] {
                assert!(!html.contains(bad), "{path} must not contain {bad}");
            }
            // The page must drive the bounded endpoint, never the arbitrary one.
            assert!(
                html.contains("/v1/demo/swap"),
                "{path} must use the bounded public trigger"
            );
            assert!(
                !html.contains("/v1/swap/init") && !html.contains("/action"),
                "{path} must not call the token-gated operator endpoints"
            );
        }
    }

    /// Quota endpoint is a public GET and must never leak wallet secrets.
    #[tokio::test]
    async fn demo_quota_is_public_and_safe() {
        let state = test_state();
        let app = router(state);
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/v1/demo/quota")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), 64 * 1024)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["enabled"], json!(false));
        assert_eq!(v["rgb_wrap"], json!(false));
        let dump = v.to_string();
        for bad in ["mnemonic", "preimage", "wif", "descriptor"] {
            assert!(!dump.contains(bad), "quota JSON must not expose {bad}");
        }
    }

    #[tokio::test]
    async fn demo_wallet_board_is_uncached_and_leaks_no_watch_material() {
        let state = test_state();
        let app = router(state);
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/v1/demo/wallets")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers()
                .get(header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some("no-store")
        );
        let body = axum::body::to_bytes(res.into_body(), 256 * 1024)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert!(v["wallets"].is_array());
        assert_eq!(v["balance_cache"]["source"], json!("address-registry"));
        let dump = v.to_string().to_ascii_lowercase();
        for bad in [
            "mnemonic",
            "preimage",
            "descriptor",
            "slip77",
            "xpub",
            "tpub",
            "xprv",
            "tprv",
            "/secrets",
        ] {
            assert!(!dump.contains(bad), "wallet JSON must not expose {bad}");
        }
    }

    #[tokio::test]
    async fn public_rgb_card_uses_only_the_fixed_demo_endpoint() {
        let mut state = test_state();
        state.web_dir = repo_web_dir();
        let app = router(state);
        let res = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let html = String::from_utf8_lossy(&body);
        let start = html.find("id=\"public-rgb-lab\"").expect("public RGB card");
        let end = html[start..]
            .find("<div class=\"tabs\"")
            .map(|offset| start + offset)
            .expect("operator tabs after public card");
        let card = &html[start..end];
        assert!(card.contains("Issue → Transfer → Verify"));
        assert!(!card.contains("<input") && !card.contains("<select"));
        assert!(html.contains("/v1/demo/rgb/run"));
        assert!(html.contains("rgbmvp_rgb_lab"));
    }

    /// The demo exemption must be an EXACT path match — no prefix escape.
    #[tokio::test]
    async fn demo_exemption_does_not_leak_to_other_mutations() {
        std::env::set_var("LABD_PUBLIC_READ_ONLY", "1");
        std::env::remove_var("LABD_API_TOKEN");
        let mut cfg = Config::load().expect("config");
        cfg.security = lab_core::MutationPolicy::from_env(&cfg.labd_bind);
        // Demo ENABLED — the exemption is live for /v1/demo/swap only.
        let state = state_with(
            cfg,
            lab_core::DemoSwapPolicy {
                enabled: true,
                ..Default::default()
            },
        );
        let app = router(state);
        // The arbitrary-parameter swap endpoints must still demand a token.
        for path in ["/v1/swap/init", "/v1/rgb/issue", "/v1/rgb/transfer"] {
            let res = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(path)
                        .header("content-type", "application/json")
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                res.status(),
                StatusCode::FORBIDDEN,
                "{path} must stay token-gated even with demo swaps enabled"
            );
        }
        std::env::remove_var("LABD_PUBLIC_READ_ONLY");
    }

    #[tokio::test]
    async fn rgb_demo_exemption_is_exact_and_parameters_are_rejected() {
        std::env::set_var("LABD_PUBLIC_READ_ONLY", "1");
        std::env::remove_var("LABD_API_TOKEN");
        let mut cfg = Config::load().expect("config");
        cfg.security = lab_core::MutationPolicy::from_env(&cfg.labd_bind);
        let mut state = state_with(cfg, lab_core::DemoSwapPolicy::default());
        state.rgb_demo = Arc::new(DemoGovernor::new(lab_core::DemoSwapPolicy {
            enabled: true,
            ..Default::default()
        }));
        let app = router(state);

        let extra = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/demo/rgb/run")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"turnstile_token":"x","amount":1}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(extra.status(), StatusCode::BAD_REQUEST);

        for path in [
            "/v1/demo/rgb/run/extra",
            "/v1/rgb/issue",
            "/v1/rgb/transfer",
            "/v1/rgb/verify",
        ] {
            let res = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(path)
                        .header("content-type", "application/json")
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::FORBIDDEN, "{path}");
        }
        std::env::remove_var("LABD_PUBLIC_READ_ONLY");
    }

    /// Turnstile must gate the handler before any chain read or admission.
    #[tokio::test]
    async fn demo_swap_requires_turnstile_token() {
        std::env::set_var("LABD_PUBLIC_READ_ONLY", "1");
        std::env::set_var("LABD_DEMO_TURNSTILE_SECRET", "test-secret");
        std::env::set_var("LABD_DEMO_TURNSTILE_HOSTNAMES", "demo.example");
        std::env::remove_var("LABD_API_TOKEN");
        let mut cfg = Config::load().expect("config");
        cfg.security = lab_core::MutationPolicy::from_env(&cfg.labd_bind);
        let state = state_with(
            cfg,
            lab_core::DemoSwapPolicy {
                enabled: true,
                turnstile_required: true,
                ..Default::default()
            },
        );
        let app = router(state);
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/demo/swap")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        // No token supplied -> refused before any wallet/chain access.
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        let body = axum::body::to_bytes(res.into_body(), 64 * 1024)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["code"], json!("turnstile_required"));
        std::env::remove_var("LABD_DEMO_TURNSTILE_HOSTNAMES");
        std::env::remove_var("LABD_DEMO_TURNSTILE_SECRET");
        std::env::remove_var("LABD_PUBLIC_READ_ONLY");
    }

    #[test]
    fn client_ip_uses_exact_trusted_suffix_and_validates_the_chain() {
        let peer: SocketAddr = "203.0.113.9:1234".parse().unwrap();
        let mut h = HeaderMap::new();
        h.insert(
            "x-forwarded-for",
            HeaderValue::from_static("198.51.100.66, 192.0.2.10, 192.0.2.20"),
        );
        assert_eq!(
            client_ip(&h, Some(peer), ClientIpPolicy::default()),
            "203.0.113.9",
            "XFF is ignored without an explicit topology"
        );
        assert_eq!(
            client_ip(
                &h,
                Some(peer),
                ClientIpPolicy {
                    trusted_proxy_hops: 1,
                },
            ),
            "192.0.2.10",
            "Google-style client,load-balancer suffix selects next-to-last"
        );
        assert_eq!(
            client_ip(
                &h,
                Some(peer),
                ClientIpPolicy {
                    trusted_proxy_hops: 2,
                },
            ),
            "198.51.100.66"
        );

        h.insert(
            "x-forwarded-for",
            HeaderValue::from_static("attacker-supplied, 192.0.2.10, 192.0.2.20"),
        );
        assert_eq!(
            client_ip(
                &h,
                Some(peer),
                ClientIpPolicy {
                    trusted_proxy_hops: 1,
                },
            ),
            "192.0.2.10",
            "untrusted prefix content must not control resolution"
        );

        h.insert(
            "x-forwarded-for",
            HeaderValue::from_static("2001:db8::10, 2001:db8::20"),
        );
        assert_eq!(
            client_ip(
                &h,
                Some(peer),
                ClientIpPolicy {
                    trusted_proxy_hops: 1,
                },
            ),
            "2001:db8::10"
        );

        for bad in ["not-an-ip, 192.0.2.20", "192.0.2.10,", "192.0.2.20"] {
            let mut headers = HeaderMap::new();
            headers.insert("x-forwarded-for", HeaderValue::from_str(bad).unwrap());
            assert_eq!(
                client_ip(
                    &headers,
                    Some(peer),
                    ClientIpPolicy {
                        trusted_proxy_hops: 1,
                    },
                ),
                "203.0.113.9",
                "bad chain must collapse to the unspoofable peer bucket"
            );
        }

        let mut duplicate = HeaderMap::new();
        duplicate.append(
            "x-forwarded-for",
            HeaderValue::from_static("192.0.2.10, 192.0.2.20"),
        );
        duplicate.append(
            "x-forwarded-for",
            HeaderValue::from_static("192.0.2.30, 192.0.2.40"),
        );
        assert_eq!(
            client_ip(
                &duplicate,
                Some(peer),
                ClientIpPolicy {
                    trusted_proxy_hops: 1,
                },
            ),
            "203.0.113.9"
        );

        let oversized = (0..=MAX_XFF_ENTRIES)
            .map(|i| format!("192.0.2.{}", i + 1))
            .collect::<Vec<_>>()
            .join(",");
        let mut oversized_headers = HeaderMap::new();
        oversized_headers.insert(
            "x-forwarded-for",
            HeaderValue::from_str(&oversized).unwrap(),
        );
        assert_eq!(
            client_ip(
                &oversized_headers,
                Some(peer),
                ClientIpPolicy {
                    trusted_proxy_hops: 1,
                },
            ),
            "203.0.113.9"
        );
        assert_eq!(
            client_ip(&HeaderMap::new(), None, ClientIpPolicy::default()),
            "unknown"
        );
    }

    #[test]
    fn client_ip_policy_rejects_legacy_boolean_and_bad_hop_counts() {
        assert_eq!(
            ClientIpPolicy::from_values(Some("1"), None).unwrap(),
            ClientIpPolicy {
                trusted_proxy_hops: 1
            }
        );
        assert!(ClientIpPolicy::from_values(None, Some("1")).is_err());
        assert!(ClientIpPolicy::from_values(Some("nope"), None).is_err());
        assert!(ClientIpPolicy::from_values(Some("9"), None).is_err());
    }

    #[tokio::test]
    async fn security_headers_on_v1_get() {
        let state = test_state();
        let app = router(state);
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/v1/security")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let h = res.headers();
        assert_eq!(
            h.get(header::X_CONTENT_TYPE_OPTIONS)
                .and_then(|v| v.to_str().ok()),
            Some("nosniff")
        );
        assert_eq!(
            h.get(header::X_FRAME_OPTIONS).and_then(|v| v.to_str().ok()),
            Some("DENY")
        );
        assert_eq!(
            h.get(header::REFERRER_POLICY).and_then(|v| v.to_str().ok()),
            Some("no-referrer")
        );
        let csp = h
            .get(header::CONTENT_SECURITY_POLICY)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(csp.contains("default-src 'self'"), "csp={csp}");
        // W9.1: Turnstile needs exactly this origin in script-src AND frame-src.
        assert!(
            csp.contains(&format!(
                "script-src 'self' 'unsafe-inline' {TURNSTILE_ORIGIN}"
            )),
            "turnstile script origin must be allowed: csp={csp}"
        );
        assert!(
            csp.contains(&format!("frame-src {TURNSTILE_ORIGIN}")),
            "turnstile iframe origin must be allowed: csp={csp}"
        );
        // The relaxation is script/frame only — the page must never be able to
        // call out to a third party directly.
        assert!(
            csp.contains("connect-src 'self';"),
            "connect-src must stay self: csp={csp}"
        );
        assert!(csp.contains("frame-ancestors 'none'"), "csp={csp}");
        assert_eq!(
            h.get(header::CACHE_CONTROL).and_then(|v| v.to_str().ok()),
            Some("no-store")
        );
    }
}
