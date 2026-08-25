# T1 fee-budget crash-durability remediation

Status: **implemented and locally tested; deployment crash drill pending**.
Scope: the separate, opt-in T1 **Bitcoin testnet / Liquid Testnet** profile.
Mainnet remains out of scope.

## Finding and impact

The original governor reserved the worst-case BTC fee in memory during
admission, but wrote `demo_budget.json` only after the background driver
finished. A process crash after a transaction broadcast could therefore erase
the reservation and admit more swaps than the persistent 28,000-sat ceiling.
Malformed state was also treated like missing state, silently restarting the
counter at zero.

This was a real testnet cost-control defect. The earlier live evidence in
[T1_FIRST_SWAP.md](./T1_FIRST_SWAP.md) proved only that a *completed* swap's
spend survived restart; it did not exercise a crash during admission or
execution.

## Remediated invariant

Every new admission is allowed only when:

```text
fee_spent_sats + fee_reserved_sats + fee_committed_sats
  + next_reservation_sats <= fee_budget_sats
```

- `spent`: known BTC funding and claim/refund fees.
- `reserved`: the full worst case for a currently admitted swap.
- `committed`: conservative charges that must not return to the available
  budget: recovered crash reservations, unknown partial execution, and the
  later exit-sweep fee.

No demo session is created and no transaction can broadcast until the full
reservation is durably written. If that write fails, the endpoint returns 503.
Only a pre-execution failure proven not to have broadcast may release a
reservation. Driver errors retain the entire reservation.

The default reservation is now **1,800 sats**: 800 funding + 500 claim/refund +
500 exit sweep. Startup refuses a configuration whose reservation is below
those configured fees. The 28,000-sat ceiling therefore permits at most
**15 admissions**, leaving 1,000 sats of accounting headroom. Batching may make
the actual sweep cheaper, but the ceiling does not depend on that saving.

Settlement records actual and committed fees in full, without clamping them to
the reservation. An unexpected overrun can therefore make accounted expenditure
exceed the ceiling; that accurate state reports zero remaining swaps and blocks
all later admission. See
[T1_FEE_UNDER_RESERVATION_REMEDIATION.md](./T1_FEE_UNDER_RESERVATION_REMEDIATION.md).

## Durable write and recovery protocol

Budget mutations are serialized in-process and committed as whole JSON files:

1. write and `sync_all` `demo_budget.pending.json`;
2. write and `sync_all` `demo_budget.json`;
3. remove the pending record and sync the directory where supported.

The protocol deliberately does not depend on a temporary-file rename. Cloud
Storage FUSE is not a conventional POSIX filesystem, and its file-system
semantics differ from local storage ([Google Cloud Storage FUSE overview](https://docs.cloud.google.com/storage/docs/cloud-storage-fuse/overview)).
Cloud Storage replaces individual objects atomically, while multi-object
operations need not be atomic ([Cloud Storage objects](https://docs.cloud.google.com/storage/docs/objects)).
A remaining pending record is therefore
the authoritative recovery point. A malformed or unreadable pending or primary
record blocks startup; it never falls back to zero.

The same volume stores swap sessions, which contain the private preimage needed
to settle or refund a swap. `deploy/cloudrun-demo.yaml` therefore mounts Cloud
Storage FUSE with
`uid=65534,gid=65534,file-mode=600,dir-mode=700`, matching the Debian `nobody`
runtime identity declared by `Dockerfile.public`. Session persistence verifies
an effective mode of exactly `0600` and fails before execution if the mount is
insecure. It sets the creation mode and verifies it before writing any preimage
bytes; it never relies on `chmod`, because Cloud Storage FUSE controls modes
globally rather than per object.

At startup, a persisted in-flight reservation is converted to `committed` and
the normalized state is durably saved before the service accepts traffic. Old
snapshots remain readable through Serde defaults; any old `fee_reserved_sats`
is conservatively committed.

| Interruption point | Recovery behavior |
|---|---|
| Before pending is durable | No session or broadcast has started; request fails |
| Pending durable, primary incomplete | Pending record is authoritative |
| Admission durable, before or after broadcast | Restart commits the full reservation |
| Driver complete, settlement write fails | Prior durable reservation remains charged |
| Pending or primary malformed/unreadable | Startup refuses T1 |
| Persistent storage unavailable | Admission returns 503 before session creation |

Deleting both budget files outside the application is not distinguishable from
first initialization. Restrict bucket IAM to the runtime identity and operators,
keep object versioning enabled, and alert on unexpected object deletion.

## Verification and release gate

Local tests cover corrupt primary state, corrupt pending state, interrupted
commit recovery, durable admission followed by simulated restart, conservative
unknown execution, and the retained sweep liability. The public quota contract
exposes `fee_spent_sats`, `fee_reserved_sats`, `fee_committed_sats`, and their
`fee_accounted_sats` sum.

Before the separate T1 profile is deployed, an operator-approved acceptance
drill must:

1. keep `LABD_DEMO_SWAPS=0`, back up/version the existing budget object, and
   deploy the candidate to the isolated T1 service;
2. confirm startup rejects a deliberately malformed budget object, then restore
   its valid version;
3. enable T1, admit one operator-triggered testnet swap, and confirm the durable
   file contains a 1,800-sat reservation before the session advances;
4. terminate the instance during that swap, restart it, and confirm the
   reservation appears as `fee_committed_sats` and remaining capacity does not
   increase;
5. complete/refund and sweep all demo exits, then keep the two-week soak bounded
   by the quota endpoint and bucket-deletion alerts.

This document records code-level remediation, not live Cloud Run acceptance.
Turnstile browser-pass proof and the operator-approved crash drill remain
release gates.
