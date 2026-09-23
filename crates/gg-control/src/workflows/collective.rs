//! Collective training coordinator (ADR-0011): owns the canonical checkpoint, opens rounds of
//! `mlp.grad.v1` tasks over disjoint explicit slices, aggregates accepted gradients once with
//! explicit example weighting, applies one optimizer step and publishes the next checkpoint
//! atomically. Late or duplicate responses are never integrated.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use gg_compute::grad::{CoordOptim, OptimizerState, aggregate, apply_step};
use gg_compute::graph::task::{
    self as gtask, Named, NamedOptimizerState, aggregate_named, apply_step_named,
};
use gg_compute::mlp::{MlpTask, Optim, Weights, initial_weights};
use gg_core::*;
use gg_protocol::canonical;
use gg_protocol::domain::Domain;
use gg_protocol::messages::{
    ArtifactRef, MlpGradParamsV1, ResultSubmissionV1, TaskParamsV1, TaskSpecV1,
};
use gg_protocol::messages::{DatasetSpecV1, GraphGradParamsV1};
use gg_protocol::signed::Signed;
use gg_protocol::tensor_file::TensorF32;
use gg_protocol::tensor_file::{BundleF32, Dtype, TensorLimits};

use super::step_cache::{Entry, Moments, named_bytes};
use gg_storage::queries::{artifacts, collective as cq, identities, jobs, misc, tasks};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::AppState;

/// Run configuration (immutable after creation).
#[derive(Clone, Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct CollectiveConfig {
    pub d_input: u32,
    pub d_hidden: u32,
    pub d_output: u32,
    /// `sgd` or `adam` (host optimizer `adam-host-v1`).
    pub optimizer: String,
    pub learning_rate: f64,
    /// Examples per round (effective batch).
    pub batch_per_round: u32,
    /// Micro-batch tasks per round (disjoint slices covering the batch exactly).
    pub tasks_per_round: u32,
    /// Number of rounds (optimizer steps).
    pub rounds: u32,
    /// Seed of the initial weights; round `s` uses `seed + s` for its index stream.
    pub seed: u64,
    /// Retries of a failed round before the run fails.
    pub max_round_retries: u32,
    /// MLP runs only.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub train_x: Option<ArtifactRef>,
    /// MLP runs only.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub train_y: Option<ArtifactRef>,
    pub total_examples: u64,
    pub priority: i32,
    /// Present for a model described by a graph (`graph.grad.v1`, ADR-0016/0017).
    #[serde(default)]
    pub graph: Option<GraphRunSpec>,
}

/// What a graph-described run carries: the description, its measured costs and its datasets.
#[derive(Clone, Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct GraphRunSpec {
    /// The `gg-graph/v1` description.
    #[schema(value_type = Object)]
    pub graph: ArtifactRef,
    /// Trainable parameters.
    pub parameters: u64,
    /// Exact size of a checkpoint bundle (`GGB1`) of this graph, frozen parameters included.
    pub checkpoint_bytes: u64,
    /// Exact size of a gradient bundle in f32: the trained parameters only (ADR-0019). Also the
    /// size of the optimizer state.
    pub gradient_bytes: u64,
    /// Tasks send their gradients in bf16 (ADR-0035). Absent from runs created before: f32.
    #[serde(default)]
    pub gradient_bf16: bool,
    /// Exact size of a gradient bundle in bf16.
    #[serde(default)]
    pub gradient_bf16_bytes: u64,
    /// Segments of activation recomputation asked of every task.
    #[serde(default = "one_segment")]
    pub segments: u32,
    /// Whether tasks upload parameters segment by segment instead of all at once.
    #[serde(default)]
    pub offload_parameters: bool,
    /// From the validation report.
    pub training_flops_per_example: u64,
    /// From the validation report.
    pub activation_bytes_per_example: u64,
    /// Dataset tensors (graph inputs and the loss target), full shapes.
    #[schema(value_type = Object)]
    pub datasets: Vec<DatasetSpecV1>,
    /// Their artifacts, by name.
    #[schema(value_type = Object)]
    pub dataset_refs: BTreeMap<String, ArtifactRef>,
    /// Accelerator memory a task needs, estimated for the largest micro-batch with the chosen
    /// levers (`graph::memory::task_memory`). Only workers announcing at least this much are
    /// offered the tasks.
    #[serde(default)]
    pub task_device_bytes: u64,
    /// Minimal `gg-ops/N` level able to execute the description.
    #[serde(default = "base_profile")]
    pub required_profile: u32,
    /// Target size of a checkpoint shard (C2); `0` keeps one `checkpoint` bundle.
    #[serde(default)]
    pub shard_bytes: u64,
    /// Stage boundaries of a model split between machines (D2); empty for data parallelism.
    #[serde(default)]
    pub stage_bounds: Vec<u32>,
    /// Matrices cut into tiles by the server (D3), in the order they were cut.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub tiling: Vec<gg_compute::graph::tiling::Tiled>,
    /// The description as submitted, when the server rewrote it (tiles, D3; functions inlined,
    /// ADR-0025).
    #[serde(default)]
    #[schema(value_type = Object)]
    pub source_graph: Option<ArtifactRef>,
    /// Tasks receive their micro-batch rows, selected by the coordinator, instead of whole
    /// datasets (C4).
    #[serde(default)]
    pub batch_inputs: bool,
    /// Datasets uploaded as several row ranges, concatenated in order (C4). Empty when each
    /// dataset is one artifact (`dataset_refs`).
    #[serde(default)]
    #[schema(value_type = Object)]
    pub dataset_parts: BTreeMap<String, Vec<ArtifactRef>>,
}

fn base_profile() -> u32 {
    1
}

impl GraphRunSpec {
    /// Element type of the gradients tasks send.
    pub fn gradient_dtype(&self) -> Dtype {
        if self.gradient_bf16 {
            Dtype::Bf16
        } else {
            Dtype::F32
        }
    }

    /// Bytes of a whole gradient as tasks send it.
    pub fn task_gradient_bytes(&self) -> u64 {
        if self.gradient_bf16 {
            self.gradient_bf16_bytes
        } else {
            self.gradient_bytes
        }
    }
}

/// References of a published checkpoint.
#[derive(Clone, Debug, Serialize, Deserialize, Default, utoipa::ToSchema)]
pub struct CheckpointRefs {
    /// `w1,b1,w2,b2`.
    #[schema(value_type = Object)]
    pub weights: BTreeMap<String, ArtifactRef>,
    /// Adam first moments (absent for SGD).
    #[schema(value_type = Object)]
    pub opt_m: Option<BTreeMap<String, ArtifactRef>>,
    /// Adam second moments.
    #[schema(value_type = Object)]
    pub opt_v: Option<BTreeMap<String, ArtifactRef>>,
    /// Optimizer steps applied.
    pub opt_t: u32,
    /// Optimizer steps by which the moments above lag behind the weights: 0, or 1 while a run's
    /// state is kept in memory (ADR-0036). The moments of the last step are written behind the next
    /// round and recorded once durable; after a restart the server rebuilds them from the recorded
    /// gradients of the round that produced the weights. Absent when 0.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub opt_lag: u32,
}

fn is_zero_u32(v: &u32) -> bool {
    *v == 0
}

fn one_segment() -> u32 {
    1
}

/// Request to create a run.
#[derive(Clone, Debug, Deserialize, utoipa::ToSchema)]
pub struct CreateCollectiveRequest {
    #[serde(default)]
    pub name: String,
    /// MLP run: training inputs.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub train_x: Option<ArtifactRef>,
    /// MLP run: training targets.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub train_y: Option<ArtifactRef>,
    /// MLP run: hidden width.
    #[serde(default)]
    pub d_hidden: Option<u32>,
    /// Graph run: the `gg-graph/v1` description artifact. When present, `datasets` is required
    /// and the MLP fields are ignored.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub graph: Option<ArtifactRef>,
    /// Graph run: one tensor artifact per graph input and one for the loss target, by name.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub datasets: Option<BTreeMap<String, ArtifactRef>>,
    /// Graph run: start from these parameters (an `GGB1` bundle) instead of the seeded init.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub initial_checkpoint: Option<ArtifactRef>,
    /// Graph run: start from these parameters, one `GGT1` tensor per parameter by name. The form
    /// for a model too large for one upload: the server assembles the checkpoint shard by shard.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub initial_parameters: Option<BTreeMap<String, ArtifactRef>>,
    /// Graph run: target size of a checkpoint shard (C2). Absent: one bundle up to 256 MiB,
    /// shards above. `0`: one bundle, whatever the size (up to 1 GiB).
    #[serde(default)]
    pub shard_bytes: Option<u64>,
    /// Graph run: datasets as consecutive row ranges, one list per name, for datasets larger than
    /// one upload (C4). Exclusive with `datasets` for a given name.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub dataset_parts: Option<BTreeMap<String, Vec<ArtifactRef>>>,
    /// Graph run: send each task its micro-batch rows instead of whole datasets (C4). Absent: yes
    /// when datasets exceed 256 MiB or come in parts.
    #[serde(default)]
    pub batch_inputs: Option<bool>,
    /// Graph run split between machines (D2): explicit stage boundaries in the execution order…
    #[serde(default)]
    pub stage_bounds: Option<Vec<u32>>,
    /// …or the memory of one machine, from which the server cuts the stages itself.
    #[serde(default)]
    pub stage_memory_bytes: Option<u64>,
    /// Graph run (D3): cut every matrix larger than this many bytes into tiles, in the description
    /// itself, before planning the stages. The server rewrites the description and splits the
    /// initial weights; the published checkpoint holds the tiles (`config.graph.tiling` says how
    /// to merge them back).
    #[serde(default)]
    pub tile_max_param_bytes: Option<u64>,
    /// Graph run: segments of activation recomputation asked of every task (ADR-0019). `1` keeps
    /// every activation; above that, less memory for about a third more compute, same gradient.
    #[serde(default = "one_segment")]
    pub segments: u32,
    /// Graph run: upload parameters segment by segment instead of all at once.
    #[serde(default)]
    pub offload_parameters: bool,
    /// Graph run: element type of the gradients tasks send, `bf16` (the default: half the bytes,
    /// ADR-0035) or `f32`. The server averages and keeps weights and optimizer state in f32 either
    /// way.
    #[serde(default)]
    pub gradient_precision: Option<String>,
    #[serde(default = "default_optimizer")]
    pub optimizer: String,
    #[serde(default = "default_lr")]
    pub learning_rate: f64,
    pub batch_per_round: u32,
    pub tasks_per_round: u32,
    pub rounds: u32,
    #[serde(default = "default_seed")]
    pub seed: u64,
    #[serde(default = "default_retries")]
    pub max_round_retries: u32,
    #[serde(default)]
    pub priority: i32,
}
fn default_optimizer() -> String {
    "adam".into()
}
fn default_lr() -> f64 {
    0.02
}
fn default_seed() -> u64 {
    1
}
fn default_retries() -> u32 {
    2
}

