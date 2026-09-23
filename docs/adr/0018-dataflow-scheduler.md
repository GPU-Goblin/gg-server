# ADR-0018 — Dataflow scheduler: the coordinator schedules, peers exchange activations

Status: accepted (2026-09-17), phase P10. Refines ADR-0017.

## Context

The sponsor proposes a precise design for the PyTorch driver:

> We split the operations, causally, send them to the peer; peers reply once their stack is computed,
> sending back to the source of the computation, which emits the next packets of work to do. That
> gives us a coordinator that manages the computations and can, simply, reassemble the packets and
> redistribute the work when needed.

This is a **dataflow scheduler**: the graph is split into packets that respect dependency order, a
packet becomes runnable once its inputs exist, and the coordinator tracks that state and reissues
whatever is missing. Two qualities of this design are right and important, and the platform does not
have them today:

- **Failure recovery is trivial.** A peer that disappears breaks nothing: its packet becomes runnable
  again and goes elsewhere. ADR-0017 described a rigid chain in which the failure of one stage stops
  its whole pipeline; with volunteers, where failure over several days is certain, this is that
  design's most serious flaw.
- **Rescheduling is a state, not a reconfiguration.** Adding, removing or replacing a contributor does
  not require rebuilding a topology: the coordinator redistributes.

One question remains, and only one: **which path do the tensors take?** There are two possible
answers, with the same partitioning and the same machines in both cases.

## What the measurements say

`gg job plan --topology coordinator|peer`, on the skeletons from `gg job template`, with 100 or 200
contributors on home fiber (300 Mbit/s up) and an executor tuned to 10 TFLOP/s:

| Model | Machines | Star (everything returns to the source) | Peer to peer (stage to stage) |
|---|---|---|---|
| 1.3 B, 100 M tokens | 100 | **5.1 days** | **1.7 h** |
| 1.3 B, coordinator on symmetric 1 Gbit/s | 100 | 36.8 h | 1.7 h |
| 6.7 B, 100 M tokens | 200 | **20.3 days** | **4.1 h** |

The cause is arithmetic and does not depend on any tuning. In a star, a tensor that crosses a cut
goes through the coordinator **twice**: it receives it from one stage and sends it to the next.
Forward and backward, that makes four crossings per cut and per micro-batch, and **every cut of every
pipeline shares that single link**. On the 6.7-billion model: 4 GiB per micro-batch on the
coordinator's link, i.e. 57 s, versus 3.6 s on the most loaded cut in peer to peer.

**This ceiling cannot be raised by adding contributors or by speeding up the machines.** It is the
only resource in the system that does not parallelize. Moving the coordinator to symmetric 1 Gbit/s
fiber divides it by three and still leaves a factor of 22 relative to peer to peer.

## Decision

**Separate the control plane from the data plane.** The proposed dataflow scheduler is adopted for
control; activations do not go through it.

1. **The coordinator schedules.** It holds the dependency graph and the state of each packet, knows
   which one is runnable, detects vanished peers, reissues, reassembles. It exchanges only control
   messages: a few kilobytes, once per packet. This is exactly the proposed design, and it brings the
   failure recovery that ADR-0017 lacked.
2. **Peers exchange activations directly.** A finished packet publishes its result; the next peer
   fetches it from its producer. This layer already exists (P5): chunk manifests signed by the server,
   read tickets, TLS 1.3 with raw public keys, NAT traversal, verification of every chunk before it is
   written. A byte coming from a peer is never written without being verified against a server
   signature — this is a repository invariant.
3. **The coordinator stays on the control plane**, not on the researcher's machine. A run lasts hours
   or days; the researcher's laptop does not have to stay on, and its link does not have to carry the
   traffic. The PyTorch driver submits and monitors; it does not schedule.
4. **The star remains available and measurable.** `gg job plan --topology coordinator` puts a number
   on it, because it is the right choice in two cases: a model that fits on one machine (no cut, hence
   no topology cost), and a fast local network where the coordinator's ceiling does not bite.

## So who assembles between two stages?

The coordinator. But it assembles **references, not bytes**: a few kilobytes of bookkeeping on
digests, while the tensors travel from peer to peer.

The hand-off between stage *i*, held by peer A, and stage *i+1*, held by peer B:

