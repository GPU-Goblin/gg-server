//! Researcher routes. Every route checks project membership; ids are never sufficient.

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::put;
use axum::{Json, Router};
use gg_core::*;
use gg_protocol::messages::{ContributionReceiptV1, ResultSubmissionV1, VerificationDecisionV1};
use gg_protocol::signed::Signed;
use gg_storage::queries::{identities, jobs, ledger as lq, misc, tasks};
use serde::{Deserialize, Serialize};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::auth::{Researcher, require_project_role};
use crate::error::ApiError;
use crate::state::AppState;
use crate::workflows::claim::NamedArtifact;
use crate::workflows::jobs::{JobPlan, SubmitJobRequest, submit_job};
use crate::workflows::{cancel, decode_spec};

#[derive(Serialize, utoipa::ToSchema)]
pub struct Budget {
    pub available: Credits,
    pub reserved: Credits,
    pub spent: Credits,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct ProjectView {
    pub project_id: ProjectId,
    pub name: String,
    pub state: String,
    pub allowed_kinds: Vec<String>,
    pub role: ProjectRole,
    pub budget: Budget,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct Me {
    pub researcher_id: ResearcherId,
    pub display_name: String,
    pub admission: AdmissionState,
    pub projects: Vec<ProjectView>,
}

async fn project_views(
    state: &AppState,
    researcher: ResearcherId,
) -> Result<Vec<ProjectView>, ApiError> {
    let mut c = state
        .db()
        .pool()
        .acquire()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let mut out = Vec::new();
    for p in identities::list_projects_for(&mut c, researcher).await? {
        let role = identities::membership_role(&mut c, ProjectId(p.id), researcher)
            .await?
            .unwrap_or(ProjectRole::Viewer);
        let b = gg_ledger::budget(&mut c, ProjectId(p.id)).await?;
        out.push(ProjectView {
            project_id: ProjectId(p.id),
            name: p.name,
            state: p.state,
            allowed_kinds: p.allowed_kinds,
            role,
            budget: Budget {
                available: b.available,
                reserved: b.reserved,
                spent: b.spent,
            },
        });
    }
    Ok(out)
}

/// One worker as it announced itself at its last claim (C1). No identity is exposed: a researcher
/// plans against capacities, not against machines.
#[derive(Serialize, utoipa::ToSchema)]
pub struct WorkerCapacity {
    /// Host memory, bytes (absent: an earlier worker).
    pub ram_bytes: Option<u64>,
    /// Accelerator memory, bytes.
    pub device_memory_bytes: Option<u64>,
    /// Operator profile understood.
    pub profile: String,
    /// Engine.
    pub engine: Option<String>,
}

/// Capacity of the grid: authorized workers that claimed recently.
#[derive(Serialize, utoipa::ToSchema)]
pub struct GridCapacity {
    /// Window, seconds.
    pub within_secs: u64,
    /// Operator profile of this server.
    pub server_profile: String,
    /// Workers, largest accelerator memory first.
    pub workers: Vec<WorkerCapacity>,
}

#[utoipa::path(get, path = "/researcher/grid/capacity", tag = "researcher", responses((status = 200, body = GridCapacity)))]
async fn grid_capacity(
    State(state): State<AppState>,
    Researcher(_id): Researcher,
) -> Result<Json<GridCapacity>, ApiError> {
    const WITHIN: u64 = 300;
    let mut c = state.db().pool().acquire().await?;
    let caps = identities::recent_capabilities(&mut c, WITHIN as i64).await?;
    Ok(Json(GridCapacity {
        within_secs: WITHIN,
        server_profile: gg_compute::graph::PROFILE_VERSION.to_owned(),
        workers: caps
            .into_iter()
            .map(|w| WorkerCapacity {
                ram_bytes: w.ram_bytes.map(|b| b as u64),
                device_memory_bytes: w.device_bytes.map(|b| b as u64),
                profile: format!("gg-ops/{}", w.profile),
                engine: w.engine,
            })
            .collect(),
    }))
}

/// What a researcher asks before submitting (C4): can the grid run this description, with which
/// memory levers?
#[derive(Deserialize, utoipa::ToSchema)]
pub struct PlanRequest {
    /// The `gg-graph/v1` description, inline.
    #[schema(value_type = Object)]
    pub graph: serde_json::Value,
    /// Examples per round.
    pub batch_per_round: u32,
    /// Tasks per round.
    pub tasks_per_round: u32,
}

/// Memory levers of one task.
#[derive(Serialize, utoipa::ToSchema)]
pub struct PlannedLevers {
    /// Segments of activation recomputation.
    pub segments: u32,
    /// Parameters uploaded segment by segment.
    pub offload_parameters: bool,
    /// Accelerator memory a task needs with these levers.
    pub task_device_bytes: u64,
    /// Workers announcing enough memory and a recent enough profile.
    pub fitting_workers: usize,
}

/// A split into stages, when no single worker can hold a task of the whole model (D2).
#[derive(Serialize, utoipa::ToSchema)]
pub struct PlannedStages {
    /// Number of stages.
    pub stages: usize,
    /// Stage boundaries in the execution order.
    pub stage_bounds: Vec<u32>,
    /// Memory the stages were cut under: the largest eligible worker's.
    pub stage_memory_bytes: u64,
    /// Accelerator memory of the largest stage.
    pub largest_stage_bytes: u64,
    /// Workers accepting `graph.stage.v1` that can hold the largest stage.
    pub fitting_workers: usize,
    /// When a single matrix does not fit a machine (D3): the size above which matrices are cut
    /// into tiles before cutting stages; pass it as `tile_max_param_bytes`.
    pub tile_max_param_bytes: Option<u64>,
    /// The matrices that would be cut, and into how many tiles.
    pub tiled: Vec<(String, usize)>,
}

/// The answer.
#[derive(Serialize, utoipa::ToSchema)]
pub struct PlanView {
    /// Minimal operator profile, `gg-ops/N`.
    pub required_profile: String,
    /// Parameters, frozen ones included.
    pub parameters: u64,
    /// Trained parameters.
    pub trainable_parameters: u64,
    /// Examples in the largest task.
    pub micro_batch: u64,
    /// Accelerator memory of a task without any lever.
    pub plain_task_device_bytes: u64,
    /// Workers seen during the last five minutes.
    pub workers_seen: usize,
    /// Of those, the ones whose profile can execute the description.
    pub workers_with_profile: usize,
    /// Largest accelerator memory among them.
    pub largest_device_bytes: Option<u64>,
    /// Host memory a whole-model task reserves on its worker (its limit, enforced).
    pub task_ram_bytes: u64,
    /// Workers with the profile that announce at least `task_ram_bytes` of host memory.
    pub workers_with_ram: usize,
    /// The cheapest levers that fit the largest eligible worker, if any.
    pub recommendation: Option<PlannedLevers>,
    /// When nothing fits whole: the model split into stages, each on its own worker (D2).
    pub staged: Option<PlannedStages>,
    /// One sentence: what to do.
    pub advice: String,
}

#[utoipa::path(post, path = "/researcher/plan", tag = "researcher", request_body = PlanRequest, responses((status = 200, body = PlanView), (status = 422, description = "description refused")))]
async fn plan_route(
    State(state): State<AppState>,
    Researcher(_id): Researcher,
    Json(req): Json<PlanRequest>,
) -> Result<Json<PlanView>, ApiError> {
    use gg_compute::graph::{self, memory};
    if req.tasks_per_round == 0 || req.tasks_per_round > req.batch_per_round {
        return Err(ApiError::Unprocessable(
            "tasks_per_round must be in 1..=batch_per_round".into(),
        ));
    }
    let model: graph::ModelGraphV1 = serde_json::from_value(req.graph)
        .map_err(|e| ApiError::Unprocessable(format!("graph is not gg-graph/v1: {e}")))?;
    let model = graph::functions::inline(&model)
        .map_err(|e| ApiError::Unprocessable(format!("graph refused: {e}")))?;
    let report = graph::validate(&model)
        .map_err(|e| ApiError::Unprocessable(format!("graph refused: {e}")))?;
    let batch = req.batch_per_round.div_ceil(req.tasks_per_round) as u64;
    let profile = memory::required_profile(&model);
    let plain = memory::task_memory(&model, &report, batch, Default::default()).total();
    // The same host-memory requirement the scheduler will put on each task: a worker announcing
    // less never claims it, whatever its accelerator holds.
    let checkpoint_bytes = graph::shards::plan(&model, &report, 0)
        .map_err(|e| ApiError::Unprocessable(format!("checkpoint shards: {e}")))?
        .checkpoint_bytes();
    let task_ram = gg_compute::catalog::graph_grad_ram_bytes(
        checkpoint_bytes,
        report.activation_bytes_per_example.saturating_mul(batch),
        0,
    );
    let mut c = state.db().pool().acquire().await?;
    let caps = identities::recent_capabilities(&mut c, 300).await?;
    let with_ram =
        |w: &&jobs::WorkerCapabilities| w.ram_bytes.is_some_and(|r| r.max(0) as u64 >= task_ram);
    let workers_with_ram = caps
        .iter()
        .filter(|w| w.profile as u32 >= profile)
        .filter(with_ram)
        .count();
    let eligible: Vec<u64> = caps
        .iter()
        .filter(|w| w.profile as u32 >= profile)
        .filter(with_ram)
        .filter_map(|w| w.device_bytes.map(|b| b.max(0) as u64))
        .collect();
    let largest = eligible.iter().copied().max();
    let recommendation = largest.and_then(|cap| {
        memory::recommend(&model, &report, batch, cap).map(|(o, m)| PlannedLevers {
            segments: o.segments as u32,
            offload_parameters: o.offload_parameters,
            task_device_bytes: m.total(),
            fitting_workers: eligible.iter().filter(|b| **b >= m.total()).count(),
        })
    });
    // Nothing fits whole: the model in stages, within the memory of the workers that accept them.
    // A stage reserves its memory estimate plus the executor in host memory
    // (`graph_stage_ram_bytes`): a worker's capacity for a stage is the smaller of the two.
    let stage_capable: Vec<u64> = caps
        .iter()
        .filter(|w| w.profile as u32 >= profile)
        .filter(|w| {
            w.kinds
                .iter()
                .any(|k| k == gg_compute::graph::stage_task::KIND)
        })
        .filter_map(|w| {
            let ram = w.ram_bytes?.max(0) as u64;
            if ram < gg_compute::catalog::MIN_GRAPH_RAM_BYTES {
                return None;
            }
            let device = w.device_bytes?.max(0) as u64;
            Some(device.min(ram - gg_compute::catalog::EXECUTOR_RAM_BYTES))
        })
        .collect();
    let staged_for = |m: &graph::ModelGraphV1, r: &graph::GraphReport, cap: u64| {
        let bounds = graph::stages::partition(m, r, batch, cap).ok()?;
        let st = graph::stages::stages(m, r, &bounds).ok()?;
        let largest_stage = st
            .iter()
            .map(|s| graph::stages::stage_memory(m, r, s, batch))
            .max()?;
        Some(PlannedStages {
            stages: st.len(),
            stage_bounds: bounds,
            stage_memory_bytes: cap,
            largest_stage_bytes: largest_stage,
            fitting_workers: stage_capable
                .iter()
                .filter(|b| **b >= largest_stage)
                .count(),
            tile_max_param_bytes: None,
            tiled: Vec::new(),
        })
    };
    let staged = match (&recommendation, stage_capable.iter().copied().max()) {
        (None, Some(cap)) => staged_for(&model, &report, cap).or_else(|| {
            // A single matrix does not fit (D3): tiles, then stages. A stage counts each parameter
            // several times (value, gradient, working copies): a tile of a quarter of a machine
            // may fit nowhere. Tiles therefore shrink until the split fits, down to at most 1/64
            // of the machine.
            [4u64, 8, 16, 32, 64].into_iter().find_map(|div| {
                let max = (cap / div).max(1);
                let (tiled, done) = graph::tiling::auto_tile(&model, max).ok()?;
                let r = graph::validate(&tiled).ok()?;
                let mut p = staged_for(&tiled, &r, cap)?;
                p.tile_max_param_bytes = Some(max);
                p.tiled = done.iter().map(|t| (t.param.clone(), t.tiles)).collect();
                Some(p)
            })
        }),
        _ => None,
    };
    let advice = match (&recommendation, largest) {
        (None, _) if staged.as_ref().is_some_and(|s| !s.tiled.is_empty()) => format!(
            "no machine holds the whole model, nor even its largest matrix; with {} \
matrix(es) tiled, it fits in {} stages spread across workers",
            staged.as_ref().map_or(0, |s| s.tiled.len()),
            staged.as_ref().map_or(0, |s| s.stages)
        ),
        (None, _) if staged.is_some() => format!(
            "no machine holds the whole model; it fits in {} stages spread across \
workers (graph.stage.v1)",
            staged.as_ref().map_or(0, |s| s.stages)
        ),
        (_, None) if caps.is_empty() => {
            "no worker active in the last five minutes: nothing tells where the task will fit"
                .to_owned()
        }
        (_, None) if caps.iter().all(|w| (w.profile as u32) < profile) => format!(
            "no active worker understands gg-ops/{profile}, which this description requires"
        ),
        (_, None) => format!(
            "a task of {batch} examples reserves {task_ram} bytes of host memory and no active \
worker announces that much: increase tasks_per_round to shrink the micro-batch"
        ),
        (Some(r), _) if r.segments == 1 && !r.offload_parameters => {
            "the task fits as is".to_owned()
        }
        (Some(r), _) => format!(
            "the task fits with {} activation recomputation segment(s){}",
            r.segments,
            if r.offload_parameters {
                " and parameter offloading"
            } else {
                ""
            }
        ),
        (None, Some(cap)) => format!(
            "no memory lever makes a task of {batch} examples fit in {cap} bytes: \
increase tasks_per_round to shrink the micro-batch, or split the model into stages (ADR-0024)"
        ),
    };
    Ok(Json(PlanView {
        required_profile: format!("gg-ops/{profile}"),
        parameters: report.parameters,
        trainable_parameters: report.trainable_parameters,
        micro_batch: batch,
        plain_task_device_bytes: plain,
        workers_seen: caps.len(),
        workers_with_profile: caps.iter().filter(|w| w.profile as u32 >= profile).count(),
        largest_device_bytes: largest,
        task_ram_bytes: task_ram,
        workers_with_ram,
        recommendation,
        staged,
        advice,
    }))
}

#[utoipa::path(get, path = "/researcher/me", tag = "researcher", responses((status = 200, body = Me)))]
async fn me(
    State(state): State<AppState>,
    Researcher(id): Researcher,
) -> Result<Json<Me>, ApiError> {
    let mut c = state
        .db()
        .pool()
        .acquire()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let r = identities::get_researcher(&mut c, id)
        .await?
        .ok_or(ApiError::Unauthorized)?;
    drop(c);
    let admission = r
        .admission()
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(Me {
        researcher_id: id,
        display_name: r.display_name,
        admission,
        projects: project_views(&state, id).await?,
    }))
}

#[utoipa::path(get, path = "/researcher/projects", tag = "researcher", responses((status = 200, body = Vec<ProjectView>)))]
async fn list_projects(
    State(state): State<AppState>,
    Researcher(id): Researcher,
) -> Result<Json<Vec<ProjectView>>, ApiError> {
    Ok(Json(project_views(&state, id).await?))
}

#[utoipa::path(get, path = "/researcher/projects/{id}", tag = "researcher", responses((status = 200, body = ProjectView)))]
async fn get_project(
    State(state): State<AppState>,
    Researcher(rid): Researcher,
    Path(id): Path<ProjectId>,
) -> Result<Json<ProjectView>, ApiError> {
    let role = require_project_role(&state, rid, id, ProjectRole::Viewer).await?;
    let mut c = state
        .db()
        .pool()
        .acquire()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let p = identities::get_project(&mut c, id)
        .await?
        .ok_or_else(|| ApiError::NotFound("project".into()))?;
    let b = gg_ledger::budget(&mut c, id).await?;
    Ok(Json(ProjectView {
        project_id: id,
        name: p.name,
        state: p.state,
        allowed_kinds: p.allowed_kinds,
        role,
        budget: Budget {
            available: b.available,
            reserved: b.reserved,
            spent: b.spent,
        },
    }))
}

#[utoipa::path(post, path = "/researcher/projects/{id}/jobs", tag = "researcher", request_body = SubmitJobRequest,
    params(("Idempotency-Key" = Option<String>, Header, description = "Optional idempotency key bound to the researcher")),
    responses((status = 200, body = JobPlan)))]