/// Read the first bytes of an artifact readable by the project, and its size.
async fn artifact_head(
    state: &AppState,
    tx: &mut gg_storage::PgConnection,
    project: ProjectId,
    r: &ArtifactRef,
    head: usize,
) -> Result<Vec<u8>, ApiError> {
    use tokio::io::AsyncReadExt;
    let row = artifacts::get_artifact(tx, &r.hash)
        .await?
        .ok_or_else(|| ApiError::Unprocessable("artifact unknown".into()))?;
    if row.size as u64 != r.size
        || !artifacts::artifact_readable_by_project(tx, &r.hash, project).await?
    {
        return Err(ApiError::Forbidden(
            "artifact not readable by this project or size mismatch".into(),
        ));
    }
    let mut f = state.objects().open_read(&r.hash).await?;
    let mut buf = vec![0u8; head.min(r.size as usize)];
    f.read_exact(&mut buf)
        .await
        .map_err(|e| ApiError::Internal(format!("artifact read: {e}")))?;
    Ok(buf)
}

/// Create a run over a model described by a graph: validate the description, check every dataset
/// against it, materialize the initial checkpoint (seeded, or the one supplied), open round 0.
async fn create_graph_run(
    state: &AppState,
    researcher: ResearcherId,
    project: ProjectId,
    req: CreateCollectiveRequest,
) -> Result<RunView, ApiError> {
    use gg_compute::graph;
    let graph_ref = req
        .graph
        .ok_or_else(|| ApiError::Unprocessable("graph missing".into()))?;
    let empty = BTreeMap::new();
    let datasets = req.datasets.as_ref().unwrap_or(&empty);
    let empty_parts = BTreeMap::new();
    let dataset_parts = req.dataset_parts.as_ref().unwrap_or(&empty_parts);
    if datasets.is_empty() && dataset_parts.is_empty() {
        return Err(ApiError::Unprocessable("a graph run needs datasets".into()));
    }
    if req.batch_per_round.div_ceil(req.tasks_per_round) > gtask::MAX_EXAMPLE_COUNT
        || req.tasks_per_round > req.batch_per_round
    {
        return Err(ApiError::Unprocessable(format!(
            "each micro-batch must have 1..={} examples",
            gtask::MAX_EXAMPLE_COUNT
        )));
    }
    let optim = CoordOptim::parse(&req.optimizer)
        .ok_or_else(|| ApiError::Unprocessable("optimizer must be sgd or adam".into()))?;
    if !(req.learning_rate.is_finite() && req.learning_rate > 0.0 && req.learning_rate <= 1.0) {
        return Err(ApiError::Unprocessable(
            "learning_rate must be in (0, 1]".into(),
        ));
    }
    if graph_ref.size == 0 || graph_ref.size > gtask::MAX_GRAPH_BYTES {
        return Err(ApiError::Unprocessable(
            "graph artifact size out of range".into(),
        ));
    }
    let mut tx = state.db().begin().await?;
    let proj = identities::get_project(&mut tx, project)
        .await?
        .ok_or_else(|| ApiError::NotFound("project".into()))?;
    if !proj.allowed_kinds.iter().any(|k| k == gtask::KIND) {
        return Err(ApiError::Forbidden(format!(
            "project may not submit {}",
            gtask::KIND
        )));
    }
    // The description: validated exactly as `gg job check-model` does, refusals named.
    let graph_bytes =
        artifact_head(state, &mut tx, project, &graph_ref, graph_ref.size as usize).await?;
    let submitted: graph::ModelGraphV1 = serde_json::from_slice(&graph_bytes)
        .map_err(|e| ApiError::Unprocessable(format!("graph is not gg-graph/v1: {e}")))?;
    // Functions (ADR-0025): inlined here, once; workers only ever see a flat description.
    let inlined = !submitted.functions.is_empty();
    let model = graph::functions::inline(&submitted)
        .map_err(|e| ApiError::Unprocessable(format!("graph refused: {e}")))?;
    let report = graph::validate(&model)
        .map_err(|e| ApiError::Unprocessable(format!("graph refused: {e}")))?;
    // Matrices tiled in the description (D3): the server rewrites the description and keeps it
    // as a project artifact; the initial weights will be tiled the same way.
    let source_model = model.clone();
    let source_graph = graph_ref;
    let (model, report, tiling, rewritten) = match req.tile_max_param_bytes {
        Some(max) => {
            let (tiled, tiling) = graph::tiling::auto_tile(&model, max)
                .map_err(|e| ApiError::Unprocessable(format!("tiling: {e}")))?;
            let report = graph::validate(&tiled)
                .map_err(|e| ApiError::Unprocessable(format!("tiled graph refused: {e}")))?;
            (tiled, report, tiling, Some("tiling"))
        }
        None => (model, report, Vec::new(), inlined.then_some("functions")),
    };
    // A rewritten description (tiles or inlined functions) is kept as a project artifact: it is
    // the one workers download.
    let graph_ref = match rewritten {
        Some(why) => {
            let bytes =
                serde_json::to_vec(&model).map_err(|e| ApiError::Internal(e.to_string()))?;
            let stored = state
                .objects()
                .put_bytes(&bytes, gtask::MAX_GRAPH_BYTES)
                .await?;
            artifacts::insert_artifact(
                &mut tx,
                &stored.hash,
                stored.size,
                "application/json",
                "gg-graph-v1",
                "project",
                "",
                Some(project),
                &format!("coordinator:{why}:{researcher}"),
            )
            .await?;
            ArtifactRef {
                hash: stored.hash,
                size: stored.size,
            }
        }
        None => graph_ref,
    };
    // Checkpoint split into shards beyond 256 MiB (C2): the server never holds more than one
    // shard at a time.
    let shard_bytes = req.shard_bytes.unwrap_or_else(|| {
        graph::shards::default_shard_bytes(gtask::checkpoint_bundle_size(&model))
    });
    // Stages (D2): given bounds, or a split within the memory of one machine.
    let micro = req.batch_per_round.div_ceil(req.tasks_per_round) as u64;
    let stage_bounds: Vec<u32> = match (&req.stage_bounds, req.stage_memory_bytes) {
        (Some(b), None) => b.clone(),
        (None, Some(cap)) => graph::stages::partition(&model, &report, micro, cap)
            .map_err(|e| ApiError::Unprocessable(format!("stages: {e}")))?,
        (None, None) => Vec::new(),
        (Some(_), Some(_)) => {
            return Err(ApiError::Unprocessable(
                "stage_bounds and stage_memory_bytes are exclusive".into(),
            ));
        }
    };
    let staged = !stage_bounds.is_empty();
    let plan = if staged {
        if !proj
            .allowed_kinds
            .iter()
            .any(|k| k == gg_compute::graph::stage_task::KIND)
        {
            return Err(ApiError::Forbidden(format!(
                "project may not submit {}",
                gg_compute::graph::stage_task::KIND
            )));
        }
        let st = graph::stages::stages(&model, &report, &stage_bounds)
            .map_err(|e| ApiError::Unprocessable(format!("stages: {e}")))?;
        graph::shards::plan_stages(&model, &st)
            .map_err(|e| ApiError::Unprocessable(format!("stages: {e}")))?
    } else {
        graph::shards::plan(&model, &report, shard_bytes)
            .map_err(|e| ApiError::Unprocessable(format!("checkpoint shards: {e}")))?
    };
    let checkpoint_bytes = plan.checkpoint_bytes();
    if !plan.sharded && checkpoint_bytes > gtask::MAX_CHECKPOINT_BYTES {
        return Err(ApiError::Unprocessable(format!(
            "checkpoint of {checkpoint_bytes} bytes exceeds {} bytes in one bundle; leave \
shard_bytes unset or set it to split the checkpoint",
            gtask::MAX_CHECKPOINT_BYTES
        )));
    }
    // Datasets: one per graph input plus the target, shapes read from the headers.
    let mut specs: Vec<&graph::TensorSpec> = model.inputs.iter().collect();
    specs.push(model.loss.target());
    if datasets.len() + dataset_parts.len() != specs.len() {
        return Err(ApiError::Unprocessable(format!(
            "the graph needs {} dataset tensors ({}), {} given",
            specs.len(),
            specs
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            datasets.len()
        )));
    }
    let mut dataset_specs = Vec::with_capacity(specs.len());
    let mut total: Option<u64> = None;
    for spec in specs {
        // A dataset: one artifact, or shards of consecutive rows (C4).
        let parts: Vec<ArtifactRef> =
            match (datasets.get(&spec.name), dataset_parts.get(&spec.name)) {
                (Some(r), None) => vec![*r],
                (None, Some(list)) if !list.is_empty() && list.len() <= 4096 => list.clone(),
                (Some(_), Some(_)) => {
                    return Err(ApiError::Unprocessable(format!(
                        "dataset '{}' given both whole and in parts",
                        spec.name
                    )));
                }
                _ => {
                    return Err(ApiError::Unprocessable(format!(
                        "dataset '{}' missing",
                        spec.name
                    )));
                }
            };
        let mut shape: Vec<u64> = Vec::new();
        for (k, r) in parts.iter().enumerate() {
            let head = artifact_head(state, &mut tx, project, r, 6 + 8 * 4).await?;
            let (part_shape, size) = TensorF32::peek_shape(
                &head,
                &TensorLimits {
                    max_elements: gtask::MAX_DATASET_BYTES / 4,
                },
            )
            .map_err(|e| ApiError::Unprocessable(format!("dataset '{}': {e}", spec.name)))?;
            if size != r.size {
                return Err(ApiError::Unprocessable(format!(
                    "dataset '{}' part {k} header announces {size} bytes, artifact has {}",
                    spec.name, r.size
                )));
            }
            if k == 0 {
                shape = part_shape;
            } else if part_shape.len() != shape.len() || part_shape[1..] != shape[1..] {
                return Err(ApiError::Unprocessable(format!(
                    "dataset '{}' part {k} has shape {part_shape:?}, earlier parts {shape:?}",
                    spec.name
                )));
            } else {
                shape[0] += part_shape[0];
            }
        }
        if gg_compute::catalog::expected_rgt_size(&shape) > gtask::MAX_DATASET_BYTES {
            return Err(ApiError::Unprocessable(format!(
                "dataset '{}' above {} bytes",
                spec.name,
                gtask::MAX_DATASET_BYTES
            )));
        }
        if shape.len() != spec.shape.len() {
            return Err(ApiError::Unprocessable(format!(
                "dataset '{}' has rank {}, the graph expects {}",
                spec.name,
                shape.len(),
                spec.shape.len()
            )));
        }
        for (i, (got, want)) in shape.iter().zip(spec.shape.iter()).enumerate().skip(1) {
            if *want != 0 && got != want {
                return Err(ApiError::Unprocessable(format!(
                    "dataset '{}' dimension {i} is {got}, the graph expects {want}",
                    spec.name
                )));
            }
        }
        match total {
            None => total = Some(shape[0]),
            Some(n) if n != shape[0] => {
                return Err(ApiError::Unprocessable(format!(
                    "dataset '{}' has {} examples, others have {n}",
                    spec.name, shape[0]
                )));
            }
            _ => {}
        }
        dataset_specs.push(DatasetSpecV1 {
            name: spec.name.clone(),
            shape,
            dtype: spec.dtype.clone(),
        });
    }
    let total_examples = total.unwrap_or(0);
    if total_examples == 0 || total_examples > gtask::MAX_TOTAL_EXAMPLES {
        return Err(ApiError::Unprocessable("dataset size out of range".into()));
    }
    if req.batch_per_round as u64 > total_examples {
        return Err(ApiError::Unprocessable(
            "batch_per_round exceeds the dataset".into(),
        ));
    }
    let segments = req.segments.clamp(1, gtask::MAX_SEGMENTS);
    let task_memory = graph::memory::task_memory(
        &model,
        &report,
        req.batch_per_round.div_ceil(req.tasks_per_round) as u64,
        graph::exec::ExecOptions {
            segments: segments as usize,
            offload_parameters: req.offload_parameters,
        },
    );
    let dataset_bytes: u64 = dataset_specs
        .iter()
        .map(|d| gg_compute::catalog::expected_rgt_size(&d.shape))
        .sum();
    let batch_inputs = staged
        || req
            .batch_inputs
            .unwrap_or(!dataset_parts.is_empty() || dataset_bytes > 256 << 20);
    if staged && req.batch_inputs == Some(false) {
        return Err(ApiError::Unprocessable(
            "a staged run gives every stage its micro-batch rows: batch_inputs must be true".into(),
        ));
    }
    if !batch_inputs && !dataset_parts.is_empty() {
        return Err(ApiError::Unprocessable(
            "datasets in parts need batch_inputs: each task receives its rows".into(),
        ));
    }
    let gradient_bf16 = match req.gradient_precision.as_deref() {
        None | Some("bf16") => true,
        Some("f32") => false,
        Some(other) => {
            return Err(ApiError::Unprocessable(format!(
                "gradient_precision '{other}': expected 'bf16' or 'f32'"
            )));
        }
    };
    // Memory of a task: the largest stage when staged; the whole model otherwise.
    let task_device_bytes = if staged {
        let st = graph::stages::stages(&model, &report, &stage_bounds)
            .map_err(|e| ApiError::Unprocessable(format!("stages: {e}")))?;
        st.iter()
            .map(|s| graph::stages::stage_memory(&model, &report, s, micro))
            .max()
            .unwrap_or(0)
    } else {
        task_memory.total()
    };
    let spec = GraphRunSpec {
        tiling: tiling.clone(),
        source_graph: rewritten.is_some().then_some(source_graph),
        stage_bounds,
        batch_inputs,
        dataset_parts: dataset_parts.clone(),
        task_device_bytes,
        required_profile: graph::memory::required_profile(&model),
        graph: graph_ref,
        parameters: report.parameters,
        checkpoint_bytes,
        gradient_bytes: plan.gradient_bytes(),
        gradient_bf16,
        gradient_bf16_bytes: plan.gradient_size(Dtype::Bf16),
        shard_bytes,
        segments,
        offload_parameters: req.offload_parameters,
        training_flops_per_example: report.training_flops_per_example,
        activation_bytes_per_example: report.activation_bytes_per_example,
        datasets: dataset_specs,
        dataset_refs: datasets.clone(),
    };
    let cfg = CollectiveConfig {
        d_input: 0,
        d_hidden: 0,
        d_output: 0,
        optimizer: req.optimizer.clone(),
        learning_rate: req.learning_rate,
        batch_per_round: req.batch_per_round,
        tasks_per_round: req.tasks_per_round,
        rounds: req.rounds,
        seed: req.seed,
        max_round_retries: req.max_round_retries.min(10),
        train_x: None,
        train_y: None,
        total_examples,
        priority: req.priority,
        graph: Some(spec.clone()),
    };
    let run_id = Uuid::now_v7();
    // Initial checkpoint, shard by shard: supplied as one bundle, supplied as one tensor per
    // parameter, or seeded by the protocol. Only one shard is held in memory at a time, except for
    // a single supplied bundle (at most 1 GiB).
    let mut weights = BTreeMap::new();
    let single: Option<Named> = match &req.initial_checkpoint {
        Some(r) => {
            if req.initial_parameters.is_some() {
                return Err(ApiError::Unprocessable(
                    "initial_checkpoint and initial_parameters are exclusive".into(),
                ));
            }
            if r.size != gtask::checkpoint_bundle_size(&source_model) {
                return Err(ApiError::Unprocessable(format!(
                    "initial_checkpoint is {} bytes, this graph's checkpoint is {}",
                    r.size,
                    gtask::checkpoint_bundle_size(&source_model)
                )));
            }
            if r.size > gtask::MAX_CHECKPOINT_BYTES {
                return Err(ApiError::Unprocessable(
                    "initial_checkpoint above 1 GiB: send initial_parameters instead".into(),
                ));
            }
            let bytes = artifact_head(state, &mut tx, project, r, r.size as usize).await?;
            let bundle = BundleF32::decode(
                &bytes,
                &TensorLimits {
                    max_elements: report.parameters,
                },
            )
            .map_err(|e| ApiError::Unprocessable(format!("initial_checkpoint: {e}")))?;
            gtask::check_bundle_matches(&source_model, &bundle, "initial_checkpoint")
                .map_err(|e| ApiError::Unprocessable(e.to_string()))?;
            if !plan.sharded && tiling.is_empty() {
                weights.insert(plan.checkpoint_name(0), *r);
            }
            Some(
                graph::tiling::split_weights(&bundle.entries, &tiling)
                    .map_err(ApiError::Unprocessable)?,
            )
        }
        None => None,
    };
    if let Some(given) = &req.initial_parameters {
        let mut expected: Vec<&str> = source_model
            .parameters
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        let mut got: Vec<&str> = given.keys().map(String::as_str).collect();
        expected.sort_unstable();
        got.sort_unstable();
        if expected != got {
            return Err(ApiError::Unprocessable(
                "initial_parameters must name every parameter of the graph, and only them".into(),
            ));
        }
    }
    if weights.is_empty() {
        for (i, shard) in plan.shards.iter().enumerate() {
            let mut part = Named::new();
            for name in &shard.params {
                let spec_p = model
                    .parameters
                    .iter()
                    .find(|p| &p.name == name)
                    .ok_or_else(|| ApiError::Internal(format!("shard names '{name}'")))?;
                let t = match (&single, &req.initial_parameters) {
                    (Some(all), _) => all[name].clone(),
                    (None, Some(given)) if !given.contains_key(name) => {
                        // A tile (D3): the original weight, cut up; only this matrix is held in
                        // memory, for as long as it takes to tile it.
                        let t = tiling
                            .iter()
                            .find(|t| name.starts_with(&format!("{}.tile", t.param)))
                            .ok_or_else(|| ApiError::Internal(format!("'{name}' has no origin")))?;
                        let orig = source_model
                            .parameters
                            .iter()
                            .find(|p| p.name == t.param)
                            .ok_or_else(|| ApiError::Internal(t.param.clone()))?;
                        let r = &given[&t.param];
                        if r.size != gg_compute::catalog::expected_rgt_size(&orig.shape) {
                            return Err(ApiError::Unprocessable(format!(
                                "initial_parameters/{}: size does not match its shape",
                                t.param
                            )));
                        }
                        artifact_head(state, &mut tx, project, r, 0).await?;
                        let full = load_tensor(state, r, &orig.shape).await?;
                        full.check_finite().map_err(|e| {
                            ApiError::Unprocessable(format!("initial_parameters/{}: {e}", t.param))
                        })?;
                        let one = BTreeMap::from([(t.param.clone(), full)]);
                        graph::tiling::split_weights(&one, std::slice::from_ref(t))
                            .map_err(ApiError::Unprocessable)?
                            .remove(name)
                            .ok_or_else(|| ApiError::Internal(name.clone()))?
                    }
                    (None, Some(given)) => {
                        let r = &given[name];
                        if r.size != gg_compute::catalog::expected_rgt_size(&spec_p.shape) {
                            return Err(ApiError::Unprocessable(format!(
                                "initial_parameters/{name}: {} bytes, shape {:?} needs {}",
                                r.size,
                                spec_p.shape,
                                gg_compute::catalog::expected_rgt_size(&spec_p.shape)
                            )));
                        }
                        artifact_head(state, &mut tx, project, r, 0).await?;
                        let t = load_tensor(state, r, &spec_p.shape).await?;
                        t.check_finite().map_err(|e| {
                            ApiError::Unprocessable(format!("initial_parameters/{name}: {e}"))
                        })?;
                        t
                    }
                    (None, None) => gg_compute::graph::exec::init_parameter(spec_p, cfg.seed),
                };
                part.insert(name.clone(), t);
            }
            let r = store_bundle(state, project, run_id, &part).await?;
            if r.size != shard.checkpoint_bytes {
                return Err(ApiError::Internal(format!(
                    "shard {i} is {} bytes, plan says {}",
                    r.size, shard.checkpoint_bytes
                )));
            }
            weights.insert(plan.checkpoint_name(i), r);
        }
    }
    let ckpt = CheckpointRefs {
        weights,
        opt_m: None,
        opt_v: None,
        opt_t: 0,
        opt_lag: 0,
    };
    let experiment_id = ExperimentId::new();
    jobs::create_experiment(
        &mut tx,
        experiment_id,
        project,
        &format!("collective:{}", req.name),
        Some(researcher),
    )
    .await?;
    cq::insert_run(
        &mut tx,
        run_id,
        project.0,
        experiment_id.0,
        Some(researcher.0),
        &req.name,
        &serde_json::to_value(&cfg).map_err(|e| ApiError::Internal(e.to_string()))?,
        &serde_json::to_value(&ckpt).map_err(|e| ApiError::Internal(e.to_string()))?,
    )
    .await?;
    cq::insert_checkpoint(
        &mut tx,
        run_id,
        0,
        &serde_json::to_value(&ckpt).map_err(|e| ApiError::Internal(e.to_string()))?,
        None,
    )
    .await?;
    let run = cq::get_run(&mut tx, run_id)
        .await?
        .ok_or_else(|| ApiError::Internal("run missing".into()))?;
    open_round(state, &mut tx, &run, &cfg, &ckpt, 0, 1).await?;
    misc::audit(&mut tx, &format!("researcher:{researcher}"), "collective.create", &run_id.to_string(), serde_json::json!({ "project_id": project, "rounds": cfg.rounds, "tasks_per_round": cfg.tasks_per_round, "kind": gtask::KIND, "parameters": report.parameters })).await?;
    let _ = optim;
    tx.commit().await?;
    state.signal_work();
    run_view(state, run_id).await
}

