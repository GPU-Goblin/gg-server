//! Rounds of a model split between machines (docs/ROADMAP.md P11 D2, ADR-0024, ADR-0026).
//!
//! Each micro-batch goes through `2K` waves: the forward passes of stages 0 … K−1, then the
//! backward passes from the last stage back to the first. The micro-batches are **pipelined**:
//! each advances on its own, one job of one `graph.stage.v1` task per step, and its next step opens
//! as soon as its current task has **submitted**, before verification (speculation). The
//! coordinator remembers the attempt behind every output it passed on; if one of them is not
//! accepted in the end, the round is poisoned, its jobs are cancelled in the same transaction and
//! it is reissued with the same checkpoint. Once every micro-batch has submitted every wave and
//! everything is accepted, it aggregates shard by shard and publishes the checkpoint, as a
//! data-parallel round does.
//!
//! Activations travel as artifacts: served by the worker that produced them (before their durable
//! copy when the worker declared them), checked by the validator that recomputed the stage once
//! the durable copy exists.

use std::collections::BTreeMap;
use std::time::Instant;

use gg_compute::grad::CoordOptim;
use gg_compute::graph::stage_task::{self, Expected, Resolved};
use gg_core::*;
use gg_protocol::canonical;
use gg_protocol::domain::Domain;
use gg_protocol::messages::{
    ArtifactRef, GraphStageParamsV1, ResultSubmissionV1, TaskParamsV1, TaskSpecV1,
};
use gg_protocol::signed::Signed;
use gg_protocol::tensor_file::TensorF32;
use gg_storage::models::TaskRow;
use gg_storage::queries::collective as cq;
use gg_storage::queries::{jobs, misc, tasks};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::collective::{
    CheckpointRefs, CollectiveConfig, GraphRunSpec, StepAssembly, StepTimes, commit_step,
    keeps_state, load_bundle_as, read_rows, round_seed, run_plan, shard_step, slices, store_tensor,
};
use crate::error::ApiError;
use crate::state::AppState;

/// What earlier waves of a round produced, per micro-batch.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StageState {
    /// Micro-batches, each advancing through its own waves (pipeline, ADR-0026).
    pub micro: Vec<Micro>,
    /// Attempts whose submitted outputs were passed on to later waves: each must end accepted.
    #[serde(default)]
    pub recorded: Vec<Uuid>,
}

/// One micro-batch of a staged round.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Micro {
    /// First position in the round's index stream.
    pub start: u64,
    /// Examples.
    pub count: u32,
    /// Its rows of each dataset.
    pub data: BTreeMap<String, ArtifactRef>,
    /// Activations produced by forward passes, by tensor name.
    pub produced: BTreeMap<String, ArtifactRef>,
    /// Gradients of activations received from consumers, `tensor#c`.
    pub received: BTreeMap<String, ArtifactRef>,
    /// Parameter gradient bundles, `(gradient.j, artifact)`, in production order.
    pub grads: Vec<(String, ArtifactRef)>,
    /// Its current wave: `w < K` forward of stage `w`, then backward of stage `2K − 1 − w`; `2K`
    /// once every wave of this micro-batch has submitted.
    #[serde(default)]
    pub wave: usize,
    /// Its job of each wave opened so far, in wave order (one task each).
    #[serde(default)]
    pub jobs: Vec<Uuid>,
}

impl StageState {
    /// Every job opened for this round, all micro-batches together.
    pub fn jobs(&self) -> Vec<Uuid> {
        self.micro
            .iter()
            .flat_map(|m| m.jobs.iter().copied())
            .collect()
    }
}

fn wave_of(k: usize, wave: usize) -> (usize, bool) {
    if wave < k {
        (wave, false)
    } else {
        (2 * k - 1 - wave, true)
    }
}

async fn resolved(state: &AppState, g: &GraphRunSpec) -> Result<Resolved, ApiError> {
    let (model, _) = run_plan(state, g).await?;
    stage_task::resolve(model, &g.stage_bounds).map_err(|e| ApiError::Internal(e.to_string()))
}