async fn submit(
    State(state): State<AppState>,
    Researcher(rid): Researcher,
    Path(id): Path<ProjectId>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<(axum::http::StatusCode, Json<serde_json::Value>), ApiError> {
    require_project_role(&state, rid, id, ProjectRole::Member).await?;
    let req: SubmitJobRequest =
        serde_json::from_slice(&body).map_err(|e| ApiError::BadRequest(format!("json: {e}")))?;
    let idem_key = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    let actor = format!("researcher:{rid}");
    let request_hash = Sha256Digest::of_bytes(&body);
    if let Some(key) = &idem_key {
        if key.len() > 128 {
            return Err(ApiError::BadRequest("idempotency key too long".into()));
        }
        let mut c = state
            .db()
            .pool()
            .acquire()
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?;
        if let Some(rec) = misc::get_idempotency(&mut c, &actor, "job.submit", key).await? {
            if rec.request_hash != request_hash.as_bytes() {
                return Err(ApiError::Conflict(
                    "idempotency key reused with a different request".into(),
                ));
            }
            let status = axum::http::StatusCode::from_u16(rec.status as u16)
                .unwrap_or(axum::http::StatusCode::OK);
            return Ok((status, Json(rec.response)));
        }
    }
    let plan = submit_job(&state, rid, id, req).await?;
    let value = serde_json::to_value(&plan).map_err(|e| ApiError::Internal(e.to_string()))?;
    if let (Some(key), false) = (&idem_key, plan.dry_run) {
        let mut c = state
            .db()
            .pool()
            .acquire()
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?;
        if let Err(e) = misc::put_idempotency(
            &mut c,
            &actor,
            "job.submit",
            key,
            &request_hash,
            200,
            &value,
        )
        .await
            && !e.is_unique_violation()
        {
            return Err(e.into());
        }
    }
    Ok((axum::http::StatusCode::OK, Json(value)))
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct TaskStatus {
    pub task_id: TaskId,
    pub index: i32,
    pub compute_kind: String,
    pub state: TaskState,
    pub generation: i64,
    pub attempts_made: i32,
    pub max_attempts: i32,
    pub spec_hash: Sha256Digest,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct JobStatus {
    pub job_id: JobId,
    pub project_id: ProjectId,
    pub experiment_id: ExperimentId,
    pub name: String,
    pub state: JobState,
    pub cancelled: bool,
    pub tasks: Vec<TaskStatus>,
}

async fn job_status(
    state: &AppState,
    rid: ResearcherId,
    id: JobId,
    min: ProjectRole,
) -> Result<JobStatus, ApiError> {
    let mut c = state
        .db()
        .pool()
        .acquire()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let j = jobs::get_job(&mut c, id)
        .await?
        .ok_or_else(|| ApiError::NotFound("job".into()))?;
    drop(c);
    require_project_role(state, rid, ProjectId(j.project_id), min)
        .await
        .map_err(|_| ApiError::NotFound("job".into()))?;
    let mut c = state
        .db()
        .pool()
        .acquire()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let rows = jobs::list_tasks(&mut c, id).await?;
    let mut tasks = Vec::with_capacity(rows.len());
    let mut states = Vec::with_capacity(rows.len());
    for t in rows {
        let s = t
            .task_state()
            .map_err(|e| ApiError::Internal(e.to_string()))?;
        let spec_hash = t
            .spec_digest()
            .map_err(|e| ApiError::Internal(e.to_string()))?;
        states.push(s);
        tasks.push(TaskStatus {
            task_id: TaskId(t.id),
            index: t.index_in_job,
            compute_kind: t.compute_kind,
            state: s,
            generation: t.generation,
            attempts_made: t.attempts_made,
            max_attempts: t.max_attempts,
            spec_hash,
        });
    }
    Ok(JobStatus {
        job_id: id,
        project_id: ProjectId(j.project_id),
        experiment_id: ExperimentId(j.experiment_id),
        name: j.name,
        state: JobState::derive(&states, j.cancelled),
        cancelled: j.cancelled,
        tasks,
    })
}

#[utoipa::path(get, path = "/researcher/jobs/{id}", tag = "researcher", responses((status = 200, body = JobStatus)))]
async fn get_job(
    State(state): State<AppState>,
    Researcher(rid): Researcher,
    Path(id): Path<JobId>,
) -> Result<Json<JobStatus>, ApiError> {
    Ok(Json(
        job_status(&state, rid, id, ProjectRole::Viewer).await?,
    ))
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct Cancelled {
    pub tasks_cancelled: usize,
}

#[utoipa::path(post, path = "/researcher/jobs/{id}/cancel", tag = "researcher", responses((status = 200, body = Cancelled)))]
async fn cancel_job(
    State(state): State<AppState>,
    Researcher(rid): Researcher,
    Path(id): Path<JobId>,
) -> Result<Json<Cancelled>, ApiError> {
    job_status(&state, rid, id, ProjectRole::Member).await?;
    let n = cancel::cancel_job(
        &state,
        &format!("researcher:{rid}"),
        Authority::Researcher,
        id,
    )
    .await?;
    Ok(Json(Cancelled { tasks_cancelled: n }))
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct AttemptInfo {
    pub attempt_id: AttemptId,
    pub worker_id: WorkerId,
    pub generation: i64,
    pub role: AttemptRole,
    pub state: AttemptState,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct TaskDetail {
    pub task: TaskStatus,
    pub job_id: JobId,
    pub project_id: ProjectId,
    #[schema(value_type = Object)]
    pub spec: serde_json::Value,
    pub attempts: Vec<AttemptInfo>,
    /// Outputs of the accepted attempt (empty unless accepted).
    pub accepted_outputs: Vec<NamedArtifact>,
    /// Declared execution profile of the accepted attempt.
    #[schema(value_type = Object)]
    pub accepted_profile: Option<serde_json::Value>,
    /// Signed decision of the accepted attempt.
    #[schema(value_type = Object)]
    pub decision: Option<serde_json::Value>,
    /// Every signed decision recorded on the last submission, with the validator and its operator
    /// (ADR-0014). This is the audit trail of a federated verification, including a disagreement.
    #[schema(value_type = Object)]
    pub verifications: serde_json::Value,
    pub receipt_id: Option<ReceiptId>,
}

#[utoipa::path(get, path = "/researcher/tasks/{id}", tag = "researcher", responses((status = 200, body = TaskDetail)))]
async fn get_task(
    State(state): State<AppState>,
    Researcher(rid): Researcher,
    Path(id): Path<TaskId>,
) -> Result<Json<TaskDetail>, ApiError> {
    let mut c = state
        .db()
        .pool()
        .acquire()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let t = jobs::get_task(&mut c, id)
        .await?
        .ok_or_else(|| ApiError::NotFound("task".into()))?;
    drop(c);
    require_project_role(&state, rid, ProjectId(t.project_id), ProjectRole::Viewer)
        .await
        .map_err(|_| ApiError::NotFound("task".into()))?;
    let mut c = state
        .db()
        .pool()
        .acquire()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let spec = decode_spec(&t.spec_cbor)?;
    let s = t
        .task_state()
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let mut attempts = Vec::new();
    let mut verifications = serde_json::Value::Array(Vec::new());
    let mut accepted_outputs = Vec::new();
    let mut accepted_profile = None;
    let mut decision = None;
    let mut receipt_id = None;
    for a in tasks::list_attempts(&mut c, id).await? {
        let a_state = a
            .attempt_state()
            .map_err(|e| ApiError::Internal(e.to_string()))?;
        let role = a
            .attempt_role()
            .map_err(|e| ApiError::Internal(e.to_string()))?;
        if a_state == AttemptState::Accepted {
            if let Some(sub) = tasks::get_submission_by_attempt(&mut c, AttemptId(a.id)).await? {
                let signed: Signed<ResultSubmissionV1> =
                    Signed::from_canonical_bytes(&sub.submission_cbor)
                        .map_err(|e| ApiError::Internal(e.to_string()))?;
                accepted_outputs = signed
                    .body
                    .outputs
                    .iter()
                    .map(|(n, r)| NamedArtifact::from_ref(n, r))
                    .collect();
                accepted_profile = serde_json::to_value(&signed.body.execution_profile).ok();
            }
            if let Some(v) = tasks::get_verification_by_attempt(&mut c, AttemptId(a.id)).await?
                && let Some(d) = tasks::get_decision_by_verification(&mut c, v.id).await?
            {
                let signed: Signed<VerificationDecisionV1> =
                    Signed::from_canonical_bytes(&d.decision_cbor)
                        .map_err(|e| ApiError::Internal(e.to_string()))?;
                decision = serde_json::to_value(&signed).ok();
            }
            receipt_id = tasks::get_receipt(&mut c, AttemptId(a.id), role)
                .await?
                .map(|r| ReceiptId(r.id));
        }
        if let Some(sub) = tasks::get_submission_by_attempt(&mut c, AttemptId(a.id)).await? {
            let opinions =
                crate::workflows::federated::opinions(&mut c, SubmissionId(sub.id)).await?;
            if !opinions.is_empty() {
                verifications = serde_json::to_value(&opinions)
                    .map_err(|e| ApiError::Internal(e.to_string()))?;
            }
        }
        attempts.push(AttemptInfo {
            attempt_id: AttemptId(a.id),
            worker_id: WorkerId(a.worker_id),
            generation: a.generation,
            role,
            state: a_state,
        });
    }
    Ok(Json(TaskDetail {
        task: TaskStatus {
            task_id: id,
            index: t.index_in_job,
            compute_kind: t.compute_kind.clone(),
            state: s,
            generation: t.generation,
            attempts_made: t.attempts_made,
            max_attempts: t.max_attempts,
            spec_hash: t
                .spec_digest()
                .map_err(|e| ApiError::Internal(e.to_string()))?,
        },
        job_id: JobId(t.job_id),
        project_id: ProjectId(t.project_id),
        spec: serde_json::to_value(&spec).map_err(|e| ApiError::Internal(e.to_string()))?,
        attempts,
        accepted_outputs,
        accepted_profile,
        decision,
        verifications,
        receipt_id,
    }))
}

/// Exportable receipt with signature and canonical bytes for independent verification.
#[derive(Serialize, Deserialize, utoipa::ToSchema)]
pub struct ReceiptExport {
    pub receipt_id: ReceiptId,
    pub attempt_id: AttemptId,
    pub worker_id: WorkerId,
    pub project_id: ProjectId,
    pub role: String,
    pub amount: Credits,
    pub reward_state: String,
    pub receipt_hash: Sha256Digest,
    /// Signed receipt (JSON form).
    #[schema(value_type = Object)]
    pub receipt: Signed<ContributionReceiptV1>,
    /// Canonical DAG-CBOR bytes of the signed envelope (hex).
    pub canonical_hex: String,
}

pub fn export_receipt(row: gg_storage::models::ReceiptRow) -> Result<ReceiptExport, ApiError> {
    let receipt: Signed<ContributionReceiptV1> = Signed::from_canonical_bytes(&row.receipt_cbor)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let hash: [u8; 32] = row
        .receipt_hash
        .as_slice()
        .try_into()
        .map_err(|_| ApiError::Internal("bad receipt hash".into()))?;
    Ok(ReceiptExport {
        receipt_id: ReceiptId(row.id),
        attempt_id: AttemptId(row.attempt_id),
        worker_id: WorkerId(row.worker_id),
        project_id: ProjectId(row.project_id),
        role: row.role,
        amount: Credits(row.amount.max(0) as u64),
        reward_state: row.reward_state,
        receipt_hash: Sha256Digest(hash),
        receipt,
        canonical_hex: hex::encode(&row.receipt_cbor),
    })
}

#[utoipa::path(get, path = "/researcher/projects/{id}/receipts", tag = "researcher", responses((status = 200, body = Vec<ReceiptExport>)))]
async fn project_receipts(
    State(state): State<AppState>,
    Researcher(rid): Researcher,
    Path(id): Path<ProjectId>,
) -> Result<Json<Vec<ReceiptExport>>, ApiError> {
    require_project_role(&state, rid, id, ProjectRole::Viewer).await?;
    let mut c = state
        .db()
        .pool()
        .acquire()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let rows = tasks::list_receipts_of_project(&mut c, id, 500).await?;
    rows.into_iter()
        .map(export_receipt)
        .collect::<Result<Vec<_>, _>>()
        .map(Json)
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct LedgerEntryView {
    pub txn_id: uuid::Uuid,
    pub kind: String,
    pub account: String,
    pub debit: i64,
    pub credit: i64,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct LedgerView {
    pub budget: Budget,
    pub entries: Vec<LedgerEntryView>,
}

#[utoipa::path(get, path = "/researcher/projects/{id}/ledger", tag = "researcher", responses((status = 200, body = LedgerView)))]
async fn project_ledger(
    State(state): State<AppState>,
    Researcher(rid): Researcher,
    Path(id): Path<ProjectId>,
) -> Result<Json<LedgerView>, ApiError> {
    require_project_role(&state, rid, id, ProjectRole::Viewer).await?;
    let mut c = state
        .db()
        .pool()
        .acquire()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let b = gg_ledger::budget(&mut c, id).await?;
    let mut entries = Vec::new();
    for account in [
        gg_ledger::accounts::available(id),
        gg_ledger::accounts::reserved(id),
    ] {
        for (txn_id, kind, debit, credit) in lq::list_entries(&mut c, &account, 200).await? {
            entries.push(LedgerEntryView {
                txn_id,
                kind,
                account: account.clone(),
                debit,
                credit,
            });
        }
    }
    Ok(Json(LedgerView {
        budget: Budget {
            available: b.available,
            reserved: b.reserved,
            spent: b.spent,
        },
        entries,
    }))
}

/// Upload query parameters.
#[derive(Deserialize, utoipa::IntoParams)]
pub struct UploadQuery {
    /// Announced size in bytes.
    pub size: u64,
    /// Schema label (e.g. `ggt1-f32`).
    #[serde(default)]
    pub schema: String,
    /// Declared license.
    #[serde(default)]
    pub license: String,
}

/// Artifact metadata.
#[derive(Serialize, utoipa::ToSchema)]
pub struct ArtifactInfo {
    pub hash: Sha256Digest,
    pub size: u64,
    pub media_type: String,
    pub schema: String,
    pub visibility: String,
}

/// Store an upload body under `hash` for `project`.
#[allow(clippy::too_many_arguments)]
pub async fn store_upload(
    state: &AppState,
    project: ProjectId,
    hash: Sha256Digest,
    size: u64,
    schema: &str,
    license: &str,
    uploaded_by: &str,
    body: Body,
) -> Result<ArtifactInfo, ApiError> {
    if size > state.config().max_upload_bytes {
        return Err(ApiError::TooLarge);
    }
    {
        let mut c = state.db().pool().acquire().await?;
        if gg_storage::queries::artifacts::is_withdrawn(&mut c, &hash).await? {
            return Err(ApiError::Conflict(
                "this artifact was withdrawn by an operator and cannot be stored again".into(),
            ));
        }
    }
    use futures_util::TryStreamExt;
    let stream = body.into_data_stream().map_err(std::io::Error::other);
    let reader = tokio_util::io::StreamReader::new(stream);
    let stored = state
        .objects()
        .put_stream(reader, Some((hash, size)), state.config().max_upload_bytes)
        .await?;
    let mut c = state
        .db()
        .pool()
        .acquire()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    gg_storage::queries::artifacts::insert_artifact(
        &mut c,
        &stored.hash,
        stored.size,
        "application/octet-stream",
        schema,
        "project",
        license,
        Some(project),
        uploaded_by,
    )
    .await?;
    Ok(ArtifactInfo {
        hash: stored.hash,
        size: stored.size,
        media_type: "application/octet-stream".into(),
        schema: schema.to_owned(),
        visibility: "project".into(),
    })
}

/// Parsed single byte range.
fn parse_range(header: Option<&str>, size: u64) -> Result<Option<(u64, u64)>, ApiError> {
    let Some(h) = header else { return Ok(None) };
    let spec = h
        .strip_prefix("bytes=")
        .ok_or(ApiError::BadRequest("unsupported range unit".into()))?;
    if spec.contains(',') {
        return Err(ApiError::BadRequest(
            "multiple ranges are not supported".into(),
        ));
    }
    let (start, end) = spec
        .split_once('-')
        .ok_or(ApiError::BadRequest("malformed range".into()))?;
    let range = if start.is_empty() {
        // suffix range: last N bytes
        let n: u64 = end
            .parse()
            .map_err(|_| ApiError::BadRequest("malformed range".into()))?;
        if n == 0 || size == 0 {
            return Err(ApiError::Unprocessable("range not satisfiable".into()));
        }
        (size.saturating_sub(n), size - 1)
    } else {
        let s: u64 = start
            .parse()
            .map_err(|_| ApiError::BadRequest("malformed range".into()))?;
        let e: u64 = if end.is_empty() {
            size.saturating_sub(1)
        } else {
            end.parse()
                .map_err(|_| ApiError::BadRequest("malformed range".into()))?
        };
        if s >= size || e < s {
            return Err(ApiError::Unprocessable("range not satisfiable".into()));
        }
        (s, e.min(size - 1))
    };
    Ok(Some(range))
}

/// Stream a stored object, honouring a single `Range: bytes=…` (resumable downloads).
pub async fn stream_object_ranged(
    state: &AppState,
    hash: &Sha256Digest,
    range_header: Option<&str>,
) -> Result<Response, ApiError> {
    use tokio::io::AsyncSeekExt;
    {
        let mut c = state.db().pool().acquire().await?;
        if gg_storage::queries::artifacts::is_withdrawn(&mut c, hash).await? {
            return Err(ApiError::NotFound(
                "artifact withdrawn by an operator".into(),
            ));
        }
        if gg_storage::queries::artifacts::is_expired(&mut c, hash).await? {
            return Err(ApiError::NotFound(
                "artifact expired: an intermediate artifact of a training run, removed by the \
                 retention policy (ADR-0040)"
                    .into(),
            ));
        }
    }
    let size = state.objects().size(hash).await?;
    let mut file = state.objects().open_read(hash).await?;
    let range = match parse_range(range_header, size) {
        Ok(r) => r,
        Err(ApiError::Unprocessable(_)) => {
            return Response::builder()
                .status(416)
                .header("content-range", format!("bytes */{size}"))
                .header("accept-ranges", "bytes")
                .body(Body::empty())
                .map_err(|e| ApiError::Internal(e.to_string()));
        }
        Err(e) => return Err(e),
    };
    let (status, start, len) = match range {
        Some((s, e)) => {
            file.seek(std::io::SeekFrom::Start(s))
                .await
                .map_err(|e| ApiError::Internal(e.to_string()))?;
            (206, s, e - s + 1)
        }
        None => (200, 0, size),
    };
    let limited = tokio::io::AsyncReadExt::take(file, len);
    let stream = tokio_util::io::ReaderStream::with_capacity(limited, 256 * 1024);
    let body = Body::from_stream(stream);
    let mut b = Response::builder()
        .status(status)
        .header("content-type", "application/octet-stream")
        .header("content-length", len)
        .header("accept-ranges", "bytes")
        .header("etag", format!("\"{}\"", hash.to_hex()));
    if status == 206 {
        b = b.header(
            "content-range",
            format!("bytes {start}-{}/{size}", start + len - 1),
        );
    }
    b.body(body).map_err(|e| ApiError::Internal(e.to_string()))
}

/// Stream a stored object (no range).
pub async fn stream_object(state: &AppState, hash: &Sha256Digest) -> Result<Response, ApiError> {
    stream_object_ranged(state, hash, None).await
}

fn range_of(headers: &HeaderMap) -> Option<String> {
    headers
        .get("range")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_owned())
}

async fn upload_artifact(
    State(state): State<AppState>,
    Researcher(rid): Researcher,
    Path((id, hash)): Path<(ProjectId, Sha256Digest)>,
    Query(q): Query<UploadQuery>,
    body: Body,
) -> Result<Json<ArtifactInfo>, ApiError> {
    require_project_role(&state, rid, id, ProjectRole::Member).await?;
    if q.schema.len() > 64 || q.license.len() > 64 {
        return Err(ApiError::Unprocessable(
            "schema/license labels too long".into(),
        ));
    }
    Ok(Json(
        store_upload(
            &state,
            id,
            hash,
            q.size,
            &q.schema,
            &q.license,
            &format!("researcher:{rid}"),
            body,
        )
        .await?,
    ))
}

async fn download_artifact(
    State(state): State<AppState>,
    Researcher(rid): Researcher,
    Path((id, hash)): Path<(ProjectId, Sha256Digest)>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    require_project_role(&state, rid, id, ProjectRole::Viewer).await?;
    let mut c = state
        .db()
        .pool()
        .acquire()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    if !gg_storage::queries::artifacts::artifact_readable_by_project(&mut c, &hash, id).await? {
        return Err(ApiError::NotFound("artifact".into()));
    }
    drop(c);
    stream_object_ranged(&state, &hash, range_of(&headers).as_deref()).await
}

use crate::workflows::collective::{
    CreateCollectiveRequest, RunView, cancel_run, create_run, run_project, run_view,
};

#[utoipa::path(post, path = "/researcher/projects/{id}/collective", tag = "researcher", request_body = CreateCollectiveRequest, responses((status = 200, body = RunView)))]
async fn collective_create(
    State(state): State<AppState>,
    Researcher(rid): Researcher,
    Path(id): Path<ProjectId>,
    Json(req): Json<CreateCollectiveRequest>,
) -> Result<Json<RunView>, ApiError> {
    require_project_role(&state, rid, id, ProjectRole::Member).await?;
    Ok(Json(create_run(&state, rid, id, req).await?))
}

#[utoipa::path(get, path = "/researcher/collective/{run}", tag = "researcher", responses((status = 200, body = RunView)))]
async fn collective_get(
    State(state): State<AppState>,
    Researcher(rid): Researcher,
    Path(run): Path<uuid::Uuid>,
) -> Result<Json<RunView>, ApiError> {
    let project = run_project(&state, run).await?;
    require_project_role(&state, rid, project, ProjectRole::Viewer)
        .await
        .map_err(|_| ApiError::NotFound("collective run".into()))?;
    Ok(Json(run_view(&state, run).await?))
}

#[utoipa::path(post, path = "/researcher/collective/{run}/cancel", tag = "researcher", responses((status = 204)))]
async fn collective_cancel(
    State(state): State<AppState>,
    Researcher(rid): Researcher,
    Path(run): Path<uuid::Uuid>,
) -> Result<axum::http::StatusCode, ApiError> {
    let project = run_project(&state, run).await?;
    require_project_role(&state, rid, project, ProjectRole::Member)
        .await
        .map_err(|_| ApiError::NotFound("collective run".into()))?;
    cancel_run(&state, &format!("researcher:{rid}"), run).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// JSON routes (documented).
pub fn json_routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(collective_create))
        .routes(routes!(collective_get))
        .routes(routes!(collective_cancel))
        .routes(routes!(me))
        .routes(routes!(grid_capacity))
        .routes(routes!(plan_route))
        .routes(routes!(list_projects))
        .routes(routes!(get_project))
        .routes(routes!(submit))
        .routes(routes!(get_job))
        .routes(routes!(cancel_job))
        .routes(routes!(get_task))
        .routes(routes!(project_receipts))
        .routes(routes!(project_ledger))
}

/// Streaming routes (upload/download; documented in gg-core/docs/PROTOCOL.md and README).
pub fn upload_routes() -> Router<AppState> {
    Router::new().route(
        "/researcher/projects/{id}/artifacts/{hash}",
        put(upload_artifact).get(download_artifact),
    )
}