/// Round view.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
pub struct RoundView {
    pub step: i32,
    pub attempt: i32,
    pub job_id: JobId,
    pub state: String,
    pub round_seed: u64,
}

/// Run view.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
pub struct RunView {
    pub run_id: Uuid,
    pub project_id: ProjectId,
    pub experiment_id: ExperimentId,
    pub name: String,
    pub state: String,
    pub step: i32,
    pub rounds_total: u32,
    pub config: CollectiveConfig,
    pub checkpoint: CheckpointRefs,
    pub rounds: Vec<RoundView>,
    /// Steps with a published checkpoint (`0..=step`, one per step by primary key).
    pub checkpoint_steps: Vec<i32>,
    pub failure: String,
}

/// Disjoint slices covering `batch` examples with `k` tasks (remainder spread over the first slices).
pub fn slices(batch: u32, k: u32) -> Vec<(u64, u32)> {
    let base = batch / k;
    let rem = batch % k;
    let mut out = Vec::with_capacity(k as usize);
    let mut start = 0u64;
    for i in 0..k {
        let n = base + u32::from(i < rem);
        out.push((start, n));
        start += n as u64;
    }
    out
}

pub(crate) fn round_seed(cfg: &CollectiveConfig, step: i32) -> u64 {
    cfg.seed.wrapping_add(step as u64)
}

