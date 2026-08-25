# Public deploy sketches (U4)

**Do not** expose wallets, WIFs, or Elements RPC to the Internet.  
Public posture: **read-only** labd + static UI. Full protocol demos stay on the operator machine.

See [docs/U4_PUBLIC_HOSTING.md](../docs/U4_PUBLIC_HOSTING.md).
For the complete administrator and rollback sequence, use
[docs/PUBLISH_TUTORIAL.md](../docs/PUBLISH_TUTORIAL.md).

## Recommended split

| Surface | Host | Cost |
|---------|------|------|
| Static console / board | **Vercel** Hobby | $0 |
| Optional live `GET /v1/*` | **GCP Cloud Run** | ~$0 scale-to-zero |

Publication freeze for this lab: **single Cloud Run origin first** (no Vercel as primary).
Vercel secrets below are optional/secondary.

---

## GitHub Actions — variables & secrets

Workflows: [`.github/workflows/deploy-cloudrun.yml`](../.github/workflows/deploy-cloudrun.yml),
[`.github/workflows/deploy-vercel.yml`](../.github/workflows/deploy-vercel.yml).  
GitHub **Environment:** `public-demo` (jobs bind to it; vars/secrets may be **repository** or **environment** scoped).

### Repository / environment **variables** (`vars.*`)

| Name | Required | Example | Used by |
|------|----------|---------|---------|
| `GCP_PROJECT_ID` | **yes** (Cloud Run) | `silicon-pointer-490721-r0` | `deploy-cloudrun` job gate + image/project |
| `GCP_REGION` | **yes** (Cloud Run) | `us-central1` | Artifact Registry host + `gcloud run deploy` |
| `GCP_AR_REPO` | **yes** (Cloud Run) | `rgbmvp` | Image path `…/$GCP_AR_REPO/rgbmvp-public:sha` |
| `GCP_RUNTIME_SERVICE_ACCOUNT` | optional | `rgbmvp-public-run@PROJECT.iam.gserviceaccount.com` | Container identity (default name if unset) |
| `LABD_CORS_ORIGINS` | optional | `https://your-origin.example` | Only if a **second** browser origin needs CORS; omit for single Cloud Run origin |

```bash
# Repository-scoped (recommended minimum)
gh variable set GCP_PROJECT_ID --body "silicon-pointer-490721-r0"
gh variable set GCP_REGION --body "us-central1"
gh variable set GCP_AR_REPO --body "rgbmvp"
# optional:
# gh variable set LABD_CORS_ORIGINS --body "https://YOUR_PUBLIC_ORIGIN"

# Or environment-scoped (same names under public-demo):
# gh variable set GCP_PROJECT_ID --env public-demo --body "silicon-pointer-490721-r0"
```

### Repository / environment **secrets** (`secrets.*`)

| Name | Required | Example shape | Used by |
|------|----------|---------------|---------|
| `GCP_WORKLOAD_IDENTITY_PROVIDER` | **yes** (Cloud Run) | `projects/PROJECT_NUMBER/locations/global/workloadIdentityPools/POOL/providers/PROVIDER` | `google-github-actions/auth` |
| `GCP_SERVICE_ACCOUNT` | **yes** (Cloud Run) | `rgbmvp-deploy@PROJECT_ID.iam.gserviceaccount.com` | **Deploy** SA (OIDC; push AR + Cloud Run admin) — not the runtime SA |
| `VERCEL_TOKEN` | yes (Vercel only) | Vercel personal/token | `deploy-vercel` gate + action |
| `VERCEL_ORG_ID` | yes (Vercel only) | `team_…` / org id | `amondnet/vercel-action` |
| `VERCEL_PROJECT_ID` | yes (Vercel only) | `prj_…` | `amondnet/vercel-action` |

Built-in (no config): `GITHUB_TOKEN` — CI gitleaks, GHCR push in `docker-public.yml`.

