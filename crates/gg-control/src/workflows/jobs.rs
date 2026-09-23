//! Job submission: catalog validation, pricing, input checks, task creation.

use gg_compute::catalog;
use gg_core::*;
use gg_protocol::canonical;
use gg_protocol::domain::{Domain, hash_message};
use gg_protocol::messages::{TaskDefinitionV1, TaskSpecV1};
use gg_storage::queries::{artifacts, identities, jobs, misc};
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::state::AppState;

/// Request body of a job submission.
#[derive(Clone, Debug, Deserialize, utoipa::ToSchema)]
pub struct SubmitJobRequest {
    /// Job name.
    #[serde(default)]
    pub name: String,
    /// Existing experiment (must belong to the project) or none to create one.
    #[serde(default)]
    pub experiment_id: Option<ExperimentId>,
    /// Name of the experiment to create when `experiment_id` is absent.
    #[serde(default)]
    pub experiment_name: Option<String>,
    /// Scheduling priority (higher first).
    #[serde(default)]
    pub priority: i32,
    /// Task definitions (1..=256).
    #[schema(value_type = Vec<Object>)]
    pub tasks: Vec<TaskDefinitionV1>,
    /// Plan only: nothing is persisted.
    #[serde(default)]
    pub dry_run: bool,
}

/// Planned task.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
pub struct PlannedTask {
    /// Task id (absent for dry runs).
    pub task_id: Option<TaskId>,
    /// Index.
    pub index: u32,
    /// Compute kind.
    pub compute_kind: String,
    /// Definition hash.
    pub definition_hash: Sha256Digest,
    /// Spec hash (absent for dry runs).
    pub spec_hash: Option<Sha256Digest>,
    /// Reward for a primary attempt.
    pub reward_primary: Credits,
    /// Verification cost.
    pub verification_cost: Credits,
    /// Credits reserved per attempt.
    pub reservation_per_attempt: Credits,
    /// Resource estimate.
    #[schema(value_type = Object)]
    pub estimate: catalog::ResourceEstimate,
}

/// Job submission result / plan.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
pub struct JobPlan {
    /// Job id (absent for dry runs).
    pub job_id: Option<JobId>,
    /// Experiment id (absent for dry runs creating a new experiment).
    pub experiment_id: Option<ExperimentId>,
    /// Tasks.
    pub tasks: Vec<PlannedTask>,
    /// Sum of per-attempt reservations.
    pub total_reservation: Credits,
    /// Project budget available at submission time.
    pub budget_available: Credits,
    /// Whether this was a dry run.
    pub dry_run: bool,
}

struct Validated {
    def: TaskDefinitionV1,
    checked: catalog::ValidatedTask,
    definition_hash: Sha256Digest,
}

async fn validate_definition(
    c: &mut gg_storage::PgConnection,
    state: &AppState,
    project: ProjectId,
    allowed_kinds: &[String],
    def: &TaskDefinitionV1,
    index: usize,
) -> Result<Validated, ApiError> {
    let checked = catalog::validate_definition(def)
        .map_err(|e| ApiError::Unprocessable(format!("task {index}: {e}")))?;
    let kind = def.params.compute_kind();
    if !allowed_kinds.iter().any(|k| k == kind) {
        return Err(ApiError::Forbidden(format!(
            "task {index}: project may not submit {kind}"
        )));
    }
    let expected_price =
        catalog::price(&def.params).map_err(|e| ApiError::Unprocessable(e.to_string()))?;
    if def.reward_schedule != expected_price {
        return Err(ApiError::Unprocessable(format!(
            "task {index}: reward_schedule must equal the catalog schedule {expected_price:?}"
        )));
    }
    for (name, r) in &def.inputs {
        let row = artifacts::get_artifact(c, &r.hash).await?.ok_or_else(|| {
            ApiError::Unprocessable(format!(
                "task {index}: input '{name}' is not a known artifact"
            ))
        })?;
        if row.size as u64 != r.size {
            return Err(ApiError::Unprocessable(format!(
                "task {index}: input '{name}' size differs from the stored artifact"
            )));
        }
        if !artifacts::artifact_readable_by_project(c, &r.hash, project).await? {
            return Err(ApiError::Forbidden(format!(
                "task {index}: input '{name}' is not readable by this project"
            )));
        }
        if !state.objects().exists(&r.hash) {
            return Err(ApiError::Internal(format!(
                "artifact {} missing from object store",
                r.hash
            )));
        }
    }
    let definition_hash = hash_message(state.network(), Domain::TaskDefinition, def)?;
    Ok(Validated {
        def: def.clone(),
        checked,
        definition_hash,
    })
}