pub(crate) async fn store_tensor(
    state: &AppState,
    project: ProjectId,
    run: Uuid,
    t: &TensorF32,
) -> Result<ArtifactRef, ApiError> {
    let bytes = t.encode();
    let stored = state.objects().put_bytes(&bytes, 64 << 20).await?;
    let mut c = state
        .db()
        .pool()
        .acquire()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    artifacts::insert_artifact(
        &mut c,
        &stored.hash,
        stored.size,
        "application/octet-stream",
        "ggt1-f32",
        "project",
        "",
        Some(project),
        &format!("coordinator:{run}"),
    )
    .await?;
    Ok(ArtifactRef {
        hash: stored.hash,
        size: stored.size,
    })
}

async fn store_weights(
    state: &AppState,
    project: ProjectId,
    run: Uuid,
    w: &Weights,
) -> Result<BTreeMap<String, ArtifactRef>, ApiError> {
    let mut m = BTreeMap::new();
    for (name, t) in w.named() {
        m.insert(name.to_owned(), store_tensor(state, project, run, t).await?);
    }
    Ok(m)
}

pub(crate) async fn store_bundle(
    state: &AppState,
    project: ProjectId,
    run: Uuid,
    named: &Named,
) -> Result<ArtifactRef, ApiError> {
    let bytes = BundleF32::encode_entries_as(named, Dtype::F32)
        .map_err(|e| ApiError::Internal(format!("checkpoint bundle: {e}")))?;
    let stored = state
        .objects()
        .put_bytes(&bytes, gtask::MAX_CHECKPOINT_BYTES)
        .await?;
    record_bundle(state, project, run, stored).await
}

/// [`store_bundle`] off the async threads, for the server's step: encoding, hashing and the
/// fsynced write of a 540 MB bundle run on a blocking thread, the hash beside the write. The
/// tensors come back to the caller.
pub(crate) async fn store_bundle_owned(
    state: &AppState,
    project: ProjectId,
    run: Uuid,
    named: Named,
) -> Result<(ArtifactRef, Named), ApiError> {
    let objects = state.objects().clone();
    let (stored, named) = tokio::task::spawn_blocking(move || {
        let bytes = BundleF32::encode_entries_as(&named, Dtype::F32)
            .map_err(|e| ApiError::Internal(format!("checkpoint bundle: {e}")))?;
        let stored = objects.put_bytes_blocking(&bytes, gtask::MAX_CHECKPOINT_BYTES)?;
        Ok::<_, ApiError>((stored, named))
    })
    .await
    .map_err(|e| ApiError::Internal(format!("store task: {e}")))??;
    Ok((record_bundle(state, project, run, stored).await?, named))
}

async fn record_bundle(
    state: &AppState,
    project: ProjectId,
    run: Uuid,
    stored: gg_storage::objects::StoredObject,
) -> Result<ArtifactRef, ApiError> {
    let mut c = state
        .db()
        .pool()
        .acquire()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    artifacts::insert_artifact(
        &mut c,
        &stored.hash,
        stored.size,
        "application/octet-stream",
        "ggb1-f32",
        "project",
        "",
        Some(project),
        &format!("coordinator:{run}"),
    )
    .await?;
    Ok(ArtifactRef {
        hash: stored.hash,
        size: stored.size,
    })
}

/// Weights and optimizer state of one checkpoint shard, read at once.
pub(crate) async fn load_step_state(
    state: &AppState,
    g: &GraphRunSpec,
    ckpt: &CheckpointRefs,
    key: &str,
    shard: &gg_compute::graph::shards::Shard,
) -> Result<(Named, NamedOptimizerState), ApiError> {
    let r = ckpt
        .weights
        .get(key)
        .copied()
        .ok_or_else(|| ApiError::Internal(format!("checkpoint lacks {key}")))?;
    let weights = load_bundle(state, &r, g, shard.checkpoint_bytes);
    let (Some(m), Some(v)) = (&ckpt.opt_m, &ckpt.opt_v) else {
        let opt = NamedOptimizerState {
            m: None,
            v: None,
            t: ckpt.opt_t,
        };
        return Ok((weights.await?, opt));
    };
    let get = |map: &BTreeMap<String, ArtifactRef>| {
        map.get(key)
            .copied()
            .ok_or_else(|| ApiError::Internal(format!("optimizer lacks {key}")))
    };
    let (m, v) = (get(m)?, get(v)?);
    let (weights, m, v) = tokio::try_join!(
        weights,
        load_bundle(state, &m, g, shard.gradient_bytes),
        load_bundle(state, &v, g, shard.gradient_bytes)
    )?;
    Ok((
        weights,
        NamedOptimizerState {
            m: Some(m),
            v: Some(v),
            t: ckpt.opt_t,
        },
    ))
}

/// The mean of the tasks' gradients and one optimizer step, on a blocking thread (the step itself
/// spreads over the machine's threads).
pub(crate) async fn step_blocking(
    optim: CoordOptim,
    lr: f64,
    (mut weights, mut opt): (Named, NamedOptimizerState),
    parts: Vec<(u32, Named)>,
) -> Result<(Named, NamedOptimizerState), ApiError> {
    tokio::task::spawn_blocking(move || {
        let grad = aggregate_named(&parts)?;
        drop(parts);
        apply_step_named(optim, lr, &mut weights, &grad, &mut opt)?;
        Ok::<_, ApiError>((weights, opt))
    })
    .await
    .map_err(|e| ApiError::Internal(format!("step task: {e}")))?
}

/// References of a shard's new weights and optimizer state.
pub(crate) struct StoredStep {
    pub weights: ArtifactRef,
    pub m: Option<ArtifactRef>,
    pub v: Option<ArtifactRef>,
}

/// Stores a shard's new weights and optimizer state at once, each durably before it is
/// referenced.
pub(crate) async fn store_step_state(
    state: &AppState,
    project: ProjectId,
    run: Uuid,
    weights: Named,
    opt: NamedOptimizerState,
) -> Result<StoredStep, ApiError> {
    let store_opt = |t: Option<Named>| async move {
        match t {
            Some(t) => Ok::<_, ApiError>(Some(store_bundle_owned(state, project, run, t).await?.0)),
            None => Ok(None),
        }
    };
    let (w, m, v) = tokio::try_join!(
        store_bundle_owned(state, project, run, weights),
        store_opt(opt.m),
        store_opt(opt.v)
    )?;
    Ok(StoredStep { weights: w.0, m, v })
}

/// Timings of a round's step, summed over shards.
#[derive(Default)]
pub(crate) struct StepTimes {
    pub read: Duration,
    pub compute: Duration,
    pub store: Duration,
}

impl StepTimes {
    /// Logs where the server's step went: it is on every round's critical path.
    pub fn log(&self, run: Uuid, step: i32) {
        tracing::info!(
            run = %run,
            step,
            read_ms = self.read.as_millis() as u64,
            compute_ms = self.compute.as_millis() as u64,
            store_ms = self.store.as_millis() as u64,
            "optimizer step timed"
        );
    }
}

/// What a shard's step records in the new checkpoint.
pub(crate) struct ShardStep {
    weights: ArtifactRef,
    m: Option<ArtifactRef>,
    v: Option<ArtifactRef>,
    /// Optimizer steps of the moments recorded.
    t: u32,
    /// The moments recorded lag the weights by one step.
    lagging: bool,
}

/// The new checkpoint, shard by shard.
pub(crate) struct StepAssembly<'a> {
    before: &'a CheckpointRefs,
    weights: BTreeMap<String, ArtifactRef>,
    m: BTreeMap<String, ArtifactRef>,
    v: BTreeMap<String, ArtifactRef>,
    /// (t, lagging) of the trained shards: they must agree.
    opt: Option<(u32, bool)>,
}

impl<'a> StepAssembly<'a> {
    pub fn new(before: &'a CheckpointRefs) -> Self {
        Self {
            before,
            weights: BTreeMap::new(),
            m: BTreeMap::new(),
            v: BTreeMap::new(),
            opt: None,
        }
    }

    /// A shard without trained parameters keeps its artifact.
    pub fn unchanged(&mut self, key: &str) -> Result<(), ApiError> {
        let r = self
            .before
            .weights
            .get(key)
            .ok_or_else(|| ApiError::Internal(format!("checkpoint lacks {key}")))?;
        self.weights.insert(key.to_owned(), *r);
        Ok(())
    }

    pub fn add(&mut self, key: &str, s: ShardStep) -> Result<(), ApiError> {
        if self.opt.is_some_and(|o| o != (s.t, s.lagging)) {
            return Err(ApiError::Internal(
                "shards disagree on the optimizer step recorded".into(),
            ));
        }
        self.opt = Some((s.t, s.lagging));
        self.weights.insert(key.to_owned(), s.weights);
        if let Some(m) = s.m {
            self.m.insert(key.to_owned(), m);
        }
        if let Some(v) = s.v {
            self.v.insert(key.to_owned(), v);
        }
        Ok(())
    }

    pub fn finish(self) -> CheckpointRefs {
        let (opt_t, lagging) = self
            .opt
            .unwrap_or((self.before.opt_t, self.before.opt_lag > 0));
        CheckpointRefs {
            weights: self.weights,
            opt_m: (!self.m.is_empty()).then_some(self.m),
            opt_v: (!self.v.is_empty()).then_some(self.v),
            opt_t,
            opt_lag: lagging as u32,
        }
    }
}

/// Whether a run's step state fits in the memory kept between rounds (ADR-0036).
pub(crate) fn keeps_state(state: &AppState, plan: &gg_compute::graph::shards::ShardPlan) -> bool {
    let bytes: u64 = plan
        .shards
        .iter()
        .filter(|s| s.gradient_bytes > 0)
        .map(|s| s.checkpoint_bytes + 2 * s.gradient_bytes)
        .sum();
    bytes > 0 && bytes <= state.step_cache().capacity()
}

