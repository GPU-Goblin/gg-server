# ADR-0016 — Models described as a computation graph, architecture-agnostic

Status: accepted (2026-09-17), phase P9. Implemented in slices, see docs/ROADMAP.md.
**Amended by ADR-0017** on one point: rule 2 below forbade splitting the graph across machines;
measurement showed that splitting into stages at layer boundaries is slow but feasible, and that only
operator-by-operator distribution is not.

## Context

The current catalog exposes three computation types with typed parameters: matrix multiplication,
training a perceptron with **one** hidden layer of at most 512 units, and a micro-batch gradient. This
is a demonstration, not a research tool: no researcher trains their model this way.

The "no code submitted by a researcher" constraint is not rigidity. Two guarantees depend on it:

- **the promise made to the contributor**: they lend their machine and nothing from a third party runs
  on it;
- **verification by recomputation**: it requires determinism, which arbitrary code destroys.

The platform must therefore become model-agnostic **without** accepting code.

## Decision

**The model is data: a described computation graph, not a program.** The researcher submits a graph
(layers, operators, parameters, loss, optimizer); the executor builds it with Burn and trains it. The
code that runs remains ours, bounded and verifiable; the model is theirs.

Four rules frame this choice.

### 1. A published, versioned operator profile

The platform declares exactly which operators it can execute, at which version, with which attributes.
This profile is published by the API and can be queried offline.

**A graph that uses a missing operator is rejected at submission, naming the missing operator** —
never accepted and then failing midway. This is the direct answer to the risk "if some operations are
not supported, it will not work": the researcher finds out in a second, before reserving anything, and
knows what to ask for.

Every operator added to the profile comes, without exception, with its implementation, its
verification oracle, its determinism test, its tolerance per numeric profile, its FLOP estimator and
its memory estimator. The catalog stops being a list of models and becomes a list of operators, which
scales much better: the same set of about thirty operators covers perceptrons, convolutions,
transformer blocks and time series.

### 2. The unit of work remains the gradient of one micro-batch

The network distributes **micro-batches, not pieces of the graph**. Each task computes the gradient of
an explicit batch from a specific checkpoint, and the coordinator aggregates once, exactly as in
ADR-0011 but for an arbitrary graph.

Two reasons, one about performance, the other about verification:

- **distributing a graph operator by operator does not work over the Internet**: every intermediate
  tensor crosses the network for a few FLOPs per byte. Splitting into stages at layer boundaries, on the
  other hand, only sends the residual stream and is measured in days (ADR-0017); it is the approach
  chosen for models that no single machine can hold.
- **a gradient can be verified; a trajectory much less so.** A single step has no bifurcation point: it
  recomputes identically. Verifying a whole training run by replay would cost twice the compute and
  would remain fragile (measured in P2: one trajectory in thirteen diverges between CPU and GPU).

An accepted consequence: **each stage must fit on a contributor's machine** (ADR-0017), and a model that
fits entirely on one machine remains the most efficient case. The memory envelope is estimated before
scheduling and rejected if it exceeds what the worker advertises. Fine-tuning with adapters (LoRA and
the like) is the realistic path for large text models; full fine-tuning of a very large model on
volunteer machines is not.

### 3. ONNX is an entry point, not the definition

The internal format (`gg-graph/v1`) is explicit, bounded and versioned. ONNX import is a converter into
it, restricted to the published profile, so that researchers can export from their usual tool.
Claiming to "support ONNX" would be false: ONNX has close to two hundred operators and several
versions. The platform supports **a documented profile**, and says so.

### 4. What distributes really well stays distributed

Independently of graphs, three workloads split perfectly and remain the priority: hyperparameter
grids, model ensembles and inference batches. They have none of the constraints of collective
training.

## Consequences

- The current bounds (one hidden layer, 512 units) give way to bounds on the graph: number of nodes,
  number of parameters, estimated memory, FLOPs per task.
- Validation becomes first-class work: inferred shapes, cycles rejected, operators checked against the
  profile, memory envelope estimated — all before any budget is reserved.
- Verification is per operator: each operator carries its tolerance and its admitted numeric profile,
  as `tensor.matmul.v1` and `mlp.grad.v1` do today.
- The work proceeds in slices, each deliverable and tested: profile and validation, execution of the
  first operators, ONNX import, the `graph.grad.v1` unit of work, then integration with the
  coordinator.
