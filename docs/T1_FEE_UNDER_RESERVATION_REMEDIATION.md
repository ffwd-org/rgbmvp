# T1 fee under-reservation remediation

Status: **implemented and locally tested; independent review pending**.
Scope: the opt-in T1 Bitcoin testnet / Liquid Testnet profile. Mainnet remains
out of scope.

## Finding

T1 originally allowed configured Bitcoin transaction fees to exceed
`LABD_DEMO_MAX_FEE_SATS` after logging only a warning. The governor also settled
`fee_spent_sats` with `min(actual, reserved)`. Together, a bad configuration or
unexpected runtime fee could make persistent accounting lower than expenditure
and allow later admissions against fictitious headroom.

## Remediation

Two independent controls now fail closed:

1. **Startup configuration gate.** Before custody initialization or serving
   requests, labd sums the configured BTC funding, claim/refund, and exit-sweep
   fees using `u128` arithmetic. If that requirement exceeds
   `LABD_DEMO_MAX_FEE_SATS`, T1 startup returns an error. The wider arithmetic
   also rejects a sum that would overflow `u64`.
2. **Truthful runtime settlement.** `DemoGovernor::finish_with_liability`
   releases the prior reservation but adds the complete reported actual fee and
   complete future liability with saturating arithmetic. It never clamps either
   value to the reservation. If an unforeseen overrun takes accounted spend
   beyond the ceiling, remaining capacity becomes zero and all later admissions
   fail with `FeeBudgetExhausted`.

The default configuration remains 1,800 sats: 800 funding + 500 claim/refund +
500 exit sweep. The admission reservation is still a prevention control; full
runtime recording is defense in depth and preserves operator-visible truth if
an assumption changes.

## Verification

Regression tests establish that:

- exactly covered configuration passes;
- a one-satoshi under-reservation fails;
- an overflowing configured fee sum fails even at a `u64::MAX` reservation;
- an actual fee plus sweep liability above the reservation is recorded in full;
- the overrun reports zero remaining swaps and rejects the next admission.

Operator acceptance for the isolated T1 profile must include one deliberate
under-reserved configuration and confirm labd exits before binding its public
listener. Do not weaken this error to a warning and do not use a larger reserve
to conceal an unexplained runtime overrun; investigate the transaction fee
source first.