/// Create the task of micro-batch `i` for its current wave, under `job_id`.
#[allow(clippy::too_many_arguments)]
async fn create_task(
    state: &AppState,
    tx: &mut gg_storage::PgConnection,
    run: &cq::CollectiveRunRow,
    cfg: &CollectiveConfig,
    ckpt: &CheckpointRefs,
    r: &Resolved,
    (i, m): (usize, &Micro),
    seed: u64,
    job_id: JobId,
) -> Result<(), ApiError> {
    let g = cfg
        .graph
        .as_ref()
        .ok_or_else(|| ApiError::Internal("staged round without a graph".into()))?;
    let (stage, backward) = wave_of(r.stages.len(), m.wave);
    let (flops, act) = stage_task::stage_costs(r, stage);
    {
        let p = GraphStageParamsV1 {
            graph_hash: g.graph.hash.to_hex(),
            graph_bytes: g.graph.size,
            parameters: g.parameters,
            stage_bounds: g.stage_bounds.clone(),
            stage: stage as u32,
            backward,
            datasets: g.datasets.clone(),
            round_seed: seed,
            example_start: m.start,
            example_count: m.count,
            total_examples: cfg.total_examples,
            training_flops_per_example: flops,
            activation_bytes_per_example: act,
            gradient_bf16: g.gradient_bf16,
        };
        let mut inputs =
            BTreeMap::from([(gg_compute::graph::task::GRAPH_INPUT.to_owned(), g.graph)]);
        for (name, what) in stage_task::expected_inputs(r, &p) {
            let a = match &what {
                Expected::Shard(j) => ckpt.weights.get(&r.plan.checkpoint_name(*j)).copied(),
                Expected::Dataset(t) => m.data.get(t).copied(),
                Expected::Activation(t) => m.produced.get(t).copied(),
                Expected::OutputGradient(t, c) => m.received.get(&format!("{t}#{c}")).copied(),
            }
            .ok_or_else(|| {
                ApiError::Internal(format!("stage {stage}: input {name} not produced yet"))
            })?;
            inputs.insert(name, a);
        }
        let device = gg_compute::graph::stages::stage_memory(
            &r.graph,
            r.prepared.report(),
            &r.stages[stage],
            m.count as u64,
        );
        let output_bytes = stage_task::output_bytes(r, &p);
        let def = gg_compute::catalog::graph_stage_definition(
            p,
            inputs,
            gg_compute::catalog::graph_stage_ram_bytes(device),
            output_bytes,
        )?;
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
                compute_kind: stage_task::KIND,
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
                    device_bytes: device as i64,
                    profile: g.required_profile as i32,
                },
            },
        )
        .await?;
    }
    Ok(())
}

async fn new_job(
    tx: &mut gg_storage::PgConnection,
    run: &cq::CollectiveRunRow,
    step: i32,
    attempt: i32,
    micro: usize,
    wave: usize,
) -> Result<JobId, ApiError> {
    let job_id = JobId::new();
    jobs::create_job(
        tx,
        job_id,
        ProjectId(run.project_id),
        ExperimentId(run.experiment_id),
        None,
        &format!("{}-round{step}-a{attempt}-wave{wave}-m{micro}", run.name),
    )
    .await?;
    Ok(job_id)
}

/// Open a staged round: select every micro-batch's rows, then the first wave.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn open_round(
    state: &AppState,
    tx: &mut gg_storage::PgConnection,
    run: &cq::CollectiveRunRow,
    cfg: &CollectiveConfig,
    ckpt: &CheckpointRefs,
    step: i32,
    attempt: i32,
) -> Result<Uuid, ApiError> {
    let g = cfg
        .graph
        .as_ref()
        .ok_or_else(|| ApiError::Internal("staged round without a graph".into()))?;
    let r = resolved(state, g).await?;
    let seed = round_seed(cfg, step);
    let mut st = StageState::default();
    for (start, count) in slices(cfg.batch_per_round, cfg.tasks_per_round) {
        let idx = gg_compute::graph::task::round_indices(seed, cfg.total_examples, start, count);
        let mut data = BTreeMap::new();
        for d in &g.datasets {
            let parts =
                match g.dataset_parts.get(&d.name) {
                    Some(list) => list.clone(),
                    None => vec![*g.dataset_refs.get(&d.name).ok_or_else(|| {
                        ApiError::Internal(format!("dataset {} missing", d.name))
                    })?],
                };
            let rows = read_rows(state, &parts, &d.shape, &idx).await?;
            data.insert(
                d.name.clone(),
                store_tensor(state, ProjectId(run.project_id), run.id, &rows).await?,
            );
        }
        st.micro.push(Micro {
            start,
            count,
            data,
            ..Micro::default()
        });
    }
    let mut job_id = None;
    for i in 0..st.micro.len() {
        let job = new_job(tx, run, step, attempt, i, 0).await?;
        create_task(state, tx, run, cfg, ckpt, &r, (i, &st.micro[i]), seed, job).await?;
        st.micro[i].jobs.push(job.0);
        job_id = Some(job);
    }
    let job_id =
        job_id.ok_or_else(|| ApiError::Internal("staged round without micro-batch".into()))?;
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
    let json = serde_json::to_value(&st).map_err(|e| ApiError::Internal(e.to_string()))?;
    cq::set_stage_state(tx, round_id, job_id.0, &json).await?;
    misc::outbox_insert(tx, "collective.round_opened", &serde_json::json!({ "run_id": run.id, "step": step, "attempt": attempt, "job_id": job_id, "staged": true })).await?;
    Ok(round_id)
}