```bash
# Cloud Run OIDC (values from GCP WIF setup — never commit)
gh secret set GCP_WORKLOAD_IDENTITY_PROVIDER --body "projects/…/locations/global/workloadIdentityPools/…/providers/…"
gh secret set GCP_SERVICE_ACCOUNT --body "rgbmvp-deploy@silicon-pointer-490721-r0.iam.gserviceaccount.com"

# Optional Vercel (skip while Cloud Run is the only public origin)
# gh secret set VERCEL_TOKEN --body "…"
# gh secret set VERCEL_ORG_ID --body "…"
# gh secret set VERCEL_PROJECT_ID --body "…"

# Environment-scoped alternative:
# gh secret set GCP_WORKLOAD_IDENTITY_PROVIDER --env public-demo
# gh secret set GCP_SERVICE_ACCOUNT --env public-demo
```

### Gate behavior

| Workflow | Runs when | Skips when |
|----------|-----------|------------|
| `deploy-cloudrun` | `vars.GCP_PROJECT_ID` non-empty **and** both GCP secrets set | no project var, or missing WIF/deploy SA (soft skip) |
| `deploy-vercel` | `secrets.VERCEL_TOKEN` set | `VERCEL_TOKEN` empty (soft skip in step) |

With `GCP_PROJECT_ID` set but without OIDC secrets, the freeze workflow **soft-skips** deploy (profile remains in-repo).

### Verify on the repo

```bash
gh variable list
gh secret list
gh variable list --env public-demo
gh secret list --env public-demo
```

---

## 1. Vercel (static)

```bash
# From repo root (requires vercel CLI: npm i -g vercel)
cp deploy/vercel.json ./vercel.json   # or link
vercel                  # preview
vercel --prod           # production
```

Point the browser at static pages only, or set a future `window.LABD_API` to Cloud Run.
If Vercel is ever re-enabled, set `LABD_CORS_ORIGINS` on Cloud Run to that origin.

## 2. Cloud Run — publication freeze (first revision)

Authoritative sketch: [`deploy/cloudrun.yaml`](./cloudrun.yaml).  
CI deploy: [`.github/workflows/deploy-cloudrun.yml`](../.github/workflows/deploy-cloudrun.yml).

| Setting | Freeze value |
|---------|----------------|
| Service | `rgbmvp-public` |
| Authentication | **public** (`--allow-unauthenticated`) |
| Ingress | **all** |
| Min / max instances | **0 / 1** |
| CPU / memory | **1 / 512 MiB** |
| Runtime service account | **dedicated, no project roles** (`rgbmvp-public-run@…`) |
| Env | `LABD_PUBLIC_READ_ONLY=1`, `RGBMVP_NETWORK=liquid-testnet` (+ web/artifacts paths, rate limit) |
| Forbidden | `LABD_API_TOKEN`, wallet mounts, secret volumes, privileged default compute SA |

**Two service accounts (do not collapse):**

| Account | Role |
|---------|------|
| `rgbmvp-deploy@…` | GitHub OIDC deploy (AR write, Cloud Run admin) — **secret** `GCP_SERVICE_ACCOUNT` |
| `rgbmvp-public-run@…` | Runtime identity of the container — **no** project IAM roles for freeze |

```bash
export PROJECT=silicon-pointer-490721-r0   # or your project
export REGION=us-central1

gcloud config set project "$PROJECT"
gcloud services enable run.googleapis.com cloudbuild.googleapis.com artifactregistry.googleapis.com iam.googleapis.com

# One-time Artifact Registry repo
gcloud artifacts repositories create rgbmvp \
  --repository-format=docker --location="$REGION" || true

# One-time runtime SA (no roles bound)
gcloud iam service-accounts create rgbmvp-public-run \
  --display-name="rgbmvp public Cloud Run runtime (no privileges)" || true

IMAGE="${REGION}-docker.pkg.dev/${PROJECT}/rgbmvp/rgbmvp-public:latest"
gcloud builds submit --tag "$IMAGE" -f Dockerfile.public .

gcloud run deploy rgbmvp-public \
  --image="$IMAGE" \
  --region="$REGION" \
  --project="$PROJECT" \
  --allow-unauthenticated \
  --ingress=all \
  --min-instances=0 \
  --max-instances=1 \
  --cpu=1 \
  --memory=512Mi \
  --service-account="rgbmvp-public-run@${PROJECT}.iam.gserviceaccount.com" \
  --execution-environment=gen2 \
  --set-env-vars="LABD_PUBLIC_READ_ONLY=1,RGBMVP_NETWORK=liquid-testnet,LABD_WEB_DIR=/app/web,LABD_ARTIFACTS_DIR=/app/artifacts/public,RGBMVP_DATA_DIR=/tmp/rgbmvp-public,LABD_VERIFY_RATE_LIMIT=30" \
  --clear-secrets
```