/// Accepted outputs of a data-parallel round's tasks, in slice order, with their example counts:
/// what its aggregation averaged.
pub(crate) async fn round_graph_outputs(
    c: &mut sqlx::PgConnection,
    job: Uuid,
) -> Result<Vec<(u32, BTreeMap<String, ArtifactRef>)>, ApiError> {
    let mut rows = jobs::list_tasks(c, JobId(job)).await?;
    rows.sort_by_key(|t| t.index_in_job);
    let mut out = Vec::with_capacity(rows.len());
    for t in &rows {
        let spec: TaskSpecV1 =
            canonical::from_bytes(&t.spec_cbor).map_err(|e| ApiError::Internal(e.to_string()))?;
        let TaskParamsV1::GraphGrad(p) = &spec.definition.params else {
            return Err(ApiError::Internal("not a graph round".into()));
        };
        let accepted = tasks::list_attempts(c, TaskId(t.id))
            .await?
            .into_iter()
            .find(|a| a.state == "accepted")
            .ok_or_else(|| ApiError::Internal("accepted task without accepted attempt".into()))?;
        let sub = tasks::get_submission_by_attempt(c, AttemptId(accepted.id))
            .await?
            .ok_or_else(|| ApiError::Internal("submission missing".into()))?;
        let signed: Signed<ResultSubmissionV1> = Signed::from_canonical_bytes(&sub.submission_cbor)
            .map_err(|e| ApiError::Internal(e.to_string()))?;
        out.push((p.example_count, signed.body.outputs));
    }
    Ok(out)
}

/// Every task's gradient of one shard, read at once.
pub(crate) async fn load_shard_grads(
    state: &AppState,
    g: &GraphRunSpec,
    outputs: &[(u32, BTreeMap<String, ArtifactRef>)],
    grad_name: &str,
    shard: &gg_compute::graph::shards::Shard,
) -> Result<Vec<(u32, Named)>, ApiError> {
    futures_util::future::try_join_all(outputs.iter().map(|(count, outputs)| async move {
        let r = outputs
            .get(grad_name)
            .ok_or_else(|| ApiError::Internal(format!("output {grad_name} missing")))?;
        // Sent in bf16 or f32; averaged in f32 either way (ADR-0035).
        let dtype = g.gradient_dtype();
        let t = load_bundle_as(state, r, g, shard.gradient_size(dtype), dtype).await?;
        Ok::<_, ApiError>((*count, t))
    }))
    .await
}

/// Writes a step's moments durably and hands them back with their references.
async fn persist_moments(
    state: AppState,
    project: ProjectId,
    run: Uuid,
    mut opt: NamedOptimizerState,
) -> Result<Moments, ApiError> {
    let store = async |t: Option<Named>| match t {
        Some(t) => {
            let (r, t) = store_bundle_owned(&state, project, run, t).await?;
            Ok::<_, ApiError>((Some(r), Some(t)))
        }
        None => Ok((None, None)),
    };
    let ((m_ref, m), (v_ref, v)) = tokio::try_join!(store(opt.m.take()), store(opt.v.take()))?;
    opt.m = m;
    opt.v = v;
    Ok(Moments {
        m: m_ref,
        v: v_ref,
        opt,
    })
}

/// One trained shard's optimizer step (ADR-0036). Its weights and moments come from memory when
/// the previous step left them there, otherwise from the checkpoint, the moments brought back to
/// the weights' step by `replay` (the gradients of the round that produced the weights) if they
/// lag. With `keep`, only the new weights are written before the round closes; the moments are
/// written behind the next round, which records them once durable.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn shard_step(
    state: &AppState,
    run: &cq::CollectiveRunRow,
    g: &GraphRunSpec,
    ckpt: &CheckpointRefs,
    (key, shard): (&str, &gg_compute::graph::shards::Shard),
    (optim, lr): (CoordOptim, f64),
    parts: Vec<(u32, Named)>,
    keep: bool,
    replay: impl AsyncFnOnce() -> Result<Vec<(u32, Named)>, ApiError>,
    times: &mut StepTimes,
) -> Result<ShardStep, ApiError> {
    let project = ProjectId(run.project_id);
    let started = Instant::now();
    let weights_ref = *ckpt
        .weights
        .get(key)
        .ok_or_else(|| ApiError::Internal(format!("checkpoint lacks {key}")))?;
    let t = ckpt.opt_t + ckpt.opt_lag;
    let (weights, opt, recorded) = match state.step_cache().take(run.id, key, &weights_ref, t) {
        Some(entry) => {
            // The moments of the last step, written behind the round: durable once handed back.
            let moments = entry
                .moments
                .await
                .map_err(|e| ApiError::Internal(format!("moments task: {e}")))??;
            (entry.weights, moments.opt, (moments.m, moments.v))
        }
        None => {
            let (weights, opt) = load_step_state(state, g, ckpt, key, shard).await?;
            if ckpt.opt_lag == 0 {
                let get = |m: &Option<BTreeMap<String, ArtifactRef>>| {
                    m.as_ref().and_then(|m| m.get(key).copied())
                };
                (weights, opt, (get(&ckpt.opt_m), get(&ckpt.opt_v)))
            } else {
                // Restarted, or the state was evicted: the recorded moments are one step behind
                // the weights. The gradients of the round that produced them bring them back,
                // with the same arithmetic as the step itself.
                tracing::info!(run = %run.id, shard = key, "optimizer moments rebuilt from the round's gradients");
                state.step_cache().note_rebuilt();
                let replayed = replay().await?;
                let opt = tokio::task::spawn_blocking(move || {
                    let g = aggregate_named(&replayed)?;
                    drop(replayed);
                    let mut opt = opt;
                    gg_compute::graph::task::advance_moments_named(optim, &g, &mut opt)?;
                    Ok::<_, ApiError>(opt)
                })
                .await
                .map_err(|e| ApiError::Internal(format!("replay task: {e}")))??;
                let moments = persist_moments(state.clone(), project, run.id, opt).await?;
                (weights, moments.opt, (moments.m, moments.v))
            }
        }
    };
    if opt.t != t {
        return Err(ApiError::Internal(format!(
            "optimizer state after {} steps for weights after {t}",
            opt.t
        )));
    }
    times.read += started.elapsed();
    let started = Instant::now();
    let (weights, opt) = step_blocking(optim, lr, (weights, opt), parts).await?;
    times.compute += started.elapsed();
    let started = Instant::now();
    let t_next = opt.t;
    let done = if keep {
        // The weights before the round closes: the next round's workers need them.
        let (new_ref, weights) = store_bundle_owned(state, project, run.id, weights).await?;
        let adam = opt.m.is_some();
        let bytes = named_bytes(&weights)
            + opt.m.as_ref().map_or(0, named_bytes)
            + opt.v.as_ref().map_or(0, named_bytes);
        let moments = tokio::spawn(persist_moments(state.clone(), project, run.id, opt));
        let entry = Entry {
            weights_ref: new_ref,
            weights,
            t: t_next,
            moments,
            bytes,
        };
        // An entry refused for lack of room is dropped: the next step reads the checkpoint and
        // rebuilds the moments. What is recorded is the same either way.
        let _ = state.step_cache().put(run.id, key, entry);
        if adam {
            ShardStep {
                weights: new_ref,
                m: recorded.0,
                v: recorded.1,
                t,
                lagging: true,
            }
        } else {
            ShardStep {
                weights: new_ref,
                m: None,
                v: None,
                t: t_next,
                lagging: false,
            }
        }
    } else {
        let stored = store_step_state(state, project, run.id, weights, opt).await?;
        ShardStep {
            weights: stored.weights,
            m: stored.m,
            v: stored.v,
            t: t_next,
            lagging: false,
        }
    };
    times.store += started.elapsed();
    Ok(done)
}

/// Rows `idx` of a dataset stored as consecutive row ranges, read by seeking: the server never
/// holds more than the micro-batch (C4). `shape` is the full dataset shape.
pub(crate) async fn read_rows(
    state: &AppState,
    parts: &[ArtifactRef],
    shape: &[u64],
    idx: &[usize],
) -> Result<TensorF32, ApiError> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let row_elems: u64 = shape[1..].iter().product();
    let row_bytes = row_elems * 4;
    let header = 6 + 8 * shape.len() as u64;
    // Rows of each shard, read from its header (same rank, checked at creation).
    let mut bounds = Vec::with_capacity(parts.len());
    let mut first = 0u64;
    for r in parts {
        let rows = (r.size - header) / row_bytes.max(1);
        bounds.push((first, first + rows));
        first += rows;
    }
    let mut files: Vec<Option<tokio::fs::File>> = (0..parts.len()).map(|_| None).collect();
    let mut data = Vec::with_capacity(idx.len() * row_elems as usize);
    let mut buf = vec![0u8; row_bytes as usize];
    for &i in idx {
        let i = i as u64;
        let k = bounds
            .iter()
            .position(|(a, b)| i >= *a && i < *b)
            .ok_or_else(|| ApiError::Internal(format!("row {i} outside the dataset")))?;
        if files[k].is_none() {
            files[k] = Some(state.objects().open_read(&parts[k].hash).await?);
        }
        let f = files[k].as_mut().expect("opened above");
        f.seek(std::io::SeekFrom::Start(
            header + (i - bounds[k].0) * row_bytes,
        ))
        .await
        .map_err(|e| ApiError::Internal(format!("dataset read: {e}")))?;
        f.read_exact(&mut buf)
            .await
            .map_err(|e| ApiError::Internal(format!("dataset read: {e}")))?;
        data.extend(
            buf.chunks_exact(4)
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])),
        );
    }
    let mut out_shape = shape.to_vec();
    out_shape[0] = idx.len() as u64;
    TensorF32::new(out_shape, data).map_err(|e| ApiError::Internal(format!("micro-batch: {e}")))
}

/// The run's description and its checkpoint shards, recomputed from the stored graph: the same
/// deterministic function the workers and validators apply (C2).
pub(crate) async fn run_plan(
    state: &AppState,
    g: &GraphRunSpec,
) -> Result<
    (
        gg_compute::graph::ModelGraphV1,
        gg_compute::graph::shards::ShardPlan,
    ),
    ApiError,
> {
    let bytes = state
        .objects()
        .read_bytes(&g.graph.hash, g.graph.size)
        .await?;
    let model: gg_compute::graph::ModelGraphV1 =
        serde_json::from_slice(&bytes).map_err(|e| ApiError::Internal(format!("graph: {e}")))?;
    let report = gg_compute::graph::validate(&model)
        .map_err(|e| ApiError::Internal(format!("graph: {e}")))?;
    let plan = if g.stage_bounds.is_empty() {
        gg_compute::graph::shards::plan(&model, &report, g.shard_bytes)
            .map_err(ApiError::Internal)?
    } else {
        let st = gg_compute::graph::stages::stages(&model, &report, &g.stage_bounds)
            .map_err(ApiError::Internal)?;
        gg_compute::graph::shards::plan_stages(&model, &st).map_err(ApiError::Internal)?
    };
    if plan.checkpoint_bytes() != g.checkpoint_bytes
        || plan.gradient_bytes() != g.gradient_bytes
        || (g.gradient_bf16 && plan.gradient_size(Dtype::Bf16) != g.gradient_bf16_bytes)
    {
        return Err(ApiError::Internal(
            "shard plan does not match the run".into(),
        ));
    }
    Ok((model, plan))
}