/// The submitted (or already accepted) attempt of a task and its outputs, or `None` while the task
/// has no submission yet.
async fn submitted_outputs(
    tx: &mut gg_storage::PgConnection,
    t: &TaskRow,
) -> Result<Option<(Uuid, BTreeMap<String, ArtifactRef>)>, ApiError> {
    let state = t
        .task_state()
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    if !matches!(state, TaskState::Submitted | TaskState::Accepted) {
        return Ok(None);
    }
    let Some(attempt) = tasks::list_attempts(tx, TaskId(t.id))
        .await?
        .into_iter()
        .rev()
        .find(|a| a.state == "submitted" || a.state == "accepted")
    else {
        return Ok(None);
    };
    let sub = tasks::get_submission_by_attempt(tx, AttemptId(attempt.id))
        .await?
        .ok_or_else(|| ApiError::Internal("submitted attempt without submission".into()))?;
    let signed: Signed<ResultSubmissionV1> = Signed::from_canonical_bytes(&sub.submission_cbor)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Some((attempt.id, signed.body.outputs)))
}

/// Record what the task of micro-batch `m` for its current wave submitted: activations for later
/// stages, gradients of inputs for earlier ones, parameter gradients for the aggregation.
fn record_task(
    r: &Resolved,
    m: &mut Micro,
    recorded: &mut Vec<Uuid>,
    t: &TaskRow,
    (attempt, outs): (Uuid, BTreeMap<String, ArtifactRef>),
) -> Result<(), ApiError> {
    let k = r.stages.len();
    let (stage, backward) = wave_of(k, m.wave);
    let spec = &r.stages[stage];
    let task_spec: TaskSpecV1 =
        canonical::from_bytes(&t.spec_cbor).map_err(|e| ApiError::Internal(e.to_string()))?;
    match &task_spec.definition.params {
        TaskParamsV1::GraphStage(p)
            if p.stage as usize == stage
                && p.backward == backward
                && p.example_start == m.start => {}
        _ => {
            return Err(ApiError::Internal(
                "stage task does not match its micro-batch".into(),
            ));
        }
    }
    recorded.push(attempt);
    for (name, a) in outs {
        if let Some(k_out) = name.strip_prefix("out.") {
            let idx: usize = k_out
                .parse()
                .map_err(|_| ApiError::Internal(name.clone()))?;
            let tensor = spec
                .outputs
                .get(idx)
                .ok_or_else(|| ApiError::Internal(name.clone()))?;
            m.produced.insert(tensor.clone(), a);
        } else if let Some(k_in) = name.strip_prefix("din.") {
            let idx: usize = k_in.parse().map_err(|_| ApiError::Internal(name.clone()))?;
            let tensor = spec
                .inputs
                .get(idx)
                .ok_or_else(|| ApiError::Internal(name.clone()))?;
            let producer = r
                .stages
                .iter()
                .position(|s| s.outputs.contains(tensor))
                .ok_or_else(|| ApiError::Internal(format!("no producer of {tensor}")))?;
            let c = r
                .consumers(producer, tensor)
                .iter()
                .position(|x| *x == stage)
                .ok_or_else(|| ApiError::Internal(format!("{tensor} not consumed here")))?;
            m.received.insert(format!("{tensor}#{c}"), a);
        } else if name.starts_with("gradient.") {
            m.grads.push((name, a));
        }
    }
    Ok(())
}

