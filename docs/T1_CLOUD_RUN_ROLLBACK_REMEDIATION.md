# T1 Cloud Run rollback target remediation

Status: **implemented and locally tested; live rollback drill pending**.
Scope: the isolated T1 Bitcoin testnet / Liquid Testnet service. Mainnet remains
out of scope.

## Finding and impact

T1 deploys a Cloud Run service whose manifest identity is `rgbmvp-demo`.
The former full-rollback command applied `deploy/cloudrun.yaml`, whose
`metadata.name` is `rgbmvp-public`. Cloud Run YAML replacement acts on the
service named by the manifest, so that command created or updated the separate
publication service and left the live T1 service unchanged. Only the
`LABD_DEMO_SWAPS=0` kill switch targeted T1 correctly.

## Remediation

`deploy/cloudrun-demo-freeze.yaml` is now the sole full-rollback manifest for
T1. It deliberately retains `metadata.name: rgbmvp-demo`, but creates a
read-only revision with:

- no `LABD_DEMO_SWAPS` or mutation token;
- no wallet, exit-seed, Turnstile, or persistent-data mounts;
- no trusted-proxy or refund-watcher configuration;
- the no-role `rgbmvp-public-run` identity instead of `rgbmvp-demo-run`;
- zero minimum instances and the same testnet-only application image.

The runbook explicitly sends 100% of traffic to the new latest revision after
both the kill switch and full replacement. This matters because Cloud Run can
preserve an existing traffic split across deployments. Traffic migration is
not instantaneous and in-flight requests may finish, so operators must verify
the quota endpoint reports `enabled: false`.

The publication service `rgbmvp-public` remains independent and is neither a
rollback mechanism nor a deletion target. See Google's Cloud Run documentation
for [YAML service replacement](https://cloud.google.com/run/docs/deploying),
[revision rollback and traffic routing](https://cloud.google.com/run/docs/rollouts-rollbacks-traffic-migration),
and [service deletion](https://cloud.google.com/run/docs/managing/services).
The freeze changes the active template; it does not erase older zero-traffic
revisions or delete the retained bucket and secrets.

## Safe incident sequence

1. Update `rgbmvp-demo` with `LABD_DEMO_SWAPS=0`, route traffic to latest, and
   verify `/v1/demo/quota` reports `enabled: false`.
2. Let in-flight work finish or refund, sweep every controlled exit, and verify
   all four balances are zero. Removing custody first can strand outputs.
3. Render `deploy/cloudrun-demo-freeze.yaml` with the reviewed image tag, assert
   its service name is `rgbmvp-demo`, replace the service, and route 100% of
   traffic to latest.
4. Inspect the active template: no volumes, secret references, demo flag, or
   T1 runtime identity. Repeat the read-only security smoke and retain revision
   and traffic evidence.
5. Revoke IAM or delete retained secrets/bucket only as a separate approved
   cleanup after recovery evidence is complete.

If signing material is believed compromised, step 3 may precede recovery, but
the incident record must state that outputs can be stranded and retain the old
secret versions for an approved recovery process.

## Verification contract

`tests/test_deploy_profiles.py` pins both T1 manifests to `rgbmvp-demo`, keeps
the publication profile at `rgbmvp-public`, and rejects mutation, secret,
volume, or privileged-identity configuration in the T1 freeze profile. A live
operator drill remains required before calling rollback proven in Cloud Run.
