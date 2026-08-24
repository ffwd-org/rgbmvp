# T1 trusted-proxy and per-IP quota remediation

Status: **implemented and locally tested; live header-chain proof pending**.
Scope: the isolated T1 Bitcoin testnet / Liquid Testnet service. Mainnet remains
out of scope.

## Finding and impact

The former `LABD_TRUST_XFF=1` policy selected the rightmost
`X-Forwarded-For` entry. Google External Application Load Balancers document
that they append `client-ip,load-balancer-ip`; therefore the rightmost entry is
the balancer and the client is normally next-to-last. The old policy could put
many visitors in one proxy quota bucket. A Boolean also could not describe a
longer proxy chain, making the control silently topology-dependent.

## Remediation

`LABD_XFF_TRUSTED_HOPS=N` now declares the exact number of trusted
proxy-created entries at the right edge of XFF. Resolution discards those `N`
entries and selects the next IP to the left. The T1 Google profile uses `N=1`.
The legacy Boolean is rejected at startup when enabled.

The resolver additionally:

- ignores XFF unless an explicit nonzero topology is configured;
- requires the selected client and every trusted suffix entry to parse as an
  IPv4 or IPv6 address, while ignoring attacker-controlled prefix contents;
- rejects duplicate header fields, empty tokens, chains over 16 entries, and
  configurations over eight trusted hops;
- rejects chains without enough entries for the configured suffix; and
- falls back to the unspoofable socket peer, or the shared `unknown` bucket if
  no peer exists, instead of creating attacker-selected quota identities.

Google cautions that values preceding its appended suffix are not verified.
Selecting from the right by the configured trusted count avoids relying on a
spoofable leftmost value while still identifying the client adjacent to the
trusted suffix. See the official
[External Application Load Balancer XFF contract](https://cloud.google.com/load-balancing/docs/https#x-forwarded-for_header)
and [IPv6 client-IP guidance](https://cloud.google.com/load-balancing/docs/ipv6#client-ip-header).

## Deployment and acceptance gate

The hop count is safe only when the network path is fixed and direct paths
cannot supply a different chain. Before enabling T1:

1. send test requests from two controlled public IPv4 clients and, where
   available, one IPv6 client through the intended public origin;
2. capture a temporary redacted diagnostic containing only the entry count and
   selected index, never the full production header;
3. confirm Google supplies `client-ip,load-balancer-ip` at the trusted suffix
   and that `LABD_XFF_TRUSTED_HOPS=1` distinguishes the controlled clients;
4. send a pre-populated spoofed XFF value and confirm it is ignored to the left
   of the Google-appended suffix;
5. test missing, invalid, duplicate, oversized, and short chains and confirm
   they share the socket-peer quota identity; and
6. remove the diagnostic and retain the topology and quota evidence.

If the public path adds another trusted proxy entry, change the hop count only
after repeating this proof. Do not infer it from a single request and do not
enable T1 while the observed chain differs from the documented topology.

## Verification contract

Unit tests cover disabled trust, Google-style next-to-last selection, longer
trusted suffixes, malformed and short-chain fallback, rejection of the legacy
Boolean, and invalid hop-count configuration. This is code-level remediation;
the live Cloud Run/load-balancer header-chain proof remains a release gate.