1. **The coordinator assigns** both stages through signed assignments (`AssignmentV1`). A peer never
   executes anything without an assignment signed by the server: peers do not decide among themselves
   who works on what.
2. **A computes**, splits its output into chunks and submits `{digest, size, chunk digests}` — a few
   kilobytes, signed by A. This is today's submission, extended with chunk digests.
3. **The coordinator marks the packet finished.** The inputs of stage *i+1* exist, so it becomes
   runnable. This is where, and only where, assembly happens: a state update on a dependency graph.
4. **The coordinator sends B** the signed chunk manifest, a read ticket and the list of holders (A).
   These three routes already exist (`/worker/artifacts/{hash}/manifest`, `/peers`, `/announce`).
5. **B fetches the bytes from A**, directly, over TLS with raw public keys, verifying each chunk
   against the signed manifest before writing it. This is `fetch_with_peers`, already in service for
   input artifacts since P5.

**The order of magnitude that makes this possible.** A 67 MB activation split into 1 MiB chunks gives
64 digests, i.e. about 2.5 KiB of control plane: **one byte of bookkeeping for 26,000 bytes of
tensor**. This is why the coordinator can schedule hundreds of peers without its link mattering,
whereas it collapses if it carries the tensors itself.

### The backward pass, and what it requires

The gradient flows back in the opposite direction: B produces the gradient of its input, and A needs
it. Same mechanism, opposite direction. But **A must still be alive and still hold its activations**
when the gradient comes back: a stage assignment therefore covers forward, wait and backward — not a
single direction.

If A disappears in between, its activations are lost. Recovery stays local: the stage is reassigned
to A', which fetches A's **input** (held by the previous stage) and redoes its forward pass. The cost
is one forward recomputation of a single stage, not of the whole pipeline. This is exactly the
trade-off known as activation recomputation, and it is what makes redistribution practical.

### What the manifest guarantees, and what it does not

Today the server holds an artifact's bytes, hashes them itself and signs: the manifest certifies
"these really are the bytes of this artifact". For an intermediate activation, **the server does not
hold the bytes**: it signs the digests that the producer reports to it. The manifest then certifies
"these are the bytes A committed to", i.e. the **integrity of the transfer**, not the **correctness of
the computation**.

This is not a weakening: it is the separation that already exists for task outputs. A worker submits
output digests signed with its key; correctness comes from recomputation by a validator, never from a
digest. And the signed commitment is precisely what makes a lying peer accountable: if recomputation
differs, A's signature on that digest is the proof.

What really changes is **when** the error is seen: at the end of the chain, once the micro-batch has
gone through, instead of on a single peer's result. Hence the next section.

## What this costs, and what was not in the proposal

**Splitting the model multiplies the verification surface by the number of stages.** Today a peer
computes the full gradient of a micro-batch: one verification covers all the work. With stages, a peer
returns an intermediate activation, and a peer that lies in the middle corrupts training without
anything flagging it — the error drowns in the aggregated gradient. Verifying an activation requires
recomputing its stage, and therefore holding its weights and its input.

Consequence: the cost of an accepted result, measured in P8 at about twice its useful compute, must be
measured again for staged execution. This is a prerequisite of the slice, not a footnote.

## Consequences

- ADR-0017 is refined: splitting into stages remains the right answer, but **the rigid chain is
  replaced by dependency-driven scheduling**, which removes its most serious flaw.
- The PyTorch driver describes and submits; it neither carries tensors nor schedules.
- The P5 peer-to-peer layer becomes the training data plane, and no longer just the distribution
  channel for input artifacts.
- The cost model now knows both topologies (`gg job plan --topology`), so the choice is argued with
  numbers rather than intuitions.
- **What already exists** and is used as is: signed assignments, signed submissions carrying output
  digests, chunk manifests, read tickets, the holder registry, verified peer-to-peer fetching, NAT
  traversal, a bounded cache with pinning.
- **What is new**, and must be written: (a) a signed manifest over digests reported by the producer
  rather than computed over bytes held by the server; (b) a dependency graph of packets instead of a
  flat list of tasks; (c) a peer that keeps its stage's activations between forward and backward, and
  the reassignment that redoes its forward pass if it disappears.
- **The order of questions does not change**: first, "does the model fit on one machine?". If it does,
  there is no stage, no hand-off and none of these costs. Splitting is an answer to a memory
  constraint, never a goal.
