# ADR-0026 — Speculative waves, event-driven wake-ups, direct hand-off of activations

Status: accepted (2026-09-23). Follows ADR-0024 (staged execution). Implements the "direct hand-off" part
of ADR-0018: first within the durable storage invariant (sections 1 to 3), then with the bounded
exception the researcher decided on for intermediate activations (section 4).

## Context

In D6, a round of a 4-stage Llama took **21.7 s**, ten times longer than with data parallelism. Before
choosing what to change, the timeline of each wave was extracted from the database (task creation,
claim, submission, decision, next wave), over 40 waves:

| Item | Average time per wave |
|---|---|
| execution (claim, sandboxed executor, upload) | ~0.6 s |
| waiting for the validator to **verify** the 4 tasks | ~1.1 s |
| waiting for the **coordinator to wake up**, which only ran every 2 s (waves opened at 2.0 or 4.0 s, never in between) | ~0.9 s |

Transporting activations did not show up: locally, they weigh a few KiB. It does matter, however, on
real links with large activations (ADR-0018).

## Decision

### 1. Event-driven wake-ups

Three waits polled the database at a fixed interval:

- the coordinator, every 2 s;
- a worker's claim, every 500 ms;
- a validator's claim, every 500 ms.

They now wake up as soon as the awaited event is recorded (`AppState::signals`,
`tokio::sync::Notify`):

- a task created or requeued wakes the workers;
- a submission wakes the validators and the coordinator;
- a decision wakes the coordinator and the workers, since a rejected task goes back into the queue.

The subscription is taken **before** the database check, so that no event is lost between the two.
Polling remains as a safety net: a missed wake-up costs only one interval, never a deadlock.

### 2. Speculative waves

Wave *k + 1* opens as soon as **every task of wave *k* has been submitted**. It no longer waits for
them to be verified; verifications run in the meantime. The rules that keep the result exact:

- the coordinator **pins the attempt** whose outputs it passed on (`StageState.recorded`);
- a pinned attempt must end **accepted**. If it is rejected, expires or is superseded, the round is
  **poisoned**: all of its jobs (one wave per job) are canceled, and it is reissued from wave 0 with the
  same checkpoint. This is already what a failed task caused;
- nothing is aggregated or published until **every** task of **every** wave is accepted;
- an "inconclusive" attempt waits for the outcome of its dispute, without poisoning the round.

Fraud can therefore cost a round, **never a wrong weight**. No invariant changes:

- nothing is paid without an accepted verification;
- the checkpoint depends only on accepted results;
- a canceled task receives no further assignment.

The cost of fraud for the researcher: honest tasks computed on a false activation are correct with
respect to their inputs, hence accepted and paid, and then the round is redone. The fraudster is not
paid, and their signature on the false activation identifies them.

### 3. Direct hand-off of activations, within the invariants

The producer of an activation still sends it to the server, which stores it durably and computes its
signed manifest. In addition, the producer **keeps it in cache** and **announces itself as a holder**.
The server allows this for an artifact the worker uploaded itself (`uploaded_by = worker:<id>`), and
for nothing else. The next stage fetches it from the producer, peer to peer, and verifies each chunk
against the signed manifest before writing it. Everything goes through the existing P5 layer:

- TLS with raw public keys;
- read tickets;
- lying peers excluded;
- fallback to the server if the peer does not respond.

What this changes: the server no longer **sends** activations to the next stages. Its link now carries
only one copy of each activation instead of two. What it does not change: the upload to the server
remains **on the producer's critical path**, because an output is only referenced after durable
storage (a repository invariant).

### 4. Hand-off before the durable copy (decision of 2026-09-23)

For the activations of a stage task only:

1. The producer declares the hash, size and chunk digests (`…/outputs/{hash}/declare`).
2. It keeps the activation in cache, announces itself as a holder, and **submits without having
   uploaded it**.
3. The next wave opens. Its worker fetches the activation from the producer, each chunk verified
   against a manifest signed by the server **over the declared digests**, then the whole against the
   hash in the signed submission. The server answers `409` as long as it does not have the copy; the
   reader retries, through the peer and then the server, within a 60 s limit.
4. The durable copy is uploaded in the background after submission, **yielding** while the producer is
   serving chunks to a peer: the uplink goes first to what is on the critical path.
5. No verification is offered while any input or output of the task lacks a durable copy. Nothing is
   therefore paid or aggregated before it exists. After `GG_PENDING_OUTPUT_SECS`, the round is
   reissued.

