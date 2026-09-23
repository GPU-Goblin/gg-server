# ADR-0040 — Retention of a training run's intermediate artifacts

Status: accepted (2026-09-25).

## Context

Every round of a training run stores artifacts:

- the workers' outputs (gradients, and stage activations);
- the micro-batch rows the coordinator cuts;
- the new weights and the optimizer moments.

Nothing was ever removed. For SmolLM2-135M that is about 2 GB per round, so a run of 1000 rounds
fills 2 TB. A beta server would fill its disk within days of real use.

## Decision

- **Only intermediate artifacts of training runs go.**
  - Only an artifact a worker uploaded or the coordinator produced can expire.
  - A researcher's upload never expires: datasets, descriptions, initial checkpoints.
  - Neither do a plain job's results, or a release.
- **A round is examined once it is settled for good.** All of the following must hold:
  - it is closed (aggregated or failed);
  - `retention_after_secs` have passed (3600 by default);
  - every task is accepted, failed or cancelled (a rejected or inconclusive task may still be
    retried, a disputed one awaits a human);
  - every verification is decided, audits and engine checks included;
  - no dispute is open.
- **While a run goes on, the round that produced its current checkpoint stays whole.** Its
  gradients rebuild the optimizer moments after a restart (ADR-0036).
- **Candidates.** The inputs and outputs of the round's tasks, and what the checkpoint it produced
  references (weights, moments).
- **Kept, whatever the round.**
  - Anything a run of the project names: configuration, current checkpoint with its moments.
  - The weights of the checkpoints kept: step 0, every `checkpoint_keep_every` steps (50 by default),
    and the current one. A frozen shard shared with them stays too.
  - The inputs and outputs of the run's rounds not examined yet.
  - Anything a task not yet decided names in its spec.
- **An expired artifact keeps its row** (hash, size, owner) with the time it expired. A download
  answers that it expired, by name. Storing the same bytes again later revives it.
- **The order of removal.** The row is marked expired before the bytes are deleted, so the database
  never names a live object that is gone. A server pass runs every `retention_interval_secs`
  (300 s). `retention = false` keeps everything.

## Consequences

**Disk use.** A run keeps its current state plus the weights every 50 steps. For SmolLM2, 1000
rounds keep about 11 GB instead of 2 TB.

**What a researcher loses.** The intermediate checkpoints between two kept ones, once their round is
settled. The run's view still lists them, and downloading one says it expired.

**What the tests show.** Two end-to-end runs, one data-parallel and one in stages, run with the
retention policy every 20 ms, no delay, a checkpoint kept every 2 steps, and the server's kept
state dropped every 20 ms as restarts would drop it.

- Both still end bit for bit where the single-process replay does.
- Afterwards:
  - the kept checkpoints download, and the others say they expired;
  - workers' outputs are gone;
  - no researcher upload is.

**Limits.**

- The live set is computed per run from the database: the configuration and current checkpoint of
  the project's runs, the kept checkpoints, and the specs and submissions of the rounds not yet
  examined. With thousands of rounds left unexamined (retention turned on late), a pass costs more
  until they are cleared, 50 rounds at a time.