Budget alert: set a $1–5/month budget in GCP Billing.

## 3. Local smoke (public mode)

```bash
export LABD_PUBLIC_READ_ONLY=1
export LABD_CORS_ORIGINS=http://127.0.0.1:8080
export LABD_BIND=127.0.0.1:8080
cargo run -p lab-cli -- serve

curl -s http://127.0.0.1:8080/v1/security | jq .
# POST without token → 403
curl -s -X POST http://127.0.0.1:8080/v1/swap/init -d '{}' | jq .
```

## 4. Modal.com

Not used for the public site. Optional later for ephemeral regtest jobs only.

---

## 5. T1 demo swap deployment (operator runbook)

Deploys `deploy/cloudrun-demo.yaml` — the **only** profile where the public can
trigger a state change. It holds spendable **testnet** keys. Read
[`docs/TESTNET_PUBLIC_SWAPS.md`](../docs/TESTNET_PUBLIC_SWAPS.md) first.

**Rollback:** first disable admission on `rgbmvp-demo`. After active sessions
are finished/refunded and all exits are swept, replace that same service with
`deploy/cloudrun-demo-freeze.yaml`. The publication manifest
`deploy/cloudrun.yaml` names the separate `rgbmvp-public` service and cannot
roll back T1.

Run these yourself and paste the output back; none need `sudo`.

```bash
export PROJECT=silicon-pointer-490721-r0     # your project
export REGION=us-central1
export BUCKET="${PROJECT}-rgbmvp-demo-data"
export IMAGE_TAG=REVIEWED_SHA                  # same reviewed image as T1
# Exact public DNS name(s), comma-separated; no scheme/path/wildcard.
export TURNSTILE_HOSTNAMES=rgbmvp-demo.example.com
```

### 5.1 Enable APIs and create the runtime identity

```bash
gcloud services enable run.googleapis.com secretmanager.googleapis.com \
  storage.googleapis.com --project "$PROJECT"

gcloud iam service-accounts create rgbmvp-demo-run \
  --display-name="rgbmvp T1 demo runtime (testnet keys)" --project "$PROJECT"
```

The rollback profile switches to `rgbmvp-public-run`. Before enabling T1,
confirm that account exists, that the deployer may act as it, and that it has
no project, demo-bucket, or demo-secret access. Create it with no roles if this
project has not deployed the publication freeze before:

```bash
gcloud iam service-accounts describe \
  "rgbmvp-public-run@${PROJECT}.iam.gserviceaccount.com" --project "$PROJECT"
# If absent only:
gcloud iam service-accounts create rgbmvp-public-run \
  --display-name="rgbmvp read-only runtime (no privileges)" --project "$PROJECT"
```

### 5.2 Persistent state bucket (W4)

Admission is fail-closed until its 1,800-sat worst-case reservation is durable.
The data mount is therefore mandatory; corrupt or unavailable budget state
refuses T1 startup rather than resetting the ceiling. Object versioning makes
operator deletion or a bad external write recoverable. See
[`docs/T1_FEE_BUDGET_REMEDIATION.md`](../docs/T1_FEE_BUDGET_REMEDIATION.md).