**A first attempt was wrong, and the measurements showed it.** In that attempt, the durable copy waited
until the activation had been served once in full, with a 20 s cap. But an activation that the next
stage reads from its own cache is never served: its copy waited the full 20 s, and verification with
it. The round went from 29 s to **90 s**. Hence the rule "yield while serving" rather than "wait for the
reader".

## What the measurements establish

Same Llama from `transformers` (2 layers, 4 stages), same real grid: 3 sandboxed workers with a third
of the required memory, 5 rounds of 128 examples in 4 tasks.

| | Before | After, 1 validator | After, 3 validators |
|---|---|---|---|
| Round duration | 21.7 s | **8.9 s** | **4.6 s** |
| Execution of the 8 waves | ~21 s (including waiting) | **2.85 s** | — |
| Loss after 5 rounds | 2.5132 | 2.5132 (bit for bit) | 2.5132 (bit for bit) |

The loss values in this ADR were measured before the domain tag of the deterministic generator
(`gpu-goblin/drbg/v1`, `gg-protocol::drbg`) took the project's current name. The tag seeds the order in
which examples are drawn, so the same test now reports 2.5048 after 5 rounds; the comparison between
variants is unaffected.

- **Verification is now the bottleneck.** The 32 tasks of a round go through the validator at ~0.27 s
  each, including starting the sandboxed executor. Round duration therefore tracks the number of
  validators.
- **Data parallelism**, same grid, same model: 2.0 s → **0.41 s** per round, thanks to the wake-ups.
  The gap between stages and data parallelism remains around **×11** (3 validators). Each stage still
  pays its fixed execution and verification costs (D5).
- **Direct hand-off**, P2P enabled, 3 rounds:
  - 174 chunks from a peer versus 63 from the server, 0 rejected;
  - of 520 current announcements, 283 are from a producer for its own output.
- **Fraud**: `tests/collective.rs::a_forged_activation_already_passed_on_reissues_the_round`.
  1. A worker forges an activation in wave 0.
  2. Wave 1 reads it **before any verification**.
  3. The validator rejects it, and the round is reissued.
  4. The published checkpoint is **identical byte for byte** to that of an honest run with the same
     seed.
- **Right to announce**: `tests/p2p.rs::a_producer_may_announce_its_own_output_and_only_it`.
- The 12 end-to-end collective training tests, stages and shards included, pass unchanged. Their
  checkpoint is still equal to the single-process replay.

**Hand-off before the durable copy, simulated uplink.** A larger Llama from `transformers` (width 256,
sequence 64, ~2 MB per activation), 4 stages, 3 workers, 3 validators, 3 rounds. Each worker's uplink is
simulated at **4 MB/s** (32 Mbit/s), shared between its uploads to the server and the chunks it serves
(`dev_faults.uplink_bytes_per_sec`, a measurement setting rejected in public mode; lacking the
privileges to throttle the network card, this is a simulation, and is reported as such):

| | Average round |
|---|---|
| Activations uploaded before submitting (ADR-0026, sections 1 to 3) | **29.1 s** |
| Hand-off before the durable copy, copy waiting for the reader (first attempt) | 89.8 s |
| Hand-off before the durable copy, copy yielding while serving | **22.7 s** (−22 %) |

The gain is real but more modest than the expected "up to twice as fast". The durable copy still goes
over the same link before verification can start, so it remains part of the round's duration. Without a
throttled link, same grid: same loss **bit for bit** (2.5132 after 5 rounds), every round aggregated on
the first try, 345 chunks from peers, 0 rejected.

### 5. Follow-up of 2026-09-23: what remained, measured in release builds

**The fixed per-task costs were not where we thought.** Timed item by item, a verification spent 3 to
8 ms recomputing, and **~290 ms fetching its inputs**. Each download cost ~20 or ~40 ms, even for 1 KB:
that is the signature of Nagle's algorithm interacting with the client's delayed acknowledgment, with a
response's header and body leaving in two segments. `TCP_NODELAY` is now enabled on control plane
connections and on peer connections. Debug builds, suspected at first, weighed almost nothing (34 versus
46 ms of execution).

**Pipelining between micro-batches.** Each micro-batch advances on its own: the next stage of a
micro-batch starts as soon as its current stage is submitted, without waiting for the others. The safety
rules do not change. While doing this, a real race showed up: a poisoned round was marked as failed and
its jobs were then canceled in another transaction, which left a few milliseconds during which one of
its tasks could still be claimed. Cancellation now happens in the same transaction (`cancel_job_in`), as
it does when a researcher cancels a run.

