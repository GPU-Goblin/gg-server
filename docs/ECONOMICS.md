# Economic assessment

This document reports measurements and the assumptions that go with them, separately. It
**establishes no economic advantage** for an open network: that is precisely what the figures
below do not show.

## Chosen indicator

The full cost of an **accepted result**: the execution's computation, the verification's, the
work lost by attempts that did not complete, and the credits spent. Neither reported FLOPS nor the
number of workers, which say nothing about what a result costs.

## Measurements (2026-09-17)

Protocol: `cargo test -p gg-control --test load --release -- --ignored --nocapture`, release
profile, server and workers in the same process, local PostgreSQL 17, numeric profile
`cpu-f32-flex-v1`, `audit_rate = 0`. Hardware: 32 logical threads, Linux 7.0. Each row aggregates
one sample per task; the spread is given by p50/p95.

| Size | Tasks | Workers | Execution p50 | Verification p50 | **Full cost per accepted result** | Verification overhead | Throughput |
|---|---|---|---|---|---|---|---|
| matmul 128³ | 64 | 8 | 0.000 s | 0.000 s | 0.001 s | 29 % | 60.9 results/s |
| matmul 512³ | 32 | 8 | 0.007 s | 0.007 s | **0.015 s** | **89 %** | 22.3 results/s |
| matmul 1024³ | 16 | 8 | 0.040 s | 0.036 s | **0.075 s** | **88 %** | 11.2 results/s |

Task assignment: p50 from 9 to 19 ms, p95 from 39 to 51 ms. No lost work in these three
measurements (0 rejected submissions, one attempt per result).

Separate pure scheduling measurement (`gg-control/tests/claim_storm.rs`): 60 workers, 200 tasks,
600 assignments in 3.1 s, i.e. ≈194/s, with no double assignment.

## What these figures say

1. **An accepted result costs about twice its own computation.** At 512³ and 1024³, verification by
   full recomputation costs 88 to 89 % of the execution. The 128³ case (29 %) is dominated by fixed
   costs and must not be quoted as representative.
2. **An audit adds one more execution** on the audited share: with `audit_rate = r`, the expected
   cost per accepted result goes from about 2× to about (2 + r)× the useful computation.
3. The observed throughput is bounded by computation, not by the scheduler: 194 assignments per
   second versus 11 to 61 results per second depending on task size.

## What these figures do not say

- **Nothing about monetary or energy cost.** The necessary inputs (electricity price at each
  contributor's home, hardware depreciation, bandwidth cost) are not available here. No cost per
  result in euros is published, and no conversion is done behind the scenes.
- **Nothing about an advantage over renting compute.** A platform that spends at least twice the
  useful computation starts with a 2× handicap on raw cost. For a volunteer network to be
  worthwhile, the offered computation must be very cheap or otherwise unused. This comparison has
  not been made and must not be assumed.
- **Nothing about scaling over a network.** All these measurements are local: no Internet latency,
  no transfer of large artifacts, no heterogeneous contributors.

## Credit model

Credits (unit `TC`) are integers, announced **before** execution in the task's rate schedule
(`reward_schedule`), and split between the execution reward and the verification cost. They depend
neither on reported GPU time nor on a self-proclaimed loss improvement. Measured above: 2 TC per
result at 512³ (1 for execution + 1 for verification), 6 TC at 1024³, in line with the catalog's
rate schedule, which indexes the reward on the FLOPS **of the requested work**, not on what the
worker claims to have done.

These credits have no promised monetary value, and the settlement layer (ADR-0015) commits a test
token distribution, backed by neither a quantity of computation nor a price.

## Known levers, not implemented

- **Sampled verification** rather than systematic verification: would divide the overhead by the
  verified share, at the price of probabilistic detection. This is already the form audits take
  (P6); applying it to the main verification is a policy decision, not a fix.
- **Verification cheaper than recomputation** (algebraic proofs for matrix multiplication, for
  example): a research topic, out of scope, and with no possible extrapolation to full
  floating-point training.
- **Artifact locality**: peer-to-peer reduces the bandwidth served by the server (measured: 91
  chunks out of 237 in a six-worker demo), not the computation.