The application uses a synced pending object followed by a synced primary
object; it does not rely on rename semantics. This matches Cloud Storage FUSE's
documented non-POSIX behavior, but the isolated T1 deployment still requires
the interruption drill in the remediation record before public exposure.

```bash
gcloud storage buckets create "gs://${BUCKET}" \
  --project "$PROJECT" --location "$REGION" --uniform-bucket-level-access
gcloud storage buckets update "gs://${BUCKET}" --versioning

gcloud storage buckets add-iam-policy-binding "gs://${BUCKET}" \
  --member="serviceAccount:rgbmvp-demo-run@${PROJECT}.iam.gserviceaccount.com" \
  --role=roles/storage.objectAdmin
```

### 5.3 Wallet and exit-key secrets (W3)

The demo signs with **btc-alice** (WIF), **bob** (mnemonic), and a dedicated
256-bit root seed for the four HTLC exit keys. Startup refuses to run if any is
not mounted on a public bind.

> Pipe from your local files — do **not** paste key material into shell history.
> These are testnet keys with a small float, but treat them as real anyway.

```bash
gcloud secrets create rgbmvp-demo-btc-alice-wif --project "$PROJECT" \
  --replication-policy=automatic
gcloud secrets versions add rgbmvp-demo-btc-alice-wif --project "$PROJECT" \
  --data-file=.rgbmvp/wallets/btc-alice/wif

gcloud secrets create rgbmvp-demo-bob-mnemonic --project "$PROJECT" \
  --replication-policy=automatic
gcloud secrets versions add rgbmvp-demo-bob-mnemonic --project "$PROJECT" \
  --data-file=.rgbmvp/wallets/bob/mnemonic

# Generate the demo-exit root without placing it in a shell variable or file.
# Never reuse a wallet seed or a production/mainnet secret here.
openssl rand -hex 32 | gcloud secrets create rgbmvp-demo-exit-seed \
  --project "$PROJECT" --replication-policy=automatic --data-file=-

# Turnstile secret (from the Cloudflare dashboard).
printf '%s' "$TURNSTILE_SECRET" | gcloud secrets create rgbmvp-demo-turnstile-secret \
  --project "$PROJECT" --replication-policy=automatic --data-file=-

for S in rgbmvp-demo-btc-alice-wif rgbmvp-demo-bob-mnemonic rgbmvp-demo-exit-seed rgbmvp-demo-turnstile-secret; do
  gcloud secrets add-iam-policy-binding "$S" --project "$PROJECT" \
    --member="serviceAccount:rgbmvp-demo-run@${PROJECT}.iam.gserviceaccount.com" \
    --role=roles/secretmanager.secretAccessor
done
```

### 5.3.1 Liquid watch-only balance bundle

`GET /v1/demo/wallets` publishes aggregate **testnet L-BTC** balances for the
five predefined Liquid wallets. Blockstream cannot calculate confidential
Liquid amounts from an address alone, so labd synchronizes them server-side
with LWK/Electrum. The input is a dedicated watch bundle containing xpub
derivation plus SLIP77 blinding material. It can reveal these testnet wallet
amounts but cannot sign.

Do **not** upload `fixtures/testnet_wallets.json`: it contains public fixture
mnemonics and would give the runtime unnecessary signing material. Build the
bundle from the already-derived local descriptor files, verify every primary
address against the addresses-only registry, and stream it directly to Secret
Manager without writing another copy:

```bash
for NAME in alice bob carol lab0 maker; do
  FILE=".rgbmvp/wallets/${NAME}/descriptor"
  test -s "$FILE"
  ! grep -Eiq 'xprv|tprv|uprv|vprv' "$FILE"
  LOCAL_ADDR=$(./target/debug/rgbmvp wallet address --name "$NAME" --index 0 | jq -r .address)
  REGISTRY_ADDR=$(jq -r --arg name "$NAME" '.wallets[] | select(.name == $name) | .address_0' \
    .rgbmvp/wallet_registry.json)
  test "$LOCAL_ADDR" = "$REGISTRY_ADDR"
done

gcloud secrets create rgbmvp-demo-liquid-watch-bundle --project "$PROJECT" \
  --replication-policy=automatic 2>/dev/null || true

jq -n \
  --rawfile alice .rgbmvp/wallets/alice/descriptor \
  --rawfile bob .rgbmvp/wallets/bob/descriptor \
  --rawfile carol .rgbmvp/wallets/carol/descriptor \
  --rawfile lab0 .rgbmvp/wallets/lab0/descriptor \
  --rawfile maker .rgbmvp/wallets/maker/descriptor \
  '{version:1,network:"liquid-testnet",wallets:[
    {name:"alice",descriptor:($alice|rtrimstr("\n"))},
    {name:"bob",descriptor:($bob|rtrimstr("\n"))},
    {name:"carol",descriptor:($carol|rtrimstr("\n"))},
    {name:"lab0",descriptor:($lab0|rtrimstr("\n"))},
    {name:"maker",descriptor:($maker|rtrimstr("\n"))}
  ]}' | gcloud secrets versions add rgbmvp-demo-liquid-watch-bundle \
    --project "$PROJECT" --data-file=-

gcloud secrets add-iam-policy-binding rgbmvp-demo-liquid-watch-bundle \
  --project "$PROJECT" \
  --member="serviceAccount:rgbmvp-demo-run@${PROJECT}.iam.gserviceaccount.com" \
  --role=roles/secretmanager.secretAccessor
```

`deploy/cloudrun-demo.yaml` pins reviewed bundle version `1` at
`/secrets-watch/liquid-watch.json`. That path is deliberately excluded from
`RGBMVP_SECRET_DIR`, so custody/signing resolution cannot consume it. The board
caches successful scans for 120 seconds and may display an explicitly marked
stale value for at most 15 minutes; T1 admission uses a separate fresh,
fail-closed cache.

### 5.4 Deploy

Edit `deploy/cloudrun-demo.yaml`, replacing `PROJECT`, `REGION`, `TAG`,
`TURNSTILE_SITEKEY`, and `TURNSTILE_HOSTNAMES`, then verify that the latter is
the exact DNS hostname configured on the Turnstile widget. The widget requests
the fixed action `rgbmvp_demo_swap`; Siteverify must echo both that action and
one configured hostname or admission fails closed. Schemes, ports, paths, and
wildcards are rejected at startup.

Then deploy:

```bash
gcloud run services replace deploy/cloudrun-demo.yaml \
  --project "$PROJECT" --region "$REGION"

gcloud run services add-iam-policy-binding rgbmvp-demo \
  --project "$PROJECT" --region "$REGION" \
  --member=allUsers --role=roles/run.invoker
```

### 5.5 Post-deploy verification (run BEFORE announcing)

```bash
URL=$(gcloud run services describe rgbmvp-demo --project "$PROJECT" \
  --region "$REGION" --format='value(status.url)')

# U4 posture retained
curl -s "$URL/v1/security" | jq '.public_read_only'          # expect true
# Arbitrary-parameter mutations still refused
curl -s -o /dev/null -w '%{http_code}\n' -X POST "$URL/v1/swap/init" -d '{}'   # expect 403
# Demo quota visible, budget intact
curl -s "$URL/v1/demo/quota" | jq '{enabled, leg_sats, rgb_wrap, budget, floats}'
# Five Liquid balances are LWK-synchronized and carry freshness metadata.
curl -s "$URL/v1/demo/wallets" | jq '{balance_cache, liquid:[.wallets[] | select(.chain == "liquid-testnet") | {name,lbtc_sats,balance_status,balance_as_of_epoch}]}'
# Demo trigger rejects a missing bot token
curl -s -X POST "$URL/v1/demo/swap" -H 'content-type: application/json' -d '{}' | jq '.code'
#   expect "turnstile_required"
```

Then confirm in the logs that custody passed:

