# Changelog

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.1.1] — 2026-10-11

- Release 0.1.1: the contributor software runs on a stock Ubuntu 24.04 (gg-worker's changelog).
- The sign-in code e-mail speaks of signing in, for researchers and contributors alike.
- nginx template: the former HTML portal pages redirect to the site's account page.
- CI reads gg-worker without a deploy key, the repositories being public.

## [0.1.0] — 2026-10-11

First complete version of phases P0 to P9. **This repository is not ready for public
deployment**: see `docs/RISKS.md`.

### Added

- **Control plane**: projects, budgets in integer credits, jobs, tasks, leases, signed receipts,
  ledger of balanced append-only entries, quotas, tested backup and restore.
- **Protocol**: canonical DAG-CBOR, Ed25519, domain separation, replayable test vectors.
- **Catalog**: matrix multiplication, perceptron training, micro-batch gradient.
- **Execution**: Burn on CPU (Flex) and GPU (WGPU/Vulkan, explicit hardware, never a silent
  fallback); executor under bubblewrap, Landlock and seccomp.
- **Collective training**: canonical coordinator, disjoint shards, single aggregation, host
  optimizer `adam-host-v1`, checkpoint per `(run, step)`.
- **Peer-to-peer**: signed chunk manifests, read tickets, TLS 1.3 with raw public keys, bounded
  cache, NAT traversal through UPnP-IGD measured on a real router, IPv6 first.
- **Federated validation**: validation operators, unpredictable audit sampling, disputes settled by
  a human.
- **Rewards**: Merkle commitment and CosmWasm contract tested on a simulated chain. No real funds.
- **Release readiness**: load and full cost measured, offline-signed and verifiable release,
  license inventory, incident procedures, residual risk report.
- **Model-agnostic training (P9)**: `gg-graph/v1` format, `gg-ops/1` operator profile published by
  the API, validation with a named rejection before reservation, staged distribution plan,
  execution with Burn, `graph.grad.v1` compute type, collective training of a graph.
- **Python package `gpugoblin`**: REST client, GGT1 and GGB1 formats, and a **PyTorch driver** that
  captures a module with `torch.export` and translates it into a verified description.

- **Memory levers executed (P10 slice 3)**: segmented activation recomputation, parameter
  offloading to host memory, frozen parameters with a gradient that only carries the trained ones.
  The gradient is unchanged (rounding difference 1.8e-5, decreasing with the number of segments)
  and the executor measures its memory peaks instead of estimating them.
- **Distribution planner**: `gg job plan` prices both possible topologies and, when an operator
  overflows, names the node and lists the remedies with their cost.

- **Profile `gg-ops/2`**: `CausalMask` operator, constant initialization for model scalars, and
  causal attention captured from PyTorch — triangular buffer, `torch.triu`, `torch.where`,
  `scaled_dot_product_attention(is_causal=True)` — recognized by the mask's value. A language model
  written like nanoGPT trains on the grid.
- **`gg job eval-model`**: offline evaluation of a description.
- **Profile `gg-ops/3`**: `Slice` operator and position table generators (`sinusoidal`,
  `rotary_cos`, `rotary_sin`); learned, sinusoidal and RoPE positions, captured from PyTorch and
  recognized by their value. Transformer skeleton with `--positions`, RoPE by default.
- **Profile `gg-ops/4`**: rotary tables with explicit frequencies and amplitude, the form of any
  RoPE scaling (Llama 3.1, YaRN, LongRoPE, linear, dynamic NTK). The PyTorch driver recovers the
  frequencies from the pair of tables, whatever the variant; `freeze_rotary_embeddings` freezes
  modules whose frequencies change with the sequence length; `gg job template --rope-scaling` reads
  the `rope_scaling` object of a `config.json`.

- **P11 (2026-09-23)**: capabilities advertised by workers, and tasks that fit them; sharded
  checkpoint; profile `gg-ops/5` (`ReduceMean`, `Rsqrt`) and `transformers` models captured as is
  (Llama, Qwen2, Mistral); `trainer.plan()`, memory levers chosen by the grid, sharded datasets and
  micro-batches supplied by the server.
- **Per-task fees** (ADR-0026): an artifact under 1 MiB (`p2p.min_bytes`) is read from the control
  plane without the peer-to-peer ceremony; a task's inputs and outputs are transferred in parallel.
  4-stage round 1.37 → 0.55 s with P2P enabled.
- **Swarm between workers** (ADR-0012): a worker serves each chunk of an input as soon as it has
  verified it; chunks are spread across concurrent readers by rendezvous hashing. Two readers of
  the same artifact request a single copy from the control plane instead of two.
- **Four chunks in flight between peers** (ADR-0012, `p2p.parallel_chunks`): behind a 40 ms round
  trip, 16 chunks in 221 ms instead of 734.
- **One connection per peer and per transfer** (ADR-0012): the TLS session is reused for all
  chunks; behind a 40 ms round trip, 8 chunks in 389 ms instead of 688.
- **Per-task fees and pipelining (P11, ADR-0026)**: `TCP_NODELAY` on control plane and peer
  connections (each download was losing 20 to 40 ms); the micro-batches of a staged round are
  pipelined. 4-stage round 4.45 → 1.27 s, data parallelism 0.41 → 0.156 s.
- **Hand-off of an activation before its durable copy (P11, ADR-0026)**: a bounded exception to
  the durable-storage invariant; the producer declares the digests and submits, the next stage
  reads from the producer, and the durable copy follows, yielding while peers are being served;
  nothing is verified or paid before it lands. Staged round −22 % on an uplink simulated at 4 MB/s.
  Measurement setting `dev_faults.uplink_bytes_per_sec`.
- **Speculative waves (P11, ADR-0026)**: the coordinator, workers and validators wake up on events
  instead of polling; the next wave of a staged round opens on submitted outputs, and the round is
  reissued if one of them is rejected; the producer of an activation serves it peer-to-peer. Staged
  round 4.7 times shorter, data parallelism 4.9 times.
- **Custom operators (P11, ADR-0025)**: profile `gg-ops/6` with `Formula`, an element-wise
  expression written in the description (closed grammar, automatic derivative, CPU and GPU);
  description functions (`functions`, called as `fn:<name>`), inlined by the server;
  `gpugoblin.formula` to write the operator inside a PyTorch module; `gpugoblin.coverage` and a
  corpus of 18 architectures checked against PyTorch (16 pass, versus 3 before); the driver
  translates `chunk`, `split`, `select`, `stack`, `Conv1d`, `GroupNorm`, `nn.LSTM` (unrolled),
  adaptive average pooling and about twenty element-wise functions.

### Changed

- Private beta: hardened deployment (systemd, Caddy, backups and restore, health check), signed
  worker releases verified with `ssh-keygen` and `sha256sum`, researcher sign-in by e-mail link
  (ADR-0028), a research data rule (`research-any-data`) and an initial grant per project,
  withdrawal of a validator or an artifact, a minimum worker version, client addresses behind the
  proxy, separate upload timeout, SIGTERM handling.
- Contributor accounts (ADR-0030): anyone signs up on the portal with a code sent by e-mail, with
  no admission; a machine attaches itself to the account with an enrollment code and works at
  once; suspending an account stops all its machines.
- The plan checks host memory as well as accelerator memory (`task_ram_bytes`,
  `workers_with_ram`) and splits a model into stages within both.
- Validation on the GPU (ADR-0031): a validator with `engine = "gpu"` recomputes model tasks on
  its GPU; a sample of what it decides (`engine_check_rate`) is recomputed again on the CPU before
  anything is aggregated or paid, and a disagreement opens a dispute. A CPU validator can take only
  audits and engine checks (`secondary_only`). On one RTX 5060 Ti, a ViT-tiny round goes from 23 to
  9 s.
- A validator can recompute model tasks on the native CUDA engine (`engine = "cuda"`, ADR-0033).
- Leases follow the heartbeat rhythm a worker announces and are kept alive for the whole attempt:
  a lost worker costs 22 s instead of 128 s on a staged run.
- Operator profiles `gg-ops/7` (ADR-0027) and `gg-ops/8`, integer tensors (ADR-0029): 26 of 26 corpus
  models captured, mixtures of experts included; a Mixtral trains on the grid.
- Requests covered by a live lease of the caller (a worker's active attempt, a validator's held
  verification) no longer consume quota; everything else is still charged, and an empty bucket
  still refuses up front. With one validator, a 4-stage round went from 2.50 s to 1.68 s.
- The code is published as several repositories: gg-core (shared crates), gg-server (this one),
  gg-worker, gg-python, and the planned gg-console and gg-web. The server and the worker depend
  on gg-core at a pinned tag.
- The client of the control plane (HTTP API, resumable transfers, artifact cache) is now its own
  crate, `gg-client`, shared by the worker, the validator and the `gg` CLI, which no longer depend
  on the worker.
- Tensor files use the `.ggt` (one tensor) and `.ggb` (named tensors) extensions in examples and
  documentation.
- An unknown field is rejected everywhere in a `gg-graph/v1` description instead of being ignored:
  a worker running an older version no longer silently computes a different function from the one
  described.

### Fixed

- The element-wise tolerance of model tasks rejected a ResNet18 computed on the GPU as accurately as
  PyTorch: model tasks are now compared by norm (ADR-0031).
- A ResNet18 trained on a GPU worker was rejected by the validator: the GPU gradient of a
  convolution whose kernel is shorter than its stride was wrong (gg-core). A staged run of 32
  sequences of SmolLM2 was refused, a stage's output size counting every activation twice.
- ConvNeXt-tiny and a staged SmolLM2-135M waited forever on 4 GiB workers: the host-memory limit
  of a task counted its activations three times, and a stage was sized from the whole model's
  parameters, while the plan said the task fit. The four pilot models (ResNet18, ViT-tiny,
  ConvNeXt-tiny, SmolLM2-135M) now train on the grid.
- A perceptron training killed while it wrote a checkpoint could resume from a mix of two
  checkpoints (the model of one step, the optimizer of the previous one) and produce a result the
  validator rejected: about one resume in four in the fault demonstration. Each checkpoint now goes
  into its own directory, flushed to disk, and `state.json`, replaced atomically last, points to it.
- A 429 (quota) or a 503 (transaction to replay) made a worker's or validator's request fail; a
  validator would then abandon a verification it could not even return, which stayed blocked for
  ten minutes. The client now waits for `Retry-After` and retries.
- The plan proposed shards of a quarter of a machine, which never fit on their own in a stage: it
  now shrinks the shards until the split fits.
- Two downloads to `in.0` and `in.1` shared the temporary file `in.part`, and the cache did not
  support two concurrent writes of the same digest.
- A transaction aborted by PostgreSQL for a deadlock answered 500: it is now replayed (decisions,
  submissions, coordinator), and answers 503 if it fails again.
- A poisoned staged round canceled its jobs after committing its failure, in a separate
  transaction: a few milliseconds during which one of its tasks could still be claimed.
- The PyTorch driver miscomputed the shape of a broadcast `matmul` (`[64, 2] × [N, 2, 8]`) and of a
  product broadcast in both directions, and translated `flatten(2)` as `flatten(1)`. A safeguard
  now checks every computed shape against the one PyTorch measures.
- The PyTorch driver mistook a captured tensor constant for a model input.
- A module with a buffer could not reload its trained weights.
- The driver emitted a `Concat` with a negative axis for `torch.cat(dim=-1)`, which validation
  rejected.
- ADR-0021 claimed the RoPE tables were byte-identical to Hugging Face's; measured again, ~95 % of
  the values are, and the difference ranges from one ulp to 1.5e-5 depending on the base. The
  measurement is now a test.

### Notes

- License: PolyForm Noncommercial 1.0.0 (free for research, source-available).
- Nothing is deployed on the Internet.