/// Loads a `GGB1` container. `expected` is the size of a checkpoint (all parameters) or that of a
/// gradient and optimizer state (trained parameters only, ADR-0019).
pub(crate) async fn load_bundle(
    state: &AppState,
    r: &ArtifactRef,
    spec: &GraphRunSpec,
    expected: u64,
) -> Result<Named, ApiError> {
    load_bundle_as(state, r, spec, expected, Dtype::F32).await
}

/// Same, for a container whose tensors are `dtype` (a task's gradient, ADR-0035); decoded to f32.
pub(crate) async fn load_bundle_as(
    state: &AppState,
    r: &ArtifactRef,
    spec: &GraphRunSpec,
    expected: u64,
    dtype: Dtype,
) -> Result<Named, ApiError> {
    if r.size != expected {
        return Err(ApiError::Internal(format!(
            "bundle is {} bytes, run expects {expected}",
            r.size
        )));
    }
    // Read, hash check and decoding on a blocking thread: the server's step reads several of
    // these at once (weights, moments, gradients), up to 540 MB each for SmolLM2.
    let objects = state.objects().clone();
    let (hash, max_elements) = (r.hash, spec.parameters);
    tokio::task::spawn_blocking(move || {
        let bytes = objects.read_bytes_blocking(&hash, expected)?;
        let bundle = BundleF32::decode_as(&bytes, &TensorLimits { max_elements }, dtype)
            .map_err(|e| ApiError::Internal(format!("bundle: {e}")))?;
        Ok::<_, ApiError>(bundle.entries)
    })
    .await
    .map_err(|e| ApiError::Internal(format!("read task: {e}")))?
}

async fn load_tensor(
    state: &AppState,
    r: &ArtifactRef,
    shape: &[u64],
) -> Result<TensorF32, ApiError> {
    let bytes = state
        .objects()
        .read_bytes(&r.hash, gg_compute::catalog::expected_rgt_size(shape))
        .await?;
    let t = TensorF32::decode(&bytes, &gg_compute::catalog::tensor_limits_for(shape))
        .map_err(|e| ApiError::Internal(format!("checkpoint tensor: {e}")))?;
    t.check_shape(shape)
        .map_err(|e| ApiError::Internal(format!("checkpoint tensor: {e}")))?;
    Ok(t)
}

