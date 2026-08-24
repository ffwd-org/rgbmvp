//! Legacy handwritten TCP HTTP/1.1 labd (LABD_HTTP=legacy).
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use lab_core::{
    cors_allow_origin, is_mutation_method, is_safe_path_id, validate_path_id, AuthDecision, Config,
    RateLimiter,
};
use lab_rgb::storage::RgbStore;
use lab_rgb::swap::SwapStore;

use crate::http_api::{
    demo_activity, demo_wallets, handle_bfa_audit_post, handle_bfa_audit_post_public,
    handle_rgb_issue_post, handle_rgb_transfer_post, handle_swap_action_post,
    handle_swap_init_post, handle_verify_post, list_rgb_contracts, list_swap_ids, public_swap_view,
};

pub(crate) fn serve_labd_legacy(cfg: &Config, bind: &str) -> Result<()> {
    let listener = TcpListener::bind(bind).with_context(|| format!("bind {bind}"))?;
    let sec = &cfg.security;
    eprintln!("labd listening on http://{bind}");
    eprintln!(
        "  U4 security: public_read_only={} loopback_bind={} token_configured={} max_body={}",
        sec.public_read_only,
        lab_core::is_loopback_bind(bind),
        sec.api_token.is_some(),
        sec.max_body_bytes
    );
    eprintln!("  GET  /                 lab console (Issue · Transfer · Verify · Swap)");
    eprintln!("  GET  /demo             read-only board");
    eprintln!("  GET  /audit            BFA audit UI");
    eprintln!("  GET  /v1               API catalog");
    eprintln!("  GET  /v1/health · /v1/phases · /v1/networks · /v1/security");
    eprintln!("  GET  /v1/proofs/{{id}} · /v1/swaps · /v1/swap/{{id}}");
    eprintln!("  GET  /v1/demo/wallets · /v1/demo/activity");
    eprintln!("  GET  /v1/rgb/contracts · /v1/rgb/plans/{{id}}");
    if sec.public_read_only {
        eprintln!(
            "  POST (mutations)       DISABLED unless Authorization: Bearer <LABD_API_TOKEN>"
        );
    } else {
        eprintln!("  POST /v1/rgb/issue · transfer · verify");
        eprintln!("  POST /v1/swap/init · /v1/swap/{{id}}/action · /v1/audit/bfa");
    }

    let web_dir = PathBuf::from(std::env::var("LABD_WEB_DIR").unwrap_or_else(|_| "web".into()));
    let artifacts_dir = PathBuf::from(
        std::env::var("LABD_ARTIFACTS_DIR").unwrap_or_else(|_| "artifacts/public".into()),
    );
    let store = RgbStore::new(&cfg.data_dir);
    let swap_store = SwapStore::new(&cfg.data_dir);
    let verify_limiter = Arc::new(RateLimiter::from_env_verify());
    eprintln!("  GET  /status · /artifacts/public/*  (public evidence)");
    eprintln!(
        "  verify rate limit: {}/min per peer (LABD_VERIFY_RATE_LIMIT)",
        std::env::var("LABD_VERIFY_RATE_LIMIT").unwrap_or_else(|_| "30".into())
    );

    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(s) => s,
            Err(_) => continue,
        };
        let peer = stream
            .peer_addr()
            .map(|a| a.ip().to_string())
            .unwrap_or_else(|_| "unknown".into());
        let mut buf = vec![0u8; sec.max_body_bytes.saturating_add(8192).min(2 * 1024 * 1024)];
        let n = match stream.read(&mut buf) {
            Ok(n) => n,
            Err(_) => continue,
        };
        let req = String::from_utf8_lossy(&buf[..n]);
        let mut lines = req.lines();
        let start = lines.next().unwrap_or("");
        let mut parts = start.split_whitespace();
        let method = parts.next().unwrap_or("GET");
        // strip query string
        let path_raw = parts.next().unwrap_or("/");
        let path = path_raw.split('?').next().unwrap_or(path_raw);

        // Parse headers of interest
        let mut content_length: Option<usize> = None;
        let mut authorization: Option<String> = None;
        let mut origin: Option<String> = None;
        for line in lines.by_ref() {
            if line.is_empty() {
                break;
            }
            if let Some((k, v)) = line.split_once(':') {
                let k = k.trim().to_ascii_lowercase();
                let v = v.trim();
                match k.as_str() {
                    "content-length" => content_length = v.parse().ok(),
                    "authorization" => authorization = Some(v.to_string()),
                    "origin" => origin = Some(v.to_string()),
                    _ => {}
                }
            }
        }
        let acao = cors_allow_origin(sec, origin.as_deref());

        // Body size gate
        if let Some(cl) = content_length {
            if cl > sec.max_body_bytes {
                let body = serde_json::to_vec(&serde_json::json!({
                    "error": "payload too large",
                    "status": "error",
                    "code": "body_too_large",
                    "max_body_bytes": sec.max_body_bytes
                }))
                .unwrap_or_default();
                write_http_response(
                    &mut stream,
                    "413 Payload Too Large",
                    "application/json",
                    &body,
                    acao.as_deref(),
                );
                continue;
            }
        }

        // U4 mutation gate
        if is_mutation_method(method) && path != "/v1/audit/bfa" {
            match sec.authorize_mutation(authorization.as_deref()) {
                AuthDecision::Allow => {}
                AuthDecision::Deny {
                    status,
                    code,
                    message,
                } => {
                    let status_line = match status {
                        401 => "401 Unauthorized",
                        403 => "403 Forbidden",
                        _ => "403 Forbidden",
                    };
                    let body = serde_json::to_vec(&serde_json::json!({
                        "error": message,
                        "status": "error",
                        "code": code,
                    }))
                    .unwrap_or_default();
                    write_http_response(
                        &mut stream,
                        status_line,
                        "application/json",
                        &body,
                        acao.as_deref(),
                    );
                    continue;
                }
            }
        }

        // CORS preflight for browser tools
        let (status, content_type, body) = if method == "OPTIONS" {
            ("204 No Content", "text/plain", Vec::new())
        } else if method == "GET" && path == "/v1/security" {
            let j = serde_json::to_vec_pretty(&lab_api::security_json(
                sec.public_read_only,
                lab_core::is_loopback_bind(bind),
                sec.api_token.is_some(),
            ))
            .unwrap();
            ("200 OK", "application/json", j)
        } else if method == "GET" && (path == "/" || path == "/index.html") {
            let html = fs::read_to_string(web_dir.join("index.html")).unwrap_or_else(|_| {
                "<html><body><h1>rgbmvp verifier</h1><p>missing web/index.html</p></body></html>"
                    .into()
            });
            ("200 OK", "text/html; charset=utf-8", html.into_bytes())
        } else if method == "GET" && (path == "/demo" || path == "/demo.html") {
            let html = fs::read_to_string(web_dir.join("demo.html")).unwrap_or_else(|_| {
                "<html><body><h1>/demo</h1><p>missing web/demo.html</p></body></html>".into()
            });
            ("200 OK", "text/html; charset=utf-8", html.into_bytes())
        } else if method == "GET" && (path == "/audit" || path == "/audit.html") {
            let html = fs::read_to_string(web_dir.join("audit.html")).unwrap_or_else(|_| {
                "<html><body><h1>/audit</h1><p>missing web/audit.html</p></body></html>".into()
            });
            ("200 OK", "text/html; charset=utf-8", html.into_bytes())
        } else if method == "GET" && (path == "/status" || path == "/status.html") {
            let html = fs::read_to_string(web_dir.join("status.html")).unwrap_or_else(|_| {
                "<html><body><h1>/status</h1><p>missing web/status.html</p></body></html>".into()
            });
            ("200 OK", "text/html; charset=utf-8", html.into_bytes())
        } else if method == "GET"
            && (path == "/artifacts/public/manifest.json"
                || path == "/manifest.json"
                || path.starts_with("/artifacts/public/"))
        {
            let rel = path
                .trim_start_matches("/artifacts/public/")
                .trim_start_matches('/');
            let name = if path == "/manifest.json" || path.ends_with("manifest.json") {
                "manifest.json"
            } else if path.ends_with("s3-rgbmvp-live.json") {
                "s3-rgbmvp-live.json"
            } else if is_safe_path_id(rel) {
                rel
            } else if !rel.contains("..")
                && rel.split('/').count() == 2
                && rel.split('/').all(|seg| {
                    !seg.is_empty()
                        && seg
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
                })
            {
                // e.g. bfa/honest.json
                rel
            } else {
                ""
            };
            if name.is_empty() || name.contains("..") {
                (
                    "400 Bad Request",
                    "application/json",
                    br#"{"error":"bad artifact path","status":"error"}"#.to_vec(),
                )
            } else {
                let p = artifacts_dir.join(name);
                match fs::read(&p) {
                    Ok(b) => {
                        let ct = if name.ends_with(".json") {
                            "application/json"
                        } else {
                            "text/plain; charset=utf-8"
                        };
                        ("200 OK", ct, b)
                    }
                    Err(_) => (
                        "404 Not Found",
                        "application/json",
                        br#"{"error":"artifact not found","status":"error"}"#.to_vec(),
                    ),
                }
            }
        } else if method == "GET" && path == "/v1" {
            let j = serde_json::to_vec_pretty(&lab_api::root_json()).unwrap();
            ("200 OK", "application/json", j)
        } else if method == "GET" && path == "/v1/phases" {
            let j = serde_json::to_vec_pretty(&lab_api::phases_json()).unwrap();
            ("200 OK", "application/json", j)
        } else if method == "GET" && path == "/v1/health" {
            let report = lab_chain::network_status(cfg).unwrap_or_else(|e| {
                let mut r = lab_core::HealthReport::phase0_base(cfg.network);
                r.status = "error".into();
                r.checks.push(lab_core::HealthCheck {
                    name: "status".into(),
                    ok: false,
                    detail: Some(e.to_string()),
                });
                r
            });
            let j = serde_json::to_vec_pretty(&lab_api::health_json(&report)).unwrap();
            ("200 OK", "application/json", j)
        } else if method == "GET" && path == "/v1/networks" {
            let j = serde_json::to_vec_pretty(&serde_json::json!({
                "networks": ["liquid-testnet", "bitcoin-testnet"],
                "default": "liquid-testnet",
                "mainnet": false
            }))
            .unwrap();
            ("200 OK", "application/json", j)
        } else if method == "GET" && path.starts_with("/v1/proofs/") {
            let id = path.trim_start_matches("/v1/proofs/");
            if let Err(e) = validate_path_id(id) {
                (
                    "400 Bad Request",
                    "application/json",
                    serde_json::to_vec(&serde_json::json!({"error": e.to_string(), "code": "bad_id", "status": "error"})).unwrap(),
                )
            } else {
                match store.load_proof(id) {
                    Ok(p) => (
                        "200 OK",
                        "application/json",
                        serde_json::to_vec_pretty(&p).unwrap(),
                    ),
                    Err(e) => (
                        "404 Not Found",
                        "application/json",
                        serde_json::to_vec(&serde_json::json!({"error": e.to_string()})).unwrap(),
                    ),
                }
            }
        } else if method == "GET" && path == "/v1/swaps" {
            match list_swap_ids(&cfg.data_dir) {
                Ok(ids) => (
                    "200 OK",
                    "application/json",
                    serde_json::to_vec_pretty(&serde_json::json!({ "swaps": ids })).unwrap(),
                ),
                Err(e) => (
                    "500 Internal Server Error",
                    "application/json",
                    serde_json::to_vec(&serde_json::json!({"error": e.to_string()})).unwrap(),
                ),
            }
        } else if method == "GET" && path.starts_with("/v1/swap/") && !path.contains("/action") {
            let id = path.trim_start_matches("/v1/swap/");
            // strip trailing slash
            let id = id.trim_end_matches('/');
            if let Err(e) = validate_path_id(id) {
                (
                    "400 Bad Request",
                    "application/json",
                    serde_json::to_vec(&serde_json::json!({"error": e.to_string(), "code": "bad_id", "status": "error"})).unwrap(),
                )
            } else {
                match swap_store.load(id) {
                    Ok(s) => {
                        let public = public_swap_view(&s, cfg);
                        (
                            "200 OK",
                            "application/json",
                            serde_json::to_vec_pretty(&public).unwrap(),
                        )
                    }
                    Err(e) => (
                        "404 Not Found",
                        "application/json",
                        serde_json::to_vec(
                            &serde_json::json!({"error": e.to_string(), "status": "error"}),
                        )
                        .unwrap(),
                    ),
                }
            }
        } else if method == "POST" && path == "/v1/swap/init" {
            let body_start = req.find("\r\n\r\n").map(|i| i + 4).unwrap_or(req.len());
            let body_str = &req[body_start..];
            match handle_swap_init_post(cfg, &swap_store, body_str) {
                Ok(v) => (
                    "200 OK",
                    "application/json",
                    serde_json::to_vec_pretty(&v).unwrap(),
                ),
                Err(e) => (
                    "400 Bad Request",
                    "application/json",
                    serde_json::to_vec(
                        &serde_json::json!({"error": e.to_string(), "status": "error"}),
                    )
                    .unwrap(),
                ),
            }
        } else if method == "POST" && path.starts_with("/v1/swap/") && path.ends_with("/action") {
            // /v1/swap/{id}/action
            let mid = path
                .trim_start_matches("/v1/swap/")
                .trim_end_matches("/action")
                .trim_end_matches('/');
            let body_start = req.find("\r\n\r\n").map(|i| i + 4).unwrap_or(req.len());
            let body_str = &req[body_start..];
            if let Err(e) = validate_path_id(mid) {
                (
                    "400 Bad Request",
                    "application/json",
                    serde_json::to_vec(&serde_json::json!({"error": e.to_string(), "code": "bad_id", "status": "error"})).unwrap(),
                )
            } else {
                match handle_swap_action_post(cfg, &swap_store, mid, body_str) {
                    Ok(v) => (
                        "200 OK",
                        "application/json",
                        serde_json::to_vec_pretty(&v).unwrap(),
                    ),
                    Err(e) => (
                        "400 Bad Request",
                        "application/json",
                        serde_json::to_vec(
                            &serde_json::json!({"error": e.to_string(), "status": "error"}),
                        )
                        .unwrap(),
                    ),
                }
            }
        } else if method == "GET" && path == "/v1/demo/wallets" {
            match demo_wallets(cfg) {
                Ok(v) => (
                    "200 OK",
                    "application/json",
                    serde_json::to_vec_pretty(&v).unwrap(),
                ),
                Err(e) => (
                    "500 Internal Server Error",
                    "application/json",
                    serde_json::to_vec(&serde_json::json!({"error": e.to_string()})).unwrap(),
                ),
            }
        } else if method == "GET" && path == "/v1/demo/activity" {
            match demo_activity(cfg) {
                Ok(v) => (
                    "200 OK",
                    "application/json",
                    serde_json::to_vec_pretty(&v).unwrap(),
                ),
                Err(e) => (
                    "500 Internal Server Error",
                    "application/json",
                    serde_json::to_vec(&serde_json::json!({"error": e.to_string()})).unwrap(),
                ),
            }
        } else if method == "GET" && path == "/v1/rgb/contracts" {
            match list_rgb_contracts(cfg) {
                Ok(v) => (
                    "200 OK",
                    "application/json",
                    serde_json::to_vec_pretty(&v).unwrap(),
                ),
                Err(e) => (
                    "500 Internal Server Error",
                    "application/json",
                    serde_json::to_vec(
                        &serde_json::json!({"error": e.to_string(), "status": "error"}),
                    )
                    .unwrap(),
                ),
            }
        } else if method == "GET" && path.starts_with("/v1/rgb/plans/") {
            let id = path.trim_start_matches("/v1/rgb/plans/");
            if let Err(e) = validate_path_id(id) {
                (
                    "400 Bad Request",
                    "application/json",
                    serde_json::to_vec(&serde_json::json!({"error": e.to_string(), "code": "bad_id", "status": "error"})).unwrap(),
                )
            } else {
                match store.load_transfer(id) {
                    Ok(p) => (
                        "200 OK",
                        "application/json",
                        serde_json::to_vec_pretty(&serde_json::json!({"plan_id": id, "plan": p}))
                            .unwrap(),
                    ),
                    Err(e) => (
                        "404 Not Found",
                        "application/json",
                        serde_json::to_vec(
                            &serde_json::json!({"error": e.to_string(), "status": "error"}),
                        )
                        .unwrap(),
                    ),
                }
            }
        } else if method == "POST" && path == "/v1/rgb/verify" {
            // Rate-limit verify (Esplora-backed) per peer IP — U4 public soak.
            if !verify_limiter.check(&peer) {
                (
                    "429 Too Many Requests",
                    "application/json",
                    serde_json::to_vec(&serde_json::json!({
                        "error": "verify rate limit exceeded; retry later",
                        "status": "error",
                        "code": "rate_limited"
                    }))
                    .unwrap(),
                )
            } else {
                let body_start = req.find("\r\n\r\n").map(|i| i + 4).unwrap_or(req.len());
                let body_str = &req[body_start..];
                match handle_verify_post(cfg, &store, body_str) {
                    Ok(v) => (
                        "200 OK",
                        "application/json",
                        serde_json::to_vec_pretty(&v).unwrap(),
                    ),
                    Err(e) => (
                        "400 Bad Request",
                        "application/json",
                        serde_json::to_vec(
                            &serde_json::json!({"error": e.to_string(), "status": "error"}),
                        )
                        .unwrap(),
                    ),
                }
            }
        } else if method == "POST" && path == "/v1/rgb/issue" {
            let body_start = req.find("\r\n\r\n").map(|i| i + 4).unwrap_or(req.len());
            let body_str = &req[body_start..];
            match handle_rgb_issue_post(cfg, &store, body_str) {
                Ok(v) => (
                    "200 OK",
                    "application/json",
                    serde_json::to_vec_pretty(&v).unwrap(),
                ),
                Err(e) => (
                    "400 Bad Request",
                    "application/json",
                    serde_json::to_vec(
                        &serde_json::json!({"error": e.to_string(), "status": "error"}),
                    )
                    .unwrap(),
                ),
            }
        } else if method == "POST" && path == "/v1/rgb/transfer" {
            let body_start = req.find("\r\n\r\n").map(|i| i + 4).unwrap_or(req.len());
            let body_str = &req[body_start..];
            match handle_rgb_transfer_post(cfg, &store, body_str) {
                Ok(v) => (
                    "200 OK",
                    "application/json",
                    serde_json::to_vec_pretty(&v).unwrap(),
                ),
                Err(e) => (
                    "400 Bad Request",
                    "application/json",
                    serde_json::to_vec(
                        &serde_json::json!({"error": e.to_string(), "status": "error"}),
                    )
                    .unwrap(),
                ),
            }
        } else if method == "GET" && path == "/v1/audit/bfa/samples" {
            let p = artifacts_dir.join("bfa/index.json");
            match fs::read(&p) {
                Ok(b) => ("200 OK", "application/json", b),
                Err(_) => (
                    "200 OK",
                    "application/json",
                    serde_json::to_vec_pretty(&lab_api::bfa_samples_json()).unwrap(),
                ),
            }
        } else if method == "POST" && path == "/v1/audit/bfa" {
            let body_start = req.find("\r\n\r\n").map(|i| i + 4).unwrap_or(req.len());
            let body_str = &req[body_start..];
            let result = if sec.public_read_only {
                handle_bfa_audit_post_public(body_str)
            } else {
                handle_bfa_audit_post(body_str)
            };
            match result {
                Ok(v) => {
                    let code = if v.ok {
                        "200 OK"
                    } else {
                        "422 Unprocessable Entity"
                    };
                    (
                        code,
                        "application/json",
                        serde_json::to_vec_pretty(&v).unwrap(),
                    )
                }
                Err(e) => (
                    "400 Bad Request",
                    "application/json",
                    serde_json::to_vec(
                        &serde_json::json!({"error": e.to_string(), "status": "error"}),
                    )
                    .unwrap(),
                ),
            }
        } else {
            (
                "404 Not Found",
                "application/json",
                br#"{"error":"not found","status":"error"}"#.to_vec(),
            )
        };

        write_http_response(&mut stream, status, content_type, &body, acao.as_deref());
    }
    Ok(())
}

fn write_http_response(
    stream: &mut impl Write,
    status: &str,
    content_type: &str,
    body: &[u8],
    allow_origin: Option<&str>,
) {
    let mut headers = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Headers: Content-Type, Authorization\r\nConnection: close\r\n",
        body.len()
    );
    if let Some(o) = allow_origin {
        headers.push_str(&format!("Access-Control-Allow-Origin: {o}\r\n"));
        headers.push_str("Vary: Origin\r\n");
    }
    headers.push_str("\r\n");
    let _ = stream.write_all(headers.as_bytes());
    let _ = stream.write_all(body);
}