```bash
gcloud run services logs read rgbmvp-demo --project "$PROJECT" --region "$REGION" \
  --limit=50 | grep -E 'T1 (custody|demo|refund)'
```

Expect `T1 custody OK`, `T1 demo swaps: ENABLED`, `T1 refund watcher`. If you
see `custody preflight failed`, the service is intentionally refusing to sign —
fix the mount before proceeding.

### 5.6 Kill switch

First response: flip the flag off on the **T1 service** and explicitly route
all traffic to that new revision. Cloud Run traffic changes are not
instantaneous; requests already in flight may complete, so verify the quota
endpoint before treating admission as stopped.

```bash
gcloud run services update rgbmvp-demo --project "$PROJECT" --region "$REGION" \
  --update-env-vars=LABD_DEMO_SWAPS=0
gcloud run services update-traffic rgbmvp-demo --project "$PROJECT" \
  --region "$REGION" --to-latest

URL=$(gcloud run services describe rgbmvp-demo --project "$PROJECT" \
  --region "$REGION" --format='value(status.url)')
curl -fsS "$URL/v1/demo/quota" | jq -e '.enabled == false'
```

Do not remove custody from an active watcher: finish or refund every active
`demo-*` session, sweep all four exits, and independently confirm their
balances are zero. If key compromise requires an immediate freeze, accept and
record that exits may need recovery with retained secret versions.

Full rollback then replaces **`rgbmvp-demo`**, not `rgbmvp-public`. Render the
same-service freeze profile with the reviewed image tag, assert its target, and
route 100% of traffic to the new revision even if a prior traffic split exists:

```bash
sed -e "s/PROJECT/${PROJECT}/g" -e "s/REGION/${REGION}/g" \
  -e "s/TAG/${IMAGE_TAG}/g" deploy/cloudrun-demo-freeze.yaml \
  > /tmp/rgbmvp-demo-freeze.yaml
grep -q '^  name: rgbmvp-demo$' /tmp/rgbmvp-demo-freeze.yaml

gcloud run services replace /tmp/rgbmvp-demo-freeze.yaml \
  --project "$PROJECT" --region "$REGION"
gcloud run services update-traffic rgbmvp-demo --project "$PROJECT" \
  --region "$REGION" --to-latest
```

Verify `metadata.name=rgbmvp-demo`, the no-role `rgbmvp-public-run` service
account, no volumes or secret references, and no demo flag in the active
template. Then repeat the read-only security smoke:

```bash
gcloud run services describe rgbmvp-demo --project "$PROJECT" --region "$REGION" \
  --format='yaml(metadata.name,spec.template.spec.serviceAccountName,spec.template.spec.volumes,spec.template.spec.containers[0].env,status.traffic)'
curl -fsS "$URL/v1/security" | jq -e '.public_read_only == true'
curl -sS -o /dev/null -w '%{http_code}\n' -X POST "$URL/v1/demo/swap" \
  -H 'content-type: application/json' -d '{}'
# expect 404; also confirm the latest revision receives 100% traffic
```

This changes the active revision of `rgbmvp-demo`; it does not delete the
independent `rgbmvp-public` service, old zero-traffic revisions, or the retained
T1 bucket/secrets. Remove or revoke those only as a separately approved cleanup
after recovery evidence is complete.

### 5.7 Key rotation

First disable T1, finish or refund every active demo session, sweep all four
current exit addresses, and independently verify their balances are zero.
Rotating earlier makes the current seed unavailable to the watcher and strands
its outputs. Retain the prior Secret Manager version until that check passes.

```bash
gcloud secrets versions add rgbmvp-demo-bob-mnemonic --project "$PROJECT" \
  --data-file=/path/to/new/mnemonic
openssl rand -hex 32 | gcloud secrets versions add rgbmvp-demo-exit-seed \
  --project "$PROJECT" --data-file=-
gcloud run services update rgbmvp-demo --project "$PROJECT" --region "$REGION"  # restart to remount
```

Rotate after the soak, and immediately if the container is ever suspected
compromised. Old wallets keep only a small float by design.
