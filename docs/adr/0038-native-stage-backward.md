# ADR-0038 — Stage backward passes and memory levers with PyTorch's autograd on the native CUDA engine

Status: accepted (2026-09-25).

## Context

ADR-0033 moved whole-model tasks (`graph.grad.v1`) to PyTorch's autograd on the CUDA engine. Stage
tasks (`graph.stage.v1`), the path of models larger than one machine, still computed their backward
pass with Burn's generic formulas on LibTorch. These are slow on convolutions: a depthwise 7×7
convolution's weight gradient takes 306 ms, against 3 ms in PyTorch.

## Decision

- **A native stage backward, `exec::torch::stage_backward_cuda`.**
  - The forward pass is the executor's own operators, as on every engine.
  - The stage's trained parameters and its activation inputs are marked as requiring a gradient.
  - The last stage starts from the loss. Another stage starts from `Σ ⟨output, received gradient⟩`,
    the gradients received for an output summed in the given order, exactly as the generic path
    does.
  - PyTorch's `backward` computes the parameter gradients and the gradients sent to the previous
    stage (`din.k`).
  - A stage's forward task is unchanged: it has no backward pass.
- **Direct transfers, whole-model tasks included.**
  - Parameters go to the card as one copy into LibTorch and one transfer, after the same checks
    (declared shape, finite values).
  - Gradients come back into the host vector directly.
  - Burn's conversion made two more host copies on each side.
  - Moving everything as one block, or through pinned memory, was measured slower: allocating and
    first touching fresh 540 MB buffers costs more than the transfer. It was not kept.
- **Memory levers on the native engine.** A whole-model task with segments or parameter offloading
  runs its execution order as stages, one after the other in the executor.
  - The cut is the same one the generic path uses.
  - Only boundary activations are kept, and each segment uploads only its own parameters.
  - Until now the engine refused such a task. Yet the plan recommended segments from the memory
    workers announce, so a CUDA worker took the task, failed it three times, and the run failed:
    ConvNeXt-tiny on 1.5 GB cards, found while measuring this ADR.

## Consequences

**Measured.** Two backward passes of a model cut in two stages, one process, RTX 5060 Ti
(`stage_backward_time`, `cargo test --features cuda --test cuda -- --ignored`):

| Model, batch | Burn's formulas on LibTorch | Native |
|---|---|---|
| ConvNeXt-tiny, 224 px, 32 images | 2 765 ms | 316 ms |
| ResNet18, 224 px, 32 images | 220 ms | 169 ms |
| SmolLM2-135M, 8 × 64 tokens | 1 030 ms | 890 ms |

- **SmolLM2 per stage** is about 35 ms of computation, 150 ms of upload and 250 ms of download.
  The transfers remain the cost, and the copy LibTorch makes on the way back cannot be avoided
  without `unsafe`, which gg-compute forbids.

**On the grid.** 2 CUDA workers, each allowed only part of the model, and a CUDA validator. Every
decision was accepted (32 of 32).

| Round | Before | After |
|---|---|---|
| ConvNeXt-tiny in stages | 4.5–7.4 s | 2.8–4.2 s |
| SmolLM2 in stages | 16.4–21.7 s | 17.0–19.9 s, dominated by transfers and the server |

**What the tests show.** `native_stage_backward_passes_the_model_task_rule_against_the_cpu` runs
the chain of stages for a transformer, a `transformers` Llama, ConvNeXt and ResNet18, cut in two and
three stages. The CUDA chain gives the CPU chain's loss and gradients within the validators' rule:
worst relative norm 1.9e-4 against 2e-2.

**Why ConvNeXt stays slow.** PyTorch itself takes 219 ms per step on ConvNeXt-tiny against 37 ms
for ResNet18 (strict float32, 32 images at 224 px).

- Half of its step is the weight gradient of its 7×7 depthwise convolutions: memory-bound, and poorly
  served by cuDNN in float32.
- Layout conversions add 25 ms.
- Its 4× wider MLP matrices run without tensor cores, since TF32 is forbidden.
- With cuDNN's algorithm autotuning, PyTorch reaches 165 ms. The engine now turns autotuning on
  (`cudnn_set_benchmark`): float32 algorithms only, and a trial of about 20 ms per shape and per
  session.
- It gains only 4 % here (two stages' backward passes, 340 → 326 ms): the executor already lands
  near the faster algorithms. Its forward and backward computation, about 180 ms, is close to
  PyTorch's best. Transfers add about 130 ms.

**Memory levers.** `memory_levers_run_natively` checks a Llama and ConvNeXt with 3 and 4 segments,
with and without offloading. It compares them with the CPU under the same levers.

- The peak of activations is 2 to 3 times lower, and a segment holds only its own parameters.
- On the grid, ConvNeXt-tiny on a CUDA worker limited to 1.5 GB now trains with the 4 segments the
  plan proposes: rounds of 3.5–4.2 s, every decision accepted.