fn shapes(cfg: &CollectiveConfig) -> [(&'static str, Vec<u64>); 4] {
    [
        ("w1", vec![cfg.d_input as u64, cfg.d_hidden as u64]),
        ("b1", vec![cfg.d_hidden as u64]),
        ("w2", vec![cfg.d_hidden as u64, cfg.d_output as u64]),
        ("b2", vec![cfg.d_output as u64]),
    ]
}

async fn load_weights(
    state: &AppState,
    cfg: &CollectiveConfig,
    refs: &BTreeMap<String, ArtifactRef>,
) -> Result<Weights, ApiError> {
    let mut got: BTreeMap<&str, TensorF32> = BTreeMap::new();
    for (name, shape) in shapes(cfg) {
        let r = refs
            .get(name)
            .ok_or_else(|| ApiError::Internal(format!("checkpoint lacks {name}")))?;
        got.insert(name, load_tensor(state, r, &shape).await?);
    }
    Ok(Weights {
        w1: got["w1"].clone(),
        b1: got["b1"].clone(),
        w2: got["w2"].clone(),
        b2: got["b2"].clone(),
    })
}

/// Open a round: one job with `tasks_per_round` `mlp.grad.v1` tasks over disjoint slices.
pub(crate) async fn open_round(
    state: &AppState,
    tx: &mut gg_storage::PgConnection,
    run: &cq::CollectiveRunRow,
    cfg: &CollectiveConfig,
    ckpt: &CheckpointRefs,
    step: i32,
    attempt: i32,
) -> Result<Uuid, ApiError> {
    if cfg
        .graph
        .as_ref()
        .is_some_and(|g| !g.stage_bounds.is_empty())
    {
        return crate::workflows::staged::open_round(state, tx, run, cfg, ckpt, step, attempt)
            .await;
    }
    let seed = round_seed(cfg, step);
    let job_id = JobId::new();
    jobs::create_job(
        tx,
        job_id,
        ProjectId(run.project_id),
        ExperimentId(run.experiment_id),
        None,
        &format!("{}-round{step}-a{attempt}", run.name),
    )
    .await?;
    let mut inputs = BTreeMap::new();
    for (name, r) in &ckpt.weights {
        inputs.insert(name.clone(), *r);
    }
    let kind: &str = if cfg.graph.is_some() {
        gtask::KIND
    } else {
        inputs.insert(
            "train_x".into(),
            cfg.train_x
                .ok_or_else(|| ApiError::Internal("mlp run without train_x".into()))?,
        );
        inputs.insert(
            "train_y".into(),
            cfg.train_y
                .ok_or_else(|| ApiError::Internal("mlp run without train_y".into()))?,
        );
        "mlp.grad.v1"
    };
    if let Some(g) = &cfg.graph {
        inputs.insert(gtask::GRAPH_INPUT.into(), g.graph);
        if !g.batch_inputs {
            for (name, r) in &g.dataset_refs {
                inputs.insert(name.clone(), *r);
            }
        }
    }
    for (i, (start, count)) in slices(cfg.batch_per_round, cfg.tasks_per_round)
        .into_iter()
        .enumerate()
    {
        // Micro-batch selected here (C4): the task downloads only its own rows.
        let mut inputs = inputs.clone();
        if let Some(g) = cfg.graph.as_ref().filter(|g| g.batch_inputs) {
            let idx = gtask::round_indices(seed, cfg.total_examples, start, count);
            for d in &g.datasets {
                let parts = match g.dataset_parts.get(&d.name) {
                    Some(list) => list.clone(),
                    None => vec![*g.dataset_refs.get(&d.name).ok_or_else(|| {
                        ApiError::Internal(format!("dataset {} missing", d.name))
                    })?],
                };
                let rows = read_rows(state, &parts, &d.shape, &idx).await?;
                inputs.insert(
                    d.name.clone(),
                    store_tensor(state, ProjectId(run.project_id), run.id, &rows).await?,
                );
            }
        }
        let def = match &cfg.graph {
            Some(g) => gg_compute::catalog::graph_grad_definition(
                GraphGradParamsV1 {
                    graph_hash: g.graph.hash.to_hex(),
                    graph_bytes: g.graph.size,
                    parameters: g.parameters,
                    checkpoint_bytes: g.checkpoint_bytes,
                    gradient_bytes: g.task_gradient_bytes(),
                    segments: g.segments,
                    offload_parameters: g.offload_parameters,
                    training_flops_per_example: g.training_flops_per_example,
                    activation_bytes_per_example: g.activation_bytes_per_example,
                    datasets: g.datasets.clone(),
                    round_seed: seed,
                    example_start: start,
                    example_count: count,
                    total_examples: cfg.total_examples,
                    shard_bytes: g.shard_bytes,
                    batch_inputs: g.batch_inputs,
                    gradient_bf16: g.gradient_bf16,
                },
                inputs.clone(),
            )?,
            None => gg_compute::catalog::grad_definition(
                MlpGradParamsV1 {
                    d_input: cfg.d_input,
                    d_hidden: cfg.d_hidden,
                    d_output: cfg.d_output,
                    round_seed: seed,
                    example_start: start,
                    example_count: count,
                    total_examples: cfg.total_examples,
                },
                inputs.clone(),
            )?,
        };
        let task_id = TaskId::new();
        let spec = TaskSpecV1 {
            spec_version: 1,
            network: state.network().clone(),
            project_id: ProjectId(run.project_id),
            experiment_id: ExperimentId(run.experiment_id),
            job_id,
            task_id,
            index: i as u32,
            definition: def.clone(),
        };
        let spec_cbor = canonical::to_bytes(&spec)?;
        let spec_hash =
            gg_protocol::domain::hash_bytes(state.network(), Domain::TaskSpec, &spec_cbor);
        let definition_hash =
            gg_protocol::domain::hash_message(state.network(), Domain::TaskDefinition, &def)?;
        let checked = gg_compute::validate_definition(&def)?;
        jobs::insert_task(
            tx,
            &jobs::NewTask {
                id: task_id,
                job_id,
                project_id: ProjectId(run.project_id),
                experiment_id: ExperimentId(run.experiment_id),
                index: i as i32,
                compute_kind: kind,
                spec_hash,
                definition_hash,
                spec_cbor: &spec_cbor,
                max_attempts: def.max_attempts as i32,
                priority: cfg.priority,
                reward_primary: def.reward_schedule.primary,
                verification_cost: def.reward_schedule.verification_cost,
                est_cpu_secs: checked.estimate.est_cpu_secs,
                requirements: jobs::TaskRequirements {
                    ram_bytes: def.resource_limits.max_ram_bytes as i64,
                    device_bytes: cfg.graph.as_ref().map_or(0, |g| g.task_device_bytes) as i64,
                    profile: cfg.graph.as_ref().map_or(1, |g| g.required_profile) as i32,
                },
            },
        )
        .await?;
    }
    let round_id = Uuid::now_v7();
    cq::insert_round(
        tx,
        round_id,
        run.id,
        step,
        attempt,
        seed as i64,
        job_id.0,
        cfg.batch_per_round as i64,
    )
    .await?;
    misc::outbox_insert(tx, "collective.round_opened", &serde_json::json!({ "run_id": run.id, "step": step, "attempt": attempt, "job_id": job_id })).await?;
    Ok(round_id)
}

/// Create a run: validate, materialize the protocol initial checkpoint, open round 0.
pub async fn create_run(
    state: &AppState,
    researcher: ResearcherId,
    project: ProjectId,
    req: CreateCollectiveRequest,
) -> Result<RunView, ApiError> {
    use gg_compute::catalog::mlp_bounds::*;
    if req.name.len() > 128 {
        return Err(ApiError::Unprocessable("name too long".into()));
    }
    if req.tasks_per_round == 0
        || req.tasks_per_round > 64
        || req.rounds == 0
        || req.rounds > 1000
        || req.batch_per_round == 0
    {
        return Err(ApiError::Unprocessable(
            "tasks_per_round in 1..=64, rounds in 1..=1000, batch_per_round > 0".into(),
        ));
    }
    if req.graph.is_some() {
        return create_graph_run(state, researcher, project, req).await;
    }
    let (Some(train_x), Some(train_y), Some(d_hidden)) = (req.train_x, req.train_y, req.d_hidden)
    else {
        return Err(ApiError::Unprocessable(
            "an MLP run needs train_x, train_y and d_hidden; a graph run needs graph and datasets"
                .into(),
        ));
    };
    if req.tasks_per_round > req.batch_per_round
        || req.batch_per_round.div_ceil(req.tasks_per_round) > MAX_BATCH
    {
        return Err(ApiError::Unprocessable(format!(
            "each micro-batch must have 1..={MAX_BATCH} examples"
        )));
    }
    if d_hidden == 0 || d_hidden > MAX_D_HIDDEN {
        return Err(ApiError::Unprocessable("d_hidden out of range".into()));
    }
    let optim = CoordOptim::parse(&req.optimizer)
        .ok_or_else(|| ApiError::Unprocessable("optimizer must be sgd or adam".into()))?;
    if !(req.learning_rate.is_finite() && req.learning_rate > 0.0 && req.learning_rate <= 1.0) {
        return Err(ApiError::Unprocessable(
            "learning_rate must be in (0, 1]".into(),
        ));
    }
    let mut tx = state.db().begin().await?;
    let proj = identities::get_project(&mut tx, project)
        .await?
        .ok_or_else(|| ApiError::NotFound("project".into()))?;
    if !proj.allowed_kinds.iter().any(|k| k == "mlp.grad.v1") {
        return Err(ApiError::Forbidden(
            "project may not submit mlp.grad.v1".into(),
        ));
    }
    // Dataset widths from the artifacts (headers only).
    let mut widths = [0u64; 2];
    let mut rows = [0u64; 2];
    for (i, r) in [&train_x, &train_y].into_iter().enumerate() {
        let row = artifacts::get_artifact(&mut tx, &r.hash)
            .await?
            .ok_or_else(|| ApiError::Unprocessable("dataset artifact unknown".into()))?;
        if row.size as u64 != r.size
            || !artifacts::artifact_readable_by_project(&mut tx, &r.hash, project).await?
        {
            return Err(ApiError::Forbidden(
                "dataset artifact not readable by this project".into(),
            ));
        }
        let header = state.objects().read_bytes(&r.hash, 64 << 20).await?;
        let t = TensorF32::decode(&header, &gg_protocol::tensor_file::TensorLimits::default())
            .map_err(|e| ApiError::Unprocessable(format!("dataset: {e}")))?;
        if t.shape.len() != 2 {
            return Err(ApiError::Unprocessable(
                "dataset tensors must be [n, d]".into(),
            ));
        }
        rows[i] = t.shape[0];
        widths[i] = t.shape[1];
    }
    if rows[0] != rows[1] || rows[0] == 0 || rows[0] > MAX_EXAMPLES {
        return Err(ApiError::Unprocessable(
            "train_x/train_y row counts invalid".into(),
        ));
    }
    if widths[0] == 0
        || widths[0] > MAX_D_INPUT as u64
        || widths[1] == 0
        || widths[1] > MAX_D_OUTPUT as u64
    {
        return Err(ApiError::Unprocessable(
            "dataset widths out of range".into(),
        ));
    }
    if req.batch_per_round as u64 > rows[0] {
        return Err(ApiError::Unprocessable(
            "batch_per_round exceeds the dataset".into(),
        ));
    }
    let cfg = CollectiveConfig {
        d_input: widths[0] as u32,
        d_hidden,
        d_output: widths[1] as u32,
        optimizer: req.optimizer.clone(),
        learning_rate: req.learning_rate,
        batch_per_round: req.batch_per_round,
        tasks_per_round: req.tasks_per_round,
        rounds: req.rounds,
        seed: req.seed,
        max_round_retries: req.max_round_retries.min(10),
        train_x: Some(train_x),
        train_y: Some(train_y),
        total_examples: rows[0],
        priority: req.priority,
        graph: None,
    };
    let run_id = Uuid::now_v7();
    let init = initial_weights(&MlpTask {
        d_in: cfg.d_input as usize,
        d_h: cfg.d_hidden as usize,
        d_out: cfg.d_output as usize,
        optimizer: Optim::Sgd,
        lr: 0.0,
        batch: 1,
        steps: 1,
        seed: cfg.seed,
        n_train: cfg.total_examples as usize,
        n_eval: 0,
    });
    let weights = store_weights(state, project, run_id, &init).await?;
    let ckpt = CheckpointRefs {
        weights,
        opt_m: None,
        opt_v: None,
        opt_t: 0,
        opt_lag: 0,
    };
    let experiment_id = ExperimentId::new();
    jobs::create_experiment(
        &mut tx,
        experiment_id,
        project,
        &format!("collective:{}", req.name),
        Some(researcher),
    )
    .await?;
    cq::insert_run(
        &mut tx,
        run_id,
        project.0,
        experiment_id.0,
        Some(researcher.0),
        &req.name,
        &serde_json::to_value(&cfg).map_err(|e| ApiError::Internal(e.to_string()))?,
        &serde_json::to_value(&ckpt).map_err(|e| ApiError::Internal(e.to_string()))?,
    )
    .await?;
    cq::insert_checkpoint(
        &mut tx,
        run_id,
        0,
        &serde_json::to_value(&ckpt).map_err(|e| ApiError::Internal(e.to_string()))?,
        None,
    )
    .await?;
    let run = cq::get_run(&mut tx, run_id)
        .await?
        .ok_or_else(|| ApiError::Internal("run missing".into()))?;
    open_round(state, &mut tx, &run, &cfg, &ckpt, 0, 1).await?;
    misc::audit(&mut tx, &format!("researcher:{researcher}"), "collective.create", &run_id.to_string(), serde_json::json!({ "project_id": project, "rounds": cfg.rounds, "tasks_per_round": cfg.tasks_per_round })).await?;
    let _ = optim;
    tx.commit().await?;
    state.signal_work();
    run_view(state, run_id).await
}

/// Current view of a run.
pub async fn run_view(state: &AppState, run_id: Uuid) -> Result<RunView, ApiError> {
    let mut c = state
        .db()
        .pool()
        .acquire()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let run = cq::get_run(&mut c, run_id)
        .await?
        .ok_or_else(|| ApiError::NotFound("collective run".into()))?;
    let cfg: CollectiveConfig = serde_json::from_value(run.config.clone())
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let ckpt: CheckpointRefs = serde_json::from_value(run.checkpoint.clone())
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let rounds = cq::list_rounds(&mut c, run_id)
        .await?
        .into_iter()
        .map(|r| RoundView {
            step: r.step,
            attempt: r.attempt,
            job_id: JobId(r.job_id),
            state: r.state,
            round_seed: r.round_seed as u64,
        })
        .collect();
    let checkpoint_steps = cq::list_checkpoints(&mut c, run_id)
        .await?
        .into_iter()
        .map(|(s, _)| s)
        .collect();
    Ok(RunView {
        run_id,
        project_id: ProjectId(run.project_id),
        experiment_id: ExperimentId(run.experiment_id),
        name: run.name,
        state: run.state,
        step: run.step,
        rounds_total: cfg.rounds,
        config: cfg,
        checkpoint: ckpt,
        rounds,
        checkpoint_steps,
        failure: run.failure,
    })
}

/// Project of a run (for authorization).
pub async fn run_project(state: &AppState, run_id: Uuid) -> Result<ProjectId, ApiError> {
    let mut c = state
        .db()
        .pool()
        .acquire()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let run = cq::get_run(&mut c, run_id)
        .await?
        .ok_or_else(|| ApiError::NotFound("collective run".into()))?;
    Ok(ProjectId(run.project_id))
}

/// Cancel a run and its open rounds' jobs.
pub async fn cancel_run(state: &AppState, actor: &str, run_id: Uuid) -> Result<(), ApiError> {
    let mut tx = state.db().begin().await?;
    cancel_run_in(&mut tx, actor, run_id, "cancelled by researcher").await?;
    tx.commit().await?;
    state.step_cache().remove_run(run_id);
    Ok(())
}

/// Cancel a run inside the caller's transaction, recording `reason` as its failure.
pub(crate) async fn cancel_run_in(
    tx: &mut gg_storage::PgConnection,
    actor: &str,
    run_id: Uuid,
    reason: &str,
) -> Result<(), ApiError> {
    {
        let run = cq::lock_run(&mut *tx, run_id)
            .await?
            .ok_or_else(|| ApiError::NotFound("collective run".into()))?;
        if run.state == "running" {
            cq::update_run(
                &mut *tx,
                run_id,
                "cancelled",
                run.step,
                &run.checkpoint,
                reason,
            )
            .await?;
        }
        let rounds = cq::list_rounds(&mut *tx, run_id).await?;
        // Every job of an open round: the current one, and for a staged round every wave already
        // opened, whose tasks may still be retried after a rejection (ADR-0026).
        let mut jobs = Vec::new();
        for r in rounds.iter().filter(|r| r.state == "open") {
            cq::close_round(&mut *tx, r.id, "failed").await?;
            jobs.push(r.job_id);
            if let Some(v) = cq::stage_state(&mut *tx, r.id).await?
                && let Ok(st) = serde_json::from_value::<crate::workflows::staged::StageState>(v)
            {
                jobs.extend(st.jobs().into_iter().filter(|j| *j != r.job_id));
            }
        }
        misc::audit(
            &mut *tx,
            actor,
            "collective.cancel",
            &run_id.to_string(),
            serde_json::json!({ "reason": reason }),
        )
        .await?;
        for job in &jobs {
            crate::workflows::cancel::cancel_job_in(
                &mut *tx,
                actor,
                Authority::Server,
                JobId(*job),
            )
            .await?;
        }
    }
    Ok(())
}

/// Close a failed round, reissue it while retries remain (or fail the run), then cancel what is
/// left of its jobs: one for a data-parallel round, one per wave opened for a staged one.
pub(crate) async fn fail_round(
    state: &AppState,
    mut tx: sqlx::Transaction<'static, sqlx::Postgres>,
    run: cq::CollectiveRunRow,
    cfg: CollectiveConfig,
    round: cq::CollectiveRoundRow,
    jobs: Vec<Uuid>,
) -> Result<bool, ApiError> {
    cq::close_round(&mut tx, round.id, "failed").await?;
    if round.attempt <= cfg.max_round_retries as i32 {
        let ckpt: CheckpointRefs = serde_json::from_value(run.checkpoint.clone())
            .map_err(|e| ApiError::Internal(e.to_string()))?;
        open_round(
            state,
            &mut tx,
            &run,
            &cfg,
            &ckpt,
            round.step,
            round.attempt + 1,
        )
        .await?;
        tracing::warn!(run = %run.id, step = round.step, attempt = round.attempt + 1, "round failed, reissued");
    } else {
        cq::update_run(
            &mut tx,
            run.id,
            "failed",
            run.step,
            &run.checkpoint,
            &format!(
                "round {} failed after {} attempts",
                round.step, round.attempt
            ),
        )
        .await?;
    }
    // What remains of the failed round's jobs is cancelled in the same transaction: no task of it
    // can be claimed once the round is known to have failed.
    for job in jobs {
        crate::workflows::cancel::cancel_job_in(
            &mut tx,
            "coordinator",
            Authority::Server,
            JobId(job),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(true)
}

/// Process one open round if it is complete or failed. Returns true when something changed.
async fn process_round(state: &AppState, round_id: Uuid) -> Result<bool, ApiError> {
    let mut tx = state.db().begin().await?;
    let Some(round) = sqlx::query_as::<_, cq::CollectiveRoundRow>(
        "SELECT id, run_id, step, attempt, round_seed, job_id, state, expected_examples, created_at, closed_at FROM collective_rounds WHERE id = $1 AND state = 'open' FOR UPDATE SKIP LOCKED",
    )
    .bind(round_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| ApiError::Internal(e.to_string()))?
    else {
        return Ok(false);
    };
    let run = cq::lock_run(&mut tx, round.run_id)
        .await?
        .ok_or_else(|| ApiError::Internal("run missing".into()))?;
    let cfg: CollectiveConfig = serde_json::from_value(run.config.clone())
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    if run.state != "running" {
        cq::close_round(&mut tx, round.id, "failed").await?;
        tx.commit().await?;
        return Ok(true);
    }
    // Staged execution: speculative waves, then aggregation (D2, ADR-0026).
    if cfg
        .graph
        .as_ref()
        .is_some_and(|g| !g.stage_bounds.is_empty())
    {
        return crate::workflows::staged::advance(state, tx, run, cfg, round).await;
    }
    let task_rows = jobs::list_tasks(&mut tx, JobId(round.job_id)).await?;
    let states: Vec<TaskState> = task_rows
        .iter()
        .map(|t| t.task_state())
        .collect::<Result<_, _>>()
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    if states
        .iter()
        .any(|s| matches!(s, TaskState::Failed | TaskState::Cancelled))
    {
        let job = round.job_id;
        return fail_round(state, tx, run, cfg, round, vec![job]).await;
    }
    if !states.iter().all(|s| *s == TaskState::Accepted) {
        tx.rollback().await?;
        return Ok(false);
    }
    // All accepted: gather gradients in slice order (task index), verify coverage, aggregate once.
    let mut mlp_parts: Vec<(u32, Weights)> = Vec::new();
    let mut graph_parts: Vec<(u32, BTreeMap<String, ArtifactRef>)> = Vec::new();
    let mut covered: u64 = 0;
    let mut slices_seen = Vec::new();
    let mut ordered = task_rows.clone();
    ordered.sort_by_key(|t| t.index_in_job);
    for t in &ordered {
        let spec: TaskSpecV1 =
            canonical::from_bytes(&t.spec_cbor).map_err(|e| ApiError::Internal(e.to_string()))?;
        let (example_start, example_count) = match &spec.definition.params {
            TaskParamsV1::MlpGrad(p) if cfg.graph.is_none() => (p.example_start, p.example_count),
            TaskParamsV1::GraphGrad(p) if cfg.graph.is_some() => (p.example_start, p.example_count),
            other => {
                return Err(ApiError::Internal(format!(
                    "round task kind {} does not match the run",
                    other.compute_kind()
                )));
            }
        };
        if slices_seen.contains(&example_start) {
            return Err(ApiError::Internal("duplicate slice in round".into()));
        }
        slices_seen.push(example_start);
        covered += example_count as u64;
        let accepted = tasks::list_attempts(&mut tx, TaskId(t.id))
            .await?
            .into_iter()
            .find(|a| a.state == "accepted")
            .ok_or_else(|| ApiError::Internal("accepted task without accepted attempt".into()))?;
        let sub = tasks::get_submission_by_attempt(&mut tx, AttemptId(accepted.id))
            .await?
            .ok_or_else(|| ApiError::Internal("submission missing".into()))?;
        let signed: Signed<ResultSubmissionV1> = Signed::from_canonical_bytes(&sub.submission_cbor)
            .map_err(|e| ApiError::Internal(e.to_string()))?;
        match &cfg.graph {
            Some(_) => {
                // Gradient shards are read one shard at a time below, never all at once.
                graph_parts.push((example_count, signed.body.outputs.clone()));
            }
            None => {
                let checked = gg_compute::validate_definition(&spec.definition)?;
                let mut gm: BTreeMap<&str, TensorF32> = BTreeMap::new();
                for name in ["g_w1", "g_b1", "g_w2", "g_b2"] {
                    let r = signed
                        .body
                        .outputs
                        .get(name)
                        .ok_or_else(|| ApiError::Internal(format!("output {name} missing")))?;
                    gm.insert(
                        name,
                        load_tensor(state, r, &checked.output_shapes[name]).await?,
                    );
                }
                mlp_parts.push((
                    example_count,
                    Weights {
                        w1: gm["g_w1"].clone(),
                        b1: gm["g_b1"].clone(),
                        w2: gm["g_w2"].clone(),
                        b2: gm["g_b2"].clone(),
                    },
                ));
            }
        }
    }
    let parts_len = mlp_parts.len().max(graph_parts.len());
    if covered != round.expected_examples as u64 || parts_len != cfg.tasks_per_round as usize {
        cq::update_run(
            &mut tx,
            run.id,
            "failed",
            run.step,
            &run.checkpoint,
            "round coverage mismatch",
        )
        .await?;
        cq::close_round(&mut tx, round.id, "failed").await?;
        tx.commit().await?;
        return Ok(true);
    }
    let ckpt: CheckpointRefs = serde_json::from_value(run.checkpoint.clone())
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let optim = CoordOptim::parse(&cfg.optimizer)
        .ok_or_else(|| ApiError::Internal("bad optimizer".into()))?;
    let project = ProjectId(run.project_id);
    let new_ckpt = match &cfg.graph {
        Some(g) => {
            // Shard by shard (C2): weights, optimizer state and the tasks' gradients of one shard
            // at a time. A shard without trained parameters keeps its artifact unchanged.
            let (_, plan) = run_plan(state, g).await?;
            // The run's state stays in memory between rounds when it fits, except after the last
            // round, whose state is written entirely before the run completes (ADR-0036).
            let keep = keeps_state(state, &plan) && round.step as u32 + 1 < cfg.rounds;
            let mut steps = StepAssembly::new(&ckpt);
            let mut times = StepTimes::default();
            for (i, shard) in plan.shards.iter().enumerate() {
                let key = plan.checkpoint_name(i);
                if shard.gradient_bytes == 0 {
                    steps.unchanged(&key)?;
                    continue;
                }
                let started = Instant::now();
                let grad_name = plan.gradient_name(i);
                let parts = load_shard_grads(state, g, &graph_parts, &grad_name, shard).await?;
                times.read += started.elapsed();
                // Gradients of the round that produced the current weights, should the moments
                // have to be brought back to them.
                let replay = async || {
                    let mut c = state
                        .db()
                        .pool()
                        .acquire()
                        .await
                        .map_err(|e| ApiError::Internal(e.to_string()))?;
                    let from = cq::checkpoint_round(&mut c, run.id, run.step)
                        .await?
                        .ok_or_else(|| {
                            ApiError::Internal("no round recorded for the checkpoint".into())
                        })?;
                    let outputs = round_graph_outputs(&mut c, from.job_id).await?;
                    drop(c);
                    load_shard_grads(state, g, &outputs, &grad_name, shard).await
                };
                let done = shard_step(
                    state,
                    &run,
                    g,
                    &ckpt,
                    (&key, shard),
                    (optim, cfg.learning_rate),
                    parts,
                    keep,
                    replay,
                    &mut times,
                )
                .await?;
                steps.add(&key, done)?;
            }
            times.log(run.id, round.step);
            steps.finish()
        }
        None => {
            let mut weights = load_weights(state, &cfg, &ckpt.weights).await?;
            let mut opt_state = OptimizerState {
                m: None,
                v: None,
                t: ckpt.opt_t,
            };
            if let (Some(m), Some(v)) = (&ckpt.opt_m, &ckpt.opt_v) {
                opt_state.m = Some(load_weights(state, &cfg, m).await?);
                opt_state.v = Some(load_weights(state, &cfg, v).await?);
            }
            let g = aggregate(&mlp_parts)?;
            apply_step(optim, cfg.learning_rate, &mut weights, &g, &mut opt_state);
            CheckpointRefs {
                weights: store_weights(state, project, run.id, &weights).await?,
                opt_m: match &opt_state.m {
                    Some(m) => Some(store_weights(state, project, run.id, m).await?),
                    None => None,
                },
                opt_v: match &opt_state.v {
                    Some(v) => Some(store_weights(state, project, run.id, v).await?),
                    None => None,
                },
                opt_t: opt_state.t,
                opt_lag: 0,
            }
        }
    };
    commit_step(state, tx, &run, &cfg, &round, &new_ckpt, covered, parts_len).await
}

/// Publish the aggregated checkpoint of a round, close it, and open the next one (or complete the
/// run). Shared by data-parallel and staged rounds.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn commit_step(
    state: &AppState,
    mut tx: sqlx::Transaction<'static, sqlx::Postgres>,
    run: &cq::CollectiveRunRow,
    cfg: &CollectiveConfig,
    round: &cq::CollectiveRoundRow,
    new_ckpt: &CheckpointRefs,
    covered: u64,
    parts_len: usize,
) -> Result<bool, ApiError> {
    let ckpt_json =
        serde_json::to_value(new_ckpt).map_err(|e| ApiError::Internal(e.to_string()))?;
    let next_step = round.step + 1;
    // Unique (run, step): a second aggregation of the same step is impossible.
    cq::insert_checkpoint(&mut tx, run.id, next_step, &ckpt_json, Some(round.id)).await?;
    cq::close_round(&mut tx, round.id, "aggregated").await?;
    let completed = next_step as u32 >= cfg.rounds;
    cq::update_run(
        &mut tx,
        run.id,
        if completed { "completed" } else { "running" },
        next_step,
        &ckpt_json,
        "",
    )
    .await?;
    if !completed {
        let run_now = cq::get_run(&mut tx, run.id)
            .await?
            .ok_or_else(|| ApiError::Internal("run missing".into()))?;
        open_round(state, &mut tx, &run_now, cfg, new_ckpt, next_step, 1).await?;
    }
    misc::outbox_insert(&mut tx, "collective.round_aggregated", &serde_json::json!({ "run_id": run.id, "step": next_step, "examples": covered, "tasks": parts_len, "completed": completed })).await?;
    tx.commit().await?;
    tracing::info!(run = %run.id, step = next_step, examples = covered, completed, "collective round aggregated");
    Ok(true)
}

/// One coordinator pass over open rounds.
pub async fn coordinator_once(state: &AppState) -> Result<usize, ApiError> {
    let ids: Vec<Uuid> = {
        let mut c = state
            .db()
            .pool()
            .acquire()
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?;
        sqlx::query_as::<_, (Uuid,)>(
            "SELECT id FROM collective_rounds WHERE state = 'open' ORDER BY created_at LIMIT 50",
        )
        .fetch_all(&mut *c)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .into_iter()
        .map(|(id,)| id)
        .collect()
    };
    let mut n = 0;
    for id in ids {
        match super::retrying(|| process_round(state, id)).await {
            Ok(true) => n += 1,
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(round = %id, error = %e, "coordinator: round processing failed")
            }
        }
    }
    Ok(n)
}

/// Coordinator loop. A pass runs as soon as a submission or a decision is recorded
/// (`AppState::signal_submission`, `signal_decision`), and every 2 s regardless.
pub async fn run(state: AppState, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    loop {
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {}
            _ = state.signals().coordinator.notified() => {}
            _ = shutdown.changed() => { if *shutdown.borrow() { break; } continue; }
        }
        match coordinator_once(&state).await {
            // A pass that opened a wave or a round created claimable tasks.
            Ok(n) if n > 0 => state.signal_work(),
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "coordinator pass failed"),
        }
    }
}