/// Advance a staged round (ADR-0026).
///
/// Waves are **speculative**: the next wave opens as soon as every task of the current one has
/// *submitted*, while their verifications run. The coordinator remembers which attempt produced each
/// output it passed on. Nothing is aggregated until every task of every wave is accepted **and**
/// every remembered attempt is the accepted one. If one of them is rejected, expires or is replaced,
/// the round is poisoned: its jobs are cancelled and it is reissued from wave 0 with the same
/// checkpoint, exactly as a failed task already did. A rejected output can therefore cost a
/// round, never a wrong checkpoint.
pub(crate) async fn advance(
    state: &AppState,
    mut tx: sqlx::Transaction<'static, sqlx::Postgres>,
    run: cq::CollectiveRunRow,
    cfg: CollectiveConfig,
    round: cq::CollectiveRoundRow,
) -> Result<bool, ApiError> {
    let g = cfg
        .graph
        .clone()
        .ok_or_else(|| ApiError::Internal("staged round without a graph".into()))?;
    let r = resolved(state, &g).await?;
    let k = r.stages.len();
    let mut st: StageState = serde_json::from_value(
        cq::stage_state(&mut tx, round.id)
            .await?
            .ok_or_else(|| ApiError::Internal("staged round without state".into()))?,
    )
    .map_err(|e| ApiError::Internal(e.to_string()))?;
    let k2 = 2 * r.stages.len();

    // The task of every wave opened so far, per micro-batch (one task per job).
    let mut rows: Vec<Vec<TaskRow>> = Vec::with_capacity(st.micro.len());
    for m in &st.micro {
        let mut mine = Vec::with_capacity(m.jobs.len());
        for j in &m.jobs {
            let mut t = jobs::list_tasks(&mut tx, JobId(*j)).await?;
            if t.len() != 1 {
                return Err(ApiError::Internal("a stage job holds one task".into()));
            }
            mine.push(t.remove(0));
        }
        rows.push(mine);
    }
    let task_states: Vec<TaskState> = rows
        .iter()
        .flatten()
        .map(|t| t.task_state())
        .collect::<Result<_, _>>()
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let failed = task_states
        .iter()
        .any(|s| matches!(s, TaskState::Failed | TaskState::Cancelled));
    let recorded = tasks::attempt_states(&mut tx, &st.recorded).await?;
    // An output already passed on must end accepted. Inconclusive waits for its dispute.
    let poisoned = recorded
        .iter()
        .find(|(_, s)| !matches!(s.as_str(), "submitted" | "accepted" | "inconclusive"));
    // An activation passed on before its durable copy must see that copy arrive in time (ADR-0026).
    let overdue = gg_storage::queries::pending::overdue(&mut tx, &st.recorded).await?;
    if failed || poisoned.is_some() || recorded.len() != st.recorded.len() || !overdue.is_empty() {
        if let Some((a, s)) = poisoned {
            tracing::warn!(run = %run.id, step = round.step, attempt = %a, state = %s, "staged round: an output already passed on was not accepted, round reissued");
        }
        if let Some(a) = overdue.first() {
            tracing::warn!(run = %run.id, step = round.step, attempt = %a, "staged round: an activation passed on never reached durable storage, round reissued");
        }
        let jobs = st.jobs();
        return super::collective::fail_round(state, tx, run, cfg, round, jobs).await;
    }

    // Pipeline: each micro-batch advances on its own. As soon as its current task is submitted,
    // the next one opens, without waiting for the other micro-batches.
    let mut last_job = None;
    let mut ckpt: Option<CheckpointRefs> = None;
    for i in 0..st.micro.len() {
        if st.micro[i].wave >= k2 {
            continue;
        }
        let t = &rows[i][st.micro[i].wave];
        let Some(outs) = submitted_outputs(&mut tx, t).await? else {
            continue;
        };
        record_task(&r, &mut st.micro[i], &mut st.recorded, t, outs)?;
        st.micro[i].wave += 1;
        if st.micro[i].wave < k2 {
            if ckpt.is_none() {
                ckpt = Some(
                    serde_json::from_value(run.checkpoint.clone())
                        .map_err(|e| ApiError::Internal(e.to_string()))?,
                );
            }
            let job = new_job(
                &mut tx,
                &run,
                round.step,
                round.attempt,
                i,
                st.micro[i].wave,
            )
            .await?;
            create_task(
                state,
                &mut tx,
                &run,
                &cfg,
                ckpt.as_ref().expect("loaded above"),
                &r,
                (i, &st.micro[i]),
                round.round_seed as u64,
                job,
            )
            .await?;
            st.micro[i].jobs.push(job.0);
            last_job = Some(job.0);
        } else {
            last_job.get_or_insert(round.job_id);
        }
    }
    if let Some(job) = last_job {
        let json = serde_json::to_value(&st).map_err(|e| ApiError::Internal(e.to_string()))?;
        cq::set_stage_state(&mut tx, round.id, job, &json).await?;
        tx.commit().await?;
        tracing::debug!(run = %run.id, step = round.step, "staged round: micro-batches advanced on submitted outputs");
        return Ok(true);
    }
    if st.micro.iter().any(|m| m.wave < k2) {
        tx.rollback().await?;
        return Ok(false);
    }

    // Every wave recorded: wait until every verification has accepted.
    if !task_states.iter().all(|s| *s == TaskState::Accepted)
        || !recorded.iter().all(|(_, s)| s == "accepted")
    {
        tx.rollback().await?;
        return Ok(false);
    }
    let ckpt: CheckpointRefs = serde_json::from_value(run.checkpoint.clone())
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    // Last wave accepted: aggregate shard by shard.
    let optim = CoordOptim::parse(&cfg.optimizer)
        .ok_or_else(|| ApiError::Internal("bad optimizer".into()))?;
    let covered: u64 = st.micro.iter().map(|m| m.count as u64).sum();
    if covered != round.expected_examples as u64 {
        return Err(ApiError::Internal("staged round coverage mismatch".into()));
    }
    // The run's state stays in memory between rounds when it fits, except after the last round
    // (ADR-0036).
    let keep = keeps_state(state, &r.plan) && round.step as u32 + 1 < cfg.rounds;
    let mut steps = StepAssembly::new(&ckpt);
    let mut times = StepTimes::default();
    for (j, shard) in r.plan.shards.iter().enumerate() {
        let key = r.plan.checkpoint_name(j);
        if shard.gradient_bytes == 0 {
            steps.unchanged(&key)?;
            continue;
        }
        let grad_name = r.plan.gradient_name(j);
        let started = Instant::now();
        let parts = shard_parts(state, &g, &r, &st, shard, &grad_name).await?;
        times.read += started.elapsed();
        // Gradients of the round that produced the current weights, should the moments have to be
        // brought back to them.
        let replay = async || {
            let mut c = state
                .db()
                .pool()
                .acquire()
                .await
                .map_err(|e| ApiError::Internal(e.to_string()))?;
            let from = cq::checkpoint_round(&mut c, run.id, run.step)
                .await?
                .ok_or_else(|| ApiError::Internal("no round recorded for the checkpoint".into()))?;
            let before: StageState = serde_json::from_value(
                cq::stage_state(&mut c, from.id)
                    .await?
                    .ok_or_else(|| ApiError::Internal("staged round without state".into()))?,
            )
            .map_err(|e| ApiError::Internal(e.to_string()))?;
            drop(c);
            shard_parts(state, &g, &r, &before, shard, &grad_name).await
        };
        let done = shard_step(
            state,
            &run,
            &g,
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
    let new_ckpt = steps.finish();
    commit_step(
        state,
        tx,
        &run,
        &cfg,
        &round,
        &new_ckpt,
        covered,
        st.micro.len() * 2 * k,
    )
    .await
}

/// A shard's gradient per micro-batch of a staged round: the partial gradients of every stage that
/// uses a parameter of the shard, summed in production order (stages in decreasing order).
async fn shard_parts(
    state: &AppState,
    g: &GraphRunSpec,
    r: &Resolved,
    st: &StageState,
    shard: &gg_compute::graph::shards::Shard,
    grad_name: &str,
) -> Result<Vec<(u32, BTreeMap<String, TensorF32>)>, ApiError> {
    let partials = futures_util::future::try_join_all(st.micro.iter().map(|m| async move {
        let partial = futures_util::future::try_join_all(
            m.grads
                .iter()
                .filter(|(n, _)| n == grad_name)
                .map(|(_, a)| load_bundle_as(state, a, g, a.size, g.gradient_dtype())),
        )
        .await?;
        Ok::<_, ApiError>((m.count, partial))
    }))
    .await?;
    let mut parts: Vec<(u32, BTreeMap<String, TensorF32>)> = Vec::new();
    for (count, partial) in partials {
        let mut sum: BTreeMap<String, TensorF32> = BTreeMap::new();
        for part in partial {
            for (pn, t) in part {
                match sum.get_mut(&pn) {
                    Some(acc) => acc.data.iter_mut().zip(&t.data).for_each(|(x, y)| *x += *y),
                    None => {
                        sum.insert(pn, t);
                    }
                }
            }
        }
        // Every trained parameter of the shard: one no stage used gets a zero gradient.
        for pn in &shard.params {
            if let Some(p) = r.graph.parameters.iter().find(|q| &q.name == pn)
                && p.trainable
                && !sum.contains_key(pn)
            {
                let n: u64 = p.shape.iter().product();
                sum.insert(
                    pn.clone(),
                    TensorF32::new(p.shape.clone(), vec![0.0; n as usize])
                        .map_err(|e| ApiError::Internal(e.to_string()))?,
                );
            }
        }
        parts.push((count, sum));
    }
    Ok(parts)
}
