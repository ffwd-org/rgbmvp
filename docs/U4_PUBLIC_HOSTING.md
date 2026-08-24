# U4 — Public hosting security foundation

**Status: IMPLEMENTED** (code + deploy sketches) · **Date:** 2026-07-23  
**Roadmap:** [ROADMAP_NEXT.md](./ROADMAP_NEXT.md) · Deploy: [deploy/README.md](../deploy/README.md)

## Why U4

Protocol work (S3, C2, C4) can run on **operator localhost + public testnet RPCs**.  
Putting **labd** on the Internet without a gate risks:

- Unauthenticated POSTs (issue / fund / claim / swap)
- Open Docker Elements RPC (historically `0.0.0.0:7042`)
- Wildcard CORS
- Path traversal via free-form ids
- Accidental mainnet labels

U4 is **ops/security**, not new RGB math. P3 stays closed as a localhost console; U4 is the gate before any public bind.

## Policy (accepted)

| Control | Behavior |
|---------|----------|
| **Public surface** | Static pages + safe `GET /v1/*`; compute-only `POST /v1/audit/bfa`; optionally exact fixed demo paths `/v1/demo/swap` and `/v1/demo/rgb/run` |
| **Mutations** | Arbitrary `POST` requires `Authorization: Bearer <LABD_API_TOKEN>` in public mode. Exact demo paths are independently flag-gated and enforce Turnstile, fixed parameters, quotas, float floors, and separate durable budgets |
| **BFA audit** | Exact `POST /v1/audit/bfa` is non-mutating; public mode requires `witness_tx_hex` on every mint and never invokes RPC or a subprocess |
| **Loopback operator** | `127.0.0.1` + no token → mutations allowed (dev default) |
| **CORS** | Allowlist via `LABD_CORS_ORIGINS`; **no `*`** in public read-only mode |
| **Body limit** | `LABD_MAX_BODY_BYTES` (default 65536) |
| **Path ids** | `[A-Za-z0-9._~-]{1,128}` for proofs/plans/swaps |
| **Mainnet** | `RGBMVP_NETWORK` containing mainnet rejected at config load |
| **Docker RPC** | Host ports bound to `127.0.0.1` only (`docker-compose.yml`) |

## Environment

| Variable | Meaning |
|----------|---------|
| `LABD_PUBLIC_READ_ONLY` / `PUBLIC_READ_ONLY` | `1`/`true` → public posture |
| `LABD_API_TOKEN` | Bearer secret for mutations |
| `LABD_CORS_ORIGINS` | Comma-separated origins |
| `LABD_MAX_BODY_BYTES` | Max body size |
| `LABD_BIND` / `PORT` | Bind address; Cloud Run uses `PORT` → `0.0.0.0:$PORT` |
| `LABD_WEB_DIR` | Static web root (image: `/app/web`) |

## Local smoke

```bash
export LABD_PUBLIC_READ_ONLY=1
export LABD_BIND=127.0.0.1:8080
cargo run -p lab-cli -- serve

curl -s http://127.0.0.1:8080/v1/security | jq .
curl -s -o /dev/null -w "%{http_code}\n" -X POST http://127.0.0.1:8080/v1/swap/init -d '{}'
# expect 403

export LABD_API_TOKEN=dev-token
# restart serve, then:
curl -s -X POST http://127.0.0.1:8080/v1/rgb/verify \
  -H "Authorization: Bearer dev-token" -H "Content-Type: application/json" \
  -d '{"plan_id":"x","txid":"y"}'   # may 400 on payload, but not 403
```

## Containers & deploy

| Artifact | Role |
|----------|------|
| [`Dockerfile.public`](../Dockerfile.public) | Read-only labd + `web/` · no wallets |
| [`deploy/cloudrun.yaml`](../deploy/cloudrun.yaml) | Cloud Run service sketch |
| [`deploy/vercel.json`](../deploy/vercel.json) | Static Vercel sketch |
| [`deploy/README.md`](../deploy/README.md) | Step-by-step commands |

**Recommended:** Vercel static ($0) + optional Cloud Run GET API (scale-to-zero).  
**Not recommended:** Modal as primary site; wallets in the cloud; public Elements RPC.

## Acceptance

| Check | Pass |
|-------|------|
| `GET /v1/security` reports `public_read_only` | yes |
| Unauthenticated `POST` in public mode | 403 |
| Non-loopback without token | mutations 403 |
| Invalid path id `../x` | 400 |
| Oversized body | 413 |
| Compose Elements port on host | `127.0.0.1` only |
| Unit tests `lab-core::security` | green |

## Explicitly out of U4

- Full Axum rewrite  
- Multi-tenant auth / OAuth  
- Hosting hot wallets or Simplicity regtest on the public Internet  
- Mainnet  

## Content / CI / launch

See **[PUBLIC_LAUNCH.md](./PUBLIC_LAUNCH.md)** for Phase 4 artifacts+CI and Phase 5 soak/announce.

The proof-first browser refresh is locally validated under the unchanged U4
security posture. See [UI_REFRESH.md](./UI_REFRESH.md) and its active
[rollback plan](./UI_ROLLBACK_PLAN.md). This does not mean a public deployment
or soak has occurred.

## Next

- Enable deploy secrets (Vercel / Cloud Run OIDC)  
- 24–48h GET-only soak on first public URL  
- Independent review before marketing the URL  
