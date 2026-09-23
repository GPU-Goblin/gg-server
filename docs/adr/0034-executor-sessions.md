# ADR-0034 — Executor sessions: one sandboxed executor per training run on a GPU engine

Status: accepted (2026-09-25).

## Context

Until now each task ran in a fresh executor process inside a fresh sandbox (ADR-0009). On a GPU
that fresh start is a large part of a task. With the native CUDA engine (ADR-0033), a ViT-tiny
task took 1.2 s of executor time for 0.46 s of computation: the rest was loading LibTorch,
creating the CUDA context and warming up the kernels, paid again by every task.

Keeping GPU state across tasks is the only way to avoid it. It weakens one property: tasks no longer
start from a pristine process.

## Decision

- **An executor session on GPU engines (Vulkan and CUDA).** The worker starts `gg-executor serve`
  in the task sandbox (bubblewrap, Landlock, seccomp, cgroup at the worker's memory limit). The
  executor restricts itself once, before reading any task, then receives one attempt-directory name
  per line on standard input and answers one JSON line. The name is checked: letters, digits, `-`
  and `_` only, so nothing outside the session root.
- **Scoped to one training run.** A session serves the tasks of one `(project, experiment)`. A task
  of another run ends it and starts a new one, so nothing a task of one project leaves in memory
  reaches another project's.
- **Short-lived by construction.** The worker ends the session:
  - after `executor_session_max_tasks` tasks (200 by default);
  - after any failed task, timeout, lost lease or shutdown;
  - and the session dies with the worker.
- **A dedicated directory.** Its bundles live in `work_dir/session/`. The worker empties it at
  start-up, and it holds nothing but attempt bundles. It is the only writable bind besides the kernel
  cache.
- **What stays per task.**
  - The output budget and every input check, enforced by the executor code.
  - The lease and the deadline, enforced by the worker.
  - Recomputation by a validator (and a CPU sample, ADR-0031).
- **What becomes per session.** The file-size limit (the worker's disk budget) and the Landlock rule
  on the bundle directory (writable as a whole, since the next attempt's `outputs/` is not known in
  advance).
- **CPU tasks keep a fresh process each.** They also keep their per-task address-space limit, and
  the start-up they would save is small.
- **Opt-out.** `persistent_executor = false` restores a fresh executor per task.

## Consequences

**Measured on the grid.** The grid is a CUDA worker, a CUDA validator, and a CPU validator for
audits and engine checks:

| Model | Worker's executor time per task, before → session | of which computation | Grid round without a CPU check, before → session |
|---|---|---|---|
| ViT-tiny | 1.2 s → 0.33 s | 72 ms | 2.35 s → 1.45–2.45 s |
| ResNet18 | 1.45 s → 0.47 s | 130 ms | 3.5 s → 2.5 s |

The first task of a session still pays the start-up.

**What a compromised executor could now do.** An executor compromised through a task's inputs
could persist until the session ends. It would then affect later tasks of the **same run** only:
their results are still recomputed, and a sample on the CPU. It could not reach another project's
tasks, which start a new session, nor anything outside the sandbox. THREAT_MODEL.md records it.

**Not done.**

- Reusing parameters already on the card between tasks of one round (a checkpoint cache keyed by
  content hash).
- Pinned transfer buffers kept by the session: gg-compute forbids `unsafe`, and the safe route adds
  a copy.
- For SmolLM2 the time now goes mostly outside the executor: fetching a 540 MB checkpoint (1 s) and
  uploading its gradient to the server (up to 2.2 s).
