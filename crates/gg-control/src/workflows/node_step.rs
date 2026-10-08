//! Runs stepped by a lab node (ADR-0041, A6). The node draws the initial weights from the run's
//! seed, averages the accepted gradients of each round and applies the optimizer step, with the
//! same code as the coordinator (`aggregate_named`, `apply_step_named`), and holds every
//! checkpoint. The control plane tells it what is due, checks what it reports against the run's
//! shard plan (sizes, holder, step), records the checkpoint and opens the next round.

use std::collections::BTreeMap;

use gg_compute::grad::CoordOptim;
use gg_core::*;
use gg_protocol::messages::ArtifactRef;
use gg_storage::queries::{artifacts, collective as cq, jobs, misc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::collective::{
    CheckpointRefs, CollectiveConfig, GraphRunSpec, commit_step, open_round, round_graph_outputs,
    round_seed, run_plan, slices,
};
use crate::error::ApiError;
use crate::state::AppState;

/// One accepted task of a round: its micro-batch size and its outputs (the gradient shards).
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
pub struct StepPart {
    pub example_count: u32,
    /// A data-parallel round: the task's outputs, one gradient bundle per shard.
    #[schema(value_type = Object)]
    pub outputs: BTreeMap<String, ArtifactRef>,
    /// A staged round: the micro-batch's partial gradient bundles (`gradient.j`, artifact) in
    /// production order; a shard's gradient is their sum in that order.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[schema(value_type = Vec<Object>)]
    pub partials: Vec<(String, ArtifactRef)>,
}

/// A step due on a run the node holds: step 0 (the initial weights) or a round whose tasks were
/// all accepted.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
pub struct StepWork {
    pub run_id: Uuid,
    pub project_id: ProjectId,
    /// The step the reported checkpoint will be: 0 for the initial weights, else the round's
    /// step plus one.
    pub step: i32,
    /// The round whose gradients to apply; absent for step 0.
    pub round_id: Option<Uuid>,
    /// The run's configuration (seed, optimizer, learning rate, description, shard size).
    #[schema(value_type = Object)]
    pub config: serde_json::Value,
    /// The current checkpoint (empty for step 0).
    pub checkpoint: CheckpointRefs,
    /// Accepted tasks in slice order: what to average, weighted by example count.
    pub parts: Vec<StepPart>,
}

/// Steps due on the running runs `node` holds.
pub async fn due(state: &AppState, node: NodeId) -> Result<Vec<StepWork>, ApiError> {
    let mut c = state.db().pool().acquire().await?;
    let mut out = Vec::new();
    for run in cq::running_runs_of_node(&mut c, node.0).await? {
        let ckpt: CheckpointRefs = serde_json::from_value(run.checkpoint.clone())
            .map_err(|e| ApiError::Internal(e.to_string()))?;
        if run.step == 0 && ckpt.weights.is_empty() {
            out.push(StepWork {
                run_id: run.id,
                project_id: ProjectId(run.project_id),
                step: 0,
                round_id: None,
                config: run.config.clone(),
                checkpoint: ckpt,
                parts: Vec::new(),
            });
            continue;
        }
        let Some(round) = cq::open_round_of(&mut c, run.id).await? else {
            continue;
        };
        if round.step != run.step {
            continue;
        }
        let Some(parts) = round_parts(&mut c, &run, &round).await? else {
            continue;
        };
        out.push(StepWork {
            run_id: run.id,
            project_id: ProjectId(run.project_id),
            step: run.step + 1,
            round_id: Some(round.id),
            config: run.config.clone(),
            checkpoint: ckpt,
            parts,
        });
    }
    Ok(out)
}

/// What the node averages for `round`, once it is complete: every task accepted (and, staged,
/// every wave recorded); `None` before.
async fn round_parts(
    c: &mut sqlx::PgConnection,
    run: &cq::CollectiveRunRow,
    round: &cq::CollectiveRoundRow,
) -> Result<Option<Vec<StepPart>>, ApiError> {
    let cfg: CollectiveConfig = serde_json::from_value(run.config.clone())
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let bounds = cfg.graph.as_ref().map_or(0, |g| g.stage_bounds.len());
    if bounds > 0 {
        let stages = bounds.saturating_sub(1);
        return Ok(super::staged::node_parts(c, round, stages)
            .await?
            .map(|parts| {
                parts
                    .into_iter()
                    .map(|(example_count, partials)| StepPart {
                        example_count,
                        outputs: BTreeMap::new(),
                        partials,
                    })
                    .collect()
            }));
    }
    if !round_accepted(c, round.job_id).await? {
        return Ok(None);
    }
    Ok(Some(
        round_graph_outputs(c, round.job_id)
            .await?
            .into_iter()
            .map(|(example_count, outputs)| StepPart {
                example_count,
                outputs,
                partials: Vec::new(),
            })
            .collect(),
    ))
}

async fn round_accepted(c: &mut sqlx::PgConnection, job: Uuid) -> Result<bool, ApiError> {
    let tasks = jobs::list_tasks(c, JobId(job)).await?;
    Ok(!tasks.is_empty()
        && tasks
            .iter()
            .all(|t| t.task_state().is_ok_and(|s| s == TaskState::Accepted)))
}

/// What a node reports after a step.
#[derive(Clone, Debug, Deserialize, utoipa::ToSchema)]
pub struct StepReport {
    /// The step of the new checkpoint (as announced in [`StepWork`]).
    pub step: i32,
    /// The round applied; absent for step 0.
    #[serde(default)]
    pub round_id: Option<Uuid>,
    /// The new checkpoint, every artifact of it held by the node.
    pub checkpoint: CheckpointRefs,
}

/// Check and record a step the node reports, then open the next round (or complete the run).
pub async fn accept(
    state: &AppState,
    node: NodeId,
    run_id: Uuid,
    report: StepReport,
) -> Result<bool, ApiError> {
    let mut tx = state.db().begin().await?;
    let run = cq::lock_run(&mut tx, run_id)
        .await?
        .ok_or_else(|| ApiError::NotFound("run".into()))?;
    if run.holder_node != Some(node.0) {
        return Err(ApiError::NotFound("run".into()));
    }
    if run.state != "running" {
        return Err(ApiError::Conflict(format!("run is {}", run.state)));
    }
    let cfg: CollectiveConfig = serde_json::from_value(run.config.clone())
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let g = cfg
        .graph
        .as_ref()
        .ok_or_else(|| ApiError::Internal("node run without a graph".into()))?;
    let prev: CheckpointRefs = serde_json::from_value(run.checkpoint.clone())
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let init = prev.weights.is_empty();
    let new = &report.checkpoint;

    // The step: 0 before any checkpoint, else the next one after the open round.
    let round = if init {
        if report.step != 0 || report.round_id.is_some() || run.step != 0 {
            return Err(ApiError::Conflict(
                "step 0 is due: the initial weights".into(),
            ));
        }
        None
    } else {
        let round = cq::open_round_of(&mut tx, run.id)
            .await?
            .ok_or_else(|| ApiError::Conflict("no round open".into()))?;
        if Some(round.id) != report.round_id
            || round.step != run.step
            || report.step != run.step + 1
        {
            return Err(ApiError::Conflict(format!(
                "step {} of round {} is due",
                run.step + 1,
                round.id
            )));
        }
        Some(round)
    };

    // The checkpoint: every shard of the plan, of the plan's size, held by this node; untrained
    // shards unchanged; optimizer state as the optimizer needs it, one step further.
    let (_, plan) = run_plan(state, g).await?;
    let adam = CoordOptim::parse(&cfg.optimizer) == Some(CoordOptim::Adam);
    let bad = |why: String| Err(ApiError::Unprocessable(format!("checkpoint: {why}")));
    if new.weights.len() != plan.shards.len() || new.opt_lag != 0 {
        return bad(format!("{} shards and no lag expected", plan.shards.len()));
    }
    let expected_t = if init { 0 } else { prev.opt_t + 1 };
    if new.opt_t != expected_t {
        return bad(format!(
            "optimizer step {expected_t} expected, {}",
            new.opt_t
        ));
    }
    let mut held: Vec<&ArtifactRef> = Vec::new();
    for (i, shard) in plan.shards.iter().enumerate() {
        let key = plan.checkpoint_name(i);
        let Some(w) = new.weights.get(&key) else {
            return bad(format!("{key} missing"));
        };
        if w.size != shard.checkpoint_bytes {
            return bad(format!(
                "{key} is {} bytes, the plan says {}",
                w.size, shard.checkpoint_bytes
            ));
        }
        if !init && shard.gradient_bytes == 0 && prev.weights.get(&key) != Some(w) {
            return bad(format!(
                "{key} holds no trained parameter and must not change"
            ));
        }
        held.push(w);
        let moments = [(&new.opt_m, "m"), (&new.opt_v, "v")];
        for (map, which) in moments {
            let r = map.as_ref().and_then(|m| m.get(&key));
            match (adam && !init && shard.gradient_bytes > 0, r) {
                (true, Some(r)) if r.size == shard.gradient_bytes => held.push(r),
                (true, _) => return bad(format!("{which} of {key} missing or of the wrong size")),
                (false, Some(_)) => return bad(format!("unexpected {which} for {key}")),
                (false, None) => {}
            }
        }
    }
    for r in held {
        let row = artifacts::get_artifact(&mut tx, &r.hash).await?;
        let ok = row
            .is_some_and(|a| a.size as u64 == r.size && a.project_id == Some(run.project_id))
            && (artifacts::held_by_node(&mut tx, &r.hash).await? == Some(node)
                || state.objects().exists(&r.hash));
        if !ok {
            return bad(format!(
                "{} is not an artifact this node holds for the project",
                r.hash
            ));
        }
    }
    misc::audit(
        &mut tx,
        &format!("node:{node}"),
        "node.step",
        &run.id.to_string(),
        serde_json::json!({ "step": report.step, "round_id": report.round_id }),
    )
    .await?;

    match round {
        None => {
            let ckpt_json =
                serde_json::to_value(new).map_err(|e| ApiError::Internal(e.to_string()))?;
            cq::insert_checkpoint(&mut tx, run.id, 0, &ckpt_json, None).await?;
            cq::update_run(&mut tx, run.id, "running", 0, &ckpt_json, "").await?;
            let run_now = cq::get_run(&mut tx, run.id)
                .await?
                .ok_or_else(|| ApiError::Internal("run missing".into()))?;
            open_round(state, &mut tx, &run_now, &cfg, new, 0, 1).await?;
            tx.commit().await?;
            tracing::info!(run = %run.id, %node, "initial weights drawn by the lab node: round 0 open");
            state.signal_work();
            Ok(true)
        }
        Some(round) => {
            let parts = round_parts(&mut tx, &run, &round)
                .await?
                .ok_or_else(|| ApiError::Conflict("the round is not accepted yet".into()))?;
            let covered: u64 = parts.iter().map(|p| p.example_count as u64).sum();
            if covered != round.expected_examples as u64
                || parts.len() != cfg.tasks_per_round as usize
            {
                return Err(ApiError::Internal("round coverage mismatch".into()));
            }
            let tasks = if g.stage_bounds.is_empty() {
                parts.len()
            } else {
                parts.len() * 2 * (g.stage_bounds.len() - 1)
            };
            let done = commit_step(state, tx, &run, &cfg, &round, new, covered, tasks).await?;
            state.signal_work();
            Ok(done)
        }
    }
}

/// One task's slice of a round: which examples of the round's order it takes.
#[derive(Clone, Copy, Debug, Serialize, utoipa::ToSchema)]
pub struct RowSlice {
    pub example_start: u64,
    pub example_count: u32,
}

/// Micro-batch rows a node is to cut for a round of a run it steps (the cutting by the node): for
/// each task, the rows `round_indices(round_seed, total_examples, start, count)` of every dataset.
/// Due for the open round and the next one, so the node cuts them while the round computes.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
pub struct RowsDue {
    pub run_id: Uuid,
    pub project_id: ProjectId,
    pub step: i32,
    pub round_seed: u64,
    pub total_examples: u64,
    pub slices: Vec<RowSlice>,
    /// The datasets: name, full shape, and the artifacts of consecutive rows it is made of.
    #[schema(value_type = Vec<Object>)]
    pub datasets: Vec<RowsDataset>,
}

#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
pub struct RowsDataset {
    pub name: String,
    pub shape: Vec<u64>,
    #[schema(value_type = Vec<Object>)]
    pub parts: Vec<ArtifactRef>,
}

/// What a node declares after cutting the rows of a step: one map per task, in slice order.
#[derive(Clone, Debug, Deserialize, utoipa::ToSchema)]
pub struct RowsReport {
    pub step: i32,
    #[schema(value_type = Vec<Object>)]
    pub rows: Vec<BTreeMap<String, ArtifactRef>>,
}

/// The rows due on the runs `node` steps.
pub async fn rows_due(state: &AppState, node: NodeId) -> Result<Vec<RowsDue>, ApiError> {
    let mut c = state.db().pool().acquire().await?;
    let mut out = Vec::new();
    for run in cq::running_runs_of_node(&mut c, node.0).await? {
        let cfg: CollectiveConfig = serde_json::from_value(run.config.clone())
            .map_err(|e| ApiError::Internal(e.to_string()))?;
        let Some(g) = cfg.graph.as_ref().filter(|g| g.batch_inputs) else {
            continue;
        };
        for step in [run.step, run.step + 1] {
            if step < 0 || step as u32 >= cfg.rounds {
                continue;
            }
            if cq::node_rows(&mut c, run.id, step).await?.is_some() {
                continue;
            }
            out.push(RowsDue {
                run_id: run.id,
                project_id: ProjectId(run.project_id),
                step,
                round_seed: round_seed(&cfg, step),
                total_examples: cfg.total_examples,
                slices: slices(cfg.batch_per_round, cfg.tasks_per_round)
                    .into_iter()
                    .map(|(example_start, example_count)| RowSlice {
                        example_start,
                        example_count,
                    })
                    .collect(),
                datasets: dataset_parts(g)?,
            });
        }
    }
    Ok(out)
}

fn dataset_parts(g: &GraphRunSpec) -> Result<Vec<RowsDataset>, ApiError> {
    g.datasets
        .iter()
        .map(|d| {
            let parts =
                match g.dataset_parts.get(&d.name) {
                    Some(list) => list.clone(),
                    None => vec![*g.dataset_refs.get(&d.name).ok_or_else(|| {
                        ApiError::Internal(format!("dataset {} missing", d.name))
                    })?],
                };
            Ok(RowsDataset {
                name: d.name.clone(),
                shape: d.shape.clone(),
                parts,
            })
        })
        .collect()
}

/// Check and record the rows `node` cut for a step: one map per task, every dataset, each of the
/// size its slice gives, held by the node for the run's project. The server reads none of them.
/// Declaring the same rows again is accepted; other rows for a step already declared are not.
pub async fn accept_rows(
    state: &AppState,
    node: NodeId,
    run_id: Uuid,
    report: RowsReport,
) -> Result<(), ApiError> {
    let mut tx = state.db().begin().await?;
    let run = cq::lock_run(&mut tx, run_id)
        .await?
        .ok_or_else(|| ApiError::NotFound("run".into()))?;
    if run.holder_node != Some(node.0) {
        return Err(ApiError::NotFound("run".into()));
    }
    if run.state != "running" {
        return Err(ApiError::Conflict(format!("run is {}", run.state)));
    }
    let cfg: CollectiveConfig = serde_json::from_value(run.config.clone())
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let Some(g) = cfg.graph.as_ref().filter(|g| g.batch_inputs) else {
        return Err(ApiError::Unprocessable(
            "this run gives each task the whole datasets: no rows to cut".into(),
        ));
    };
    if report.step < run.step || report.step > run.step + 1 || report.step as u32 >= cfg.rounds {
        return Err(ApiError::Conflict(format!(
            "rows are due for steps {} and {} only",
            run.step,
            run.step + 1
        )));
    }
    let bad = |why: String| Err(ApiError::Unprocessable(format!("rows: {why}")));
    let slices = slices(cfg.batch_per_round, cfg.tasks_per_round);
    if report.rows.len() != slices.len() {
        return bad(format!("{} tasks expected", slices.len()));
    }
    for (i, (map, (_, count))) in report.rows.iter().zip(&slices).enumerate() {
        if map.len() != g.datasets.len() {
            return bad(format!("task {i}: {} datasets expected", g.datasets.len()));
        }
        for d in &g.datasets {
            let Some(r) = map.get(&d.name) else {
                return bad(format!("task {i}: {} missing", d.name));
            };
            let mut shape = d.shape.clone();
            shape[0] = *count as u64;
            let want = gg_compute::catalog::expected_rgt_size(&shape);
            if r.size != want {
                return bad(format!(
                    "task {i}: {} is {} bytes, {count} rows are {want}",
                    d.name, r.size
                ));
            }
            let row = artifacts::get_artifact(&mut tx, &r.hash).await?;
            if !row.is_some_and(|a| a.size as u64 == r.size && a.project_id == Some(run.project_id))
                || artifacts::held_by_node(&mut tx, &r.hash).await? != Some(node)
            {
                return bad(format!(
                    "{} is not an artifact this node holds for the project",
                    r.hash
                ));
            }
        }
    }
    let json = serde_json::to_value(&report.rows).map_err(|e| ApiError::Internal(e.to_string()))?;
    if let Some(prev) = cq::insert_node_rows(&mut tx, run.id, report.step, &json).await?
        && prev != json
    {
        return Err(ApiError::Conflict(format!(
            "other rows were declared for step {}",
            report.step
        )));
    }
    tx.commit().await?;
    Ok(())
}

/// The rows the node cut for `step` of `run`, one map per task.
pub(crate) async fn rows_of(
    c: &mut sqlx::PgConnection,
    run: Uuid,
    step: i32,
) -> Result<Vec<BTreeMap<String, ArtifactRef>>, ApiError> {
    let json = cq::node_rows(c, run, step).await?.ok_or_else(|| {
        ApiError::Conflict(format!(
            "the lab node has not declared the rows of step {step} yet"
        ))
    })?;
    serde_json::from_value(json).map_err(|e| ApiError::Internal(e.to_string()))
}
