# ADR-0017 — Distribution plan: stages at layer boundaries, computed before submission

Status: accepted (2026-09-17), phase P9 slice 2. Corrects a claim made in ADR-0016.

## Context

The platform's goal, as restated by its sponsor: **train very large models or run very large
computations, in a distributed way, without owning hardware that costs a fortune. Slowness is
acceptable; impossibility is not.**

ADR-0016 stated that "splitting the same graph across distant machines does not work over the
Internet". That sentence conflated two things, and the second one is false:

- **distributing operator by operator** (a PyTorch driver that sends each matrix multiplication to the
  network): every intermediate tensor crosses the Internet, the compute per byte transferred is a few
  FLOPs, and one training step of the example transformer block takes 33 minutes over a home
  connection versus 3 seconds locally. That figure stands.
- **distributing in stages at layer boundaries**: only the residual stream crosses the cut, once
  forward and once backward. The compute per byte is 30,000 to 60,000 FLOPs. It is slow, but it
  finishes in days, not years.

The `gg job plan` calculator, written to settle the question, gives the following for a 1.3-billion
parameter transformer (24 layers, width 2,048) and 100 million tokens:

| Machines | Uplink | Executor | Stages | Duration | One datacenter GPU |
|---|---|---|---|---|---|
| 100 ADSL homes | 10 Mbit/s | 300 GFLOP/s (measured) | 9 | 2.2 days | 1.5 h |
| 100 ADSL homes | 10 Mbit/s | 10 TFLOP/s (tuned) | 9 | 2.2 days | 1.5 h |
| 100 fiber homes | 100 Mbit/s | 300 GFLOP/s | 9 | 11.5 h | 1.5 h |
| 100 fiber homes | 300 Mbit/s | 300 GFLOP/s | 9 | 10.8 h | 1.5 h |
| 100 fiber homes | 300 Mbit/s | 10 TFLOP/s | 9 | 1.7 h | 1.5 h |
| 100 fiber homes | 500 Mbit/s | 10 TFLOP/s | 9 | **1.0 h** | 1.5 h |
| 100 lab machines | 1 Gbit/s | 10 TFLOP/s | 9 | 31 min | 1.5 h |

For 6.7 billion parameters (32 layers, width 4,096) on 200 machines: 5.1 days over ADSL; 28 h over
fiber at 300 Mbit/s with the measured executor; **4.1 h** at 300 Mbit/s and **2.5 h** at 500 Mbit/s
with a 10 TFLOP/s executor; 1.2 h in a lab. One datacenter GPU: 7.6 h.

Home fiber is now the norm in France, with uplinks of 100 to 500 Mbit/s: the "fiber" rows describe
actual contributors, and ADSL is the degraded case. Three lessons:

1. **Over home fiber, the executor sets the pace**, not the link: with the current kernels, the
   network takes only 8 to 40 % of the time. The executor measures 305 GFLOP/s on a card rated at
   23,000; **bringing it to 10 TFLOP/s cuts the duration by six**. This is the first work item. Only
   over ADSL does the link decide everything, and kernel speed does not matter.
2. **With a tuned executor, 100 fiber homes beat a datacenter GPU**, and 200 homes train the
   6.7-billion model overnight. The link then becomes the limit again, but only by a factor of 2 to 4,
   which activation compression can absorb.
3. **What makes a model feasible is not that it fits on one machine, but that a stage does.** The
   ADR-0016 bound is replaced.

## Decision

1. **The graph is cut into contiguous stages, at the least loaded boundary.** A stage grows as long as
   it fits in a contributor's memory (parameters × 4 for Adam, plus the activations of one
   micro-batch); when it overflows, the cut is moved back to the boundary within the stage where the
   fewest bytes cross, which lands on the residual stream between two layers and not in the middle of
   an attention block. Each crossing tensor is counted once even if it has several consumers.
2. **Pipelines are replicated**: with S stages and N contributors, ⌊N/S⌋ pipelines process different
   micro-batches and synchronize their parameters every `local_steps` micro-batches. The ADR-0011
   coordinator aggregates the gradients of a stage just as it aggregates those of an MLP today.
3. **The plan is computed before any reservation**, offline, from the validated graph (ADR-0016):
   stages, cuts, pace, what sets it, duration, total traffic, and an honest comparison with a single
   datacenter GPU. The researcher decides with full knowledge.
4. **The unit of work becomes "the gradient of one stage on one micro-batch"**, which generalizes
   ADR-0011 (a single stage) without contradicting it. It remains recomputable by a validator that
   holds the stage's weights and the signed incoming activations.

## Cost model, and what it ignores

The computation is deliberately simple; every assumption is written down so it can be challenged:

- steady-state pace = max(compute of the heaviest stage, transfer across the most loaded cut);
  activations and their gradients cross once each; the effective throughput is the minimum of the
  uplink and downlink rates;
- latency and pipeline bubbles are ignored: at a 100 ms round trip, they weigh less than 1 % as soon
  as a micro-batch takes more than a few seconds;
- a stage's memory keeps all of its activations for the backward pass (no activation recomputation):
  a pessimistic estimate, hence more stages than necessary;
- auditing multiplies the work by (1 + rate); verifying a stage costs as much as computing it;
- **contributor failure is not modeled.** Over five days with two hundred volunteers it is certain; a
  missing stage stops its pipeline until reassignment from the last checkpoint. That cost is to be
  measured in slice 4, not assumed to be zero.

## Consequences

- ADR-0016 is corrected: the constraint "the model must fit on one machine" becomes "a stage must fit
  on one machine", and the description bound rises to 10¹² parameters.
- The target audience is contributors on fiber, individuals and labs alike; ADSL links remain useful
  for what does not split: grids, ensembles, inference, and models that fit on one machine with many
  local steps.
- **The executor's priority is kernel speed**: from 1 % to a few tens of percent of the hardware.
  Next comes compressing what crosses the network (half-precision activations, adapters), which absorbs
  the remaining factor of 2 to 4 on the link.
- The defaults of `gg job plan` describe a typical home fiber connection (300 Mbit/s up, 1 Gbit/s
  down); a contributor on ADSL describes themselves with `--uplink-mbit 10`.
- The `gg-ops/1` profile lacks two operators for a trainable language model: a causal mask and a
  positional encoding. They are due in the next profile, with their oracle and tolerance.