Same small 4-stage Llama, 3 workers, 3 validators, release binaries, 5 rounds:

| | Round | Data parallelism |
|---|---|---|
| Before `TCP_NODELAY` | 4.45 s | 0.41 s |
| With `TCP_NODELAY` | **1.48 s** | **0.156 s** |
| With pipelining between micro-batches | **1.27 s** | — |

The loss stays identical bit for bit (2.5132) and every round is aggregated on the first try. Since the
start of this series (21.7 s), the staged round is **17 times shorter**. What remains is around 75 ms of
overhead per task (claim, sandboxed executor, uploads), which all 32 tasks of a round pay.

### 6. Per-task overhead, timed phase by phase

The ~75 ms per task remaining in section 5 was attributed to the sandboxed executor. Timed phase by
phase on the worker side (`attempt timed`), it broke down as follows:

| Phase | Before | After |
|---|---|---|
| Fetching inputs | 41.3 ms | **2.6 ms** |
| Uploading outputs | 22.5 ms | **10.5 ms** |
| Executor process (sandbox included, compute 1.6 ms) | 12.4 ms | 13.2 ms |
| Submission | 4.0 ms | 4.3 ms |

The sandboxed executor was therefore **not** the main item. Keeping it alive between tasks would save
only about ten milliseconds, at the price of weaker isolation: it is not done. The main item was the
peer-to-peer ceremony applied to artifacts of a few KB (announcement with a connect-back, manifest, peer
list, TLS handshake). Without peer to peer, fetching dropped from 41 to 5 ms. Hence:

- **a threshold** (`p2p.min_bytes`, 1 MiB). Below it, an artifact is read from the control plane,
  without announcement or early declaration. An activation still in transit (409) goes through the
  peers as before. The `--p2p` demo, whose artifacts are small, lowers this threshold to exercise the
  mechanism;
- **inputs fetched and outputs uploaded in parallel**. Locally the gain is small, but on a real network
  each input or output costs at least one round trip.

**Bug found along the way.** A download wrote to `dest.with_extension("part")`, and both `in.0` and
`in.1` gave `in.part`. Sequentially this was invisible; in parallel, two inputs overwrote each other.
The measurements revealed it through a **failed** run, which I checked before reading the duration
figure, too good to be true. The cache did not support two simultaneous writes of the same hash either.
Both are fixed (`part_path`, unique temporary name) and tested.

Same small 4-stage Llama, P2P enabled, 3 workers, 3 validators, release, 5 rounds: round
**1.37 → 0.55 s**, loss identical bit for bit, every round aggregated on the first try. The path for
large artifacts (2 MB activations: early hand-off, swarm) is still exercised: 152 chunks received from
peers, 0 rejected.

## Limitation uncovered

**Workers that start together on the same input all fetch it from the server.** A peer only serves what
it holds in full. Polling at 500 ms staggered start times by chance, and the `--p2p` demo depended on
this without saying so. With wake-ups, 3 workers claimed 20 ms apart, and not a single chunk was served
by a peer. The demo now lets the first worker fetch the input alone, and still requires chunks served
by peers (20 versus 10).

Activation hand-off is not affected: the producer holds its output before the next task even exists.
For large shared inputs (checkpoint shards), the answer is exchanging chunks **during** the download,
swarm-style. It is not done.

## What is not done

- ~~**Taking the upload to the server off the critical path.**~~ Done (section 4). The consumer would
  fetch the activation from the producer **before** the server has a durable copy, with a manifest
  signed over the digests announced by the producer (ADR-0018 (a)). On a home uplink, that is up to
  half the transfer time per wave. This contradicted the invariant "an artifact is only referenced after
  durable storage and a SHA-256 check". **Decision of 2026-09-23**: performance comes first, and the
  invariant gets an exception bounded to intermediate activations (docs/SPEC.md, invariant I7).
  **Implemented the same day**, see section 4.
- ~~**Pipelining between micro-batches**~~: done (section 5).
- ~~**One TCP + TLS connection per chunk** between peers~~: one session per peer and per transfer
  (ADR-0012), saving one round trip per chunk; then four chunks in flight (ADR-0012), 3.3 times faster
  behind a 40 ms round trip.
- **Fixed verification costs**: a fresh sandboxed executor per verified task (D5).