/// Validate and (unless dry run) persist a job.
pub async fn submit_job(
    state: &AppState,
    researcher: ResearcherId,
    project: ProjectId,
    req: SubmitJobRequest,
) -> Result<JobPlan, ApiError> {
    if req.tasks.is_empty() || req.tasks.len() > 256 {
        return Err(ApiError::Unprocessable(
            "tasks must contain 1..=256 definitions".into(),
        ));
    }
    if req.name.len() > 128 || req.experiment_name.as_ref().is_some_and(|n| n.len() > 128) {
        return Err(ApiError::Unprocessable("names must be <= 128 bytes".into()));
    }
    let mut tx = state.db().begin().await?;
    let proj = identities::get_project(&mut tx, project)
        .await?
        .ok_or_else(|| ApiError::NotFound("project".into()))?;
    if proj
        .project_state()
        .map_err(|e| ApiError::Internal(e.to_string()))?
        != ProjectState::Active
    {
        return Err(ApiError::Forbidden("project is not active".into()));
    }
    let budget = gg_ledger::budget(&mut tx, project).await?;
    let mut validated = Vec::with_capacity(req.tasks.len());
    for (i, def) in req.tasks.iter().enumerate() {
        validated
            .push(validate_definition(&mut tx, state, project, &proj.allowed_kinds, def, i).await?);
    }
    let mut total = Credits::ZERO;
    for v in &validated {
        total = total
            .checked_add(
                v.def
                    .reward_schedule
                    .reservation_for(AttemptRole::Primary)
                    .map_err(|e| ApiError::Unprocessable(e.to_string()))?,
            )
            .map_err(|e| ApiError::Unprocessable(e.to_string()))?;
    }

    if req.dry_run {
        let tasks = validated
            .iter()
            .enumerate()
            .map(|(i, v)| PlannedTask {
                task_id: None,
                index: i as u32,
                compute_kind: v.def.params.compute_kind().to_owned(),
                definition_hash: v.definition_hash,
                spec_hash: None,
                reward_primary: v.def.reward_schedule.primary,
                verification_cost: v.def.reward_schedule.verification_cost,
                reservation_per_attempt: v
                    .def
                    .reward_schedule
                    .reservation_for(AttemptRole::Primary)
                    .unwrap_or(Credits::ZERO),
                estimate: v.checked.estimate.clone(),
            })
            .collect();
        tx.rollback().await?;
        return Ok(JobPlan {
            job_id: None,
            experiment_id: req.experiment_id,
            tasks,
            total_reservation: total,
            budget_available: budget.available,
            dry_run: true,
        });
    }

    let experiment_id = match req.experiment_id {
        Some(e) => {
            let row = jobs::get_experiment(&mut tx, e)
                .await?
                .ok_or_else(|| ApiError::NotFound("experiment".into()))?;
            if row.project_id != project.0 {
                return Err(ApiError::NotFound("experiment".into()));
            }
            e
        }
        None => {
            let e = ExperimentId::new();
            let name = req
                .experiment_name
                .clone()
                .unwrap_or_else(|| req.name.clone());
            jobs::create_experiment(&mut tx, e, project, &name, Some(researcher)).await?;
            e
        }
    };
    let job_id = JobId::new();
    jobs::create_job(
        &mut tx,
        job_id,
        project,
        experiment_id,
        Some(researcher),
        &req.name,
    )
    .await?;
    let mut tasks = Vec::with_capacity(validated.len());
    for (i, v) in validated.into_iter().enumerate() {
        let task_id = TaskId::new();
        let spec = TaskSpecV1 {
            spec_version: 1,
            network: state.network().clone(),
            project_id: project,
            experiment_id,
            job_id,
            task_id,
            index: i as u32,
            definition: v.def.clone(),
        };
        let spec_cbor = canonical::to_bytes(&spec)?;
        let spec_hash =
            gg_protocol::domain::hash_bytes(state.network(), Domain::TaskSpec, &spec_cbor);
        jobs::insert_task(
            &mut tx,
            &jobs::NewTask {
                id: task_id,
                job_id,
                project_id: project,
                experiment_id,
                index: i as i32,
                compute_kind: v.def.params.compute_kind(),
                spec_hash,
                definition_hash: v.definition_hash,
                spec_cbor: &spec_cbor,
                max_attempts: v.def.max_attempts as i32,
                priority: req.priority,
                reward_primary: v.def.reward_schedule.primary,
                verification_cost: v.def.reward_schedule.verification_cost,
                est_cpu_secs: v.checked.estimate.est_cpu_secs,
                requirements: jobs::TaskRequirements {
                    ram_bytes: v.def.resource_limits.max_ram_bytes as i64,
                    ..Default::default()
                },
            },
        )
        .await?;
        tasks.push(PlannedTask {
            task_id: Some(task_id),
            index: i as u32,
            compute_kind: v.def.params.compute_kind().to_owned(),
            definition_hash: v.definition_hash,
            spec_hash: Some(spec_hash),
            reward_primary: v.def.reward_schedule.primary,
            verification_cost: v.def.reward_schedule.verification_cost,
            reservation_per_attempt: v
                .def
                .reward_schedule
                .reservation_for(AttemptRole::Primary)
                .unwrap_or(Credits::ZERO),
            estimate: v.checked.estimate,
        });
    }
    misc::audit(
        &mut tx,
        &format!("researcher:{researcher}"),
        "job.submit",
        &job_id.to_string(),
        serde_json::json!({ "project_id": project, "tasks": tasks.len() }),
    )
    .await?;
    misc::outbox_insert(
        &mut tx,
        "job.submitted",
        &serde_json::json!({ "job_id": job_id, "project_id": project, "tasks": tasks.len() }),
    )
    .await?;
    tx.commit().await?;
    state.signal_work();
    Ok(JobPlan {
        job_id: Some(job_id),
        experiment_id: Some(experiment_id),
        tasks,
        total_reservation: total,
        budget_available: budget.available,
        dry_run: false,
    })
}
