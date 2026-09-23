//! Operator routes.

use axum::Json;
use axum::extract::{Path, Query, State};
use gg_core::*;
use gg_protocol::keys::PublicKey;
use gg_storage::queries::{federated, identities, jobs, misc, tasks};
use serde::{Deserialize, Serialize};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::auth::{API_TOKEN_PREFIX, Admin, new_token, token_hash};
use crate::error::ApiError;
use crate::state::AppState;

#[derive(Deserialize, utoipa::ToSchema)]
pub struct CreateResearcher {
    pub display_name: String,
    #[serde(default = "default_admission")]
    pub admission: AdmissionState,
}
fn default_admission() -> AdmissionState {
    AdmissionState::Pending
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct ResearcherCreated {
    pub researcher_id: ResearcherId,
    /// API token (shown once).
    pub token: String,
    pub admission: AdmissionState,
}

#[utoipa::path(post, path = "/admin/researchers", tag = "admin", request_body = CreateResearcher, responses((status = 200, body = ResearcherCreated)))]
async fn create_researcher(
    State(state): State<AppState>,
    _a: Admin,
    Json(req): Json<CreateResearcher>,
) -> Result<Json<ResearcherCreated>, ApiError> {
    if req.display_name.is_empty() || req.display_name.len() > 128 {
        return Err(ApiError::Unprocessable(
            "display_name must be 1..=128 bytes".into(),
        ));
    }
    let id = ResearcherId::new();
    let token = new_token(API_TOKEN_PREFIX)?;
    let mut tx = state.db().begin().await?;
    identities::create_researcher(&mut tx, id, &req.display_name, req.admission).await?;
    identities::create_token(
        &mut tx,
        &token_hash(&token),
        "researcher",
        Some(id),
        "admin-created",
        None,
    )
    .await?;
    misc::audit(
        &mut tx,
        "admin",
        "researcher.create",
        &id.to_string(),
        serde_json::json!({ "admission": req.admission }),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(ResearcherCreated {
        researcher_id: id,
        token,
        admission: req.admission,
    }))
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct SetAdmission {
    pub state: AdmissionState,
    #[serde(default)]
    pub note: String,
}

#[utoipa::path(post, path = "/admin/researchers/{id}/admission", tag = "admin", request_body = SetAdmission, responses((status = 204)))]
async fn set_admission(
    State(state): State<AppState>,
    _a: Admin,
    Path(id): Path<ResearcherId>,
    Json(req): Json<SetAdmission>,
) -> Result<axum::http::StatusCode, ApiError> {
    let mut tx = state.db().begin().await?;
    if !identities::set_admission(&mut tx, id, req.state, &req.note).await? {
        return Err(ApiError::NotFound("researcher".into()));
    }
    misc::audit(
        &mut tx,
        "admin",
        "researcher.admission",
        &id.to_string(),
        serde_json::json!({ "state": req.state, "note": req.note }),
    )
    .await?;
    tx.commit().await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct CreateProject {
    pub name: String,
    #[serde(default)]
    pub allowed_kinds: Vec<String>,
    #[serde(default)]
    pub owner_researcher_id: Option<ResearcherId>,
    /// Explicit acceptance of the data rule published by `/v1/info` (docs/DECISIONS.md, D4).
    /// Without it, no project: a submitter must have read what a contributor will be able to see.
    #[serde(default)]
    pub data_policy_ack: bool,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct ProjectCreated {
    pub project_id: ProjectId,
    /// Data rule accepted for this project.
    pub data_policy: String,
}

#[utoipa::path(post, path = "/admin/projects", tag = "admin", request_body = CreateProject, responses((status = 200, body = ProjectCreated)))]
async fn create_project(
    State(state): State<AppState>,
    _a: Admin,
    Json(req): Json<CreateProject>,
) -> Result<Json<ProjectCreated>, ApiError> {
    if req.name.is_empty() || req.name.len() > 128 {
        return Err(ApiError::Unprocessable("name must be 1..=128 bytes".into()));
    }
    for k in &req.allowed_kinds {
        if !gg_compute::catalog::KINDS.contains(&k.as_str()) {
            return Err(ApiError::Unprocessable(format!("unknown compute kind {k}")));
        }
    }
    let policy = state.config().data_policy.clone();
    if !req.data_policy_ack {
        return Err(ApiError::Unprocessable(format!(
            "this deployment requires accepting its data rule ({policy}) before a project exists: {}",
            crate::api::common::data_policy_text(&policy)
        )));
    }
    let id = ProjectId::new();
    let mut tx = state.db().begin().await?;
    identities::create_project(
        &mut tx,
        id,
        &req.name,
        ProjectState::Active,
        &req.allowed_kinds,
    )
    .await?;
    identities::set_project_data_policy(&mut tx, id, &policy).await?;
    if let Some(owner) = req.owner_researcher_id {
        identities::get_researcher(&mut tx, owner)
            .await?
            .ok_or_else(|| ApiError::NotFound("researcher".into()))?;
        identities::upsert_membership(&mut tx, id, owner, ProjectRole::Owner).await?;
    }
    let initial = state.config().project_initial_grant;
    if initial > 0 {
        gg_ledger::grant(
            &mut tx,
            id,
            Credits::new(initial),
            "initial grant (project_initial_grant)",
        )
        .await?;
    }
    misc::audit(
        &mut tx,
        "admin",
        "project.create",
        &id.to_string(),
        serde_json::json!({ "allowed_kinds": req.allowed_kinds, "data_policy": policy, "initial_grant": initial }),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(ProjectCreated {
        project_id: id,
        data_policy: policy,
    }))
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct Grant {
    pub amount: Credits,
    #[serde(default)]
    pub reference: String,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct Budget {
    pub available: Credits,
    pub reserved: Credits,
    pub spent: Credits,
}

#[utoipa::path(post, path = "/admin/projects/{id}/grant", tag = "admin", request_body = Grant, responses((status = 200, body = Budget)))]
async fn grant(
    State(state): State<AppState>,
    _a: Admin,
    Path(id): Path<ProjectId>,
    Json(req): Json<Grant>,
) -> Result<Json<Budget>, ApiError> {
    let mut tx = state.db().begin().await?;
    identities::get_project(&mut tx, id)
        .await?
        .ok_or_else(|| ApiError::NotFound("project".into()))?;
    gg_ledger::grant(&mut tx, id, req.amount, &req.reference).await?;
    misc::audit(
        &mut tx,
        "admin",
        "budget.grant",
        &id.to_string(),
        serde_json::json!({ "amount": req.amount, "reference": req.reference }),
    )
    .await?;
    let b = gg_ledger::budget(&mut tx, id).await?;
    tx.commit().await?;
    Ok(Json(Budget {
        available: b.available,
        reserved: b.reserved,
        spent: b.spent,
    }))
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct AddMember {
    pub researcher_id: ResearcherId,
    pub role: ProjectRole,
}

#[utoipa::path(post, path = "/admin/projects/{id}/members", tag = "admin", request_body = AddMember, responses((status = 204)))]
async fn add_member(
    State(state): State<AppState>,
    _a: Admin,
    Path(id): Path<ProjectId>,
    Json(req): Json<AddMember>,
) -> Result<axum::http::StatusCode, ApiError> {
    let mut tx = state.db().begin().await?;
    identities::get_project(&mut tx, id)
        .await?
        .ok_or_else(|| ApiError::NotFound("project".into()))?;
    identities::get_researcher(&mut tx, req.researcher_id)
        .await?
        .ok_or_else(|| ApiError::NotFound("researcher".into()))?;
    identities::upsert_membership(&mut tx, id, req.researcher_id, req.role).await?;
    misc::audit(
        &mut tx,
        "admin",
        "project.member",
        &id.to_string(),
        serde_json::json!({ "researcher_id": req.researcher_id, "role": req.role }),
    )
    .await?;
    tx.commit().await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct Authorize {
    pub authorized: bool,
}

#[utoipa::path(post, path = "/admin/workers/{id}/authorize", tag = "admin", request_body = Authorize, responses((status = 204)))]
async fn authorize_worker(
    State(state): State<AppState>,
    _a: Admin,
    Path(id): Path<WorkerId>,
    Json(req): Json<Authorize>,
) -> Result<axum::http::StatusCode, ApiError> {
    let mut tx = state.db().begin().await?;
    if !identities::set_worker_authorized(&mut tx, id, req.authorized).await? {
        return Err(ApiError::NotFound("worker".into()));
    }
    if !req.authorized {
        identities::revoke_sessions(&mut tx, "worker", id.0).await?;
        // A deauthorized worker is no longer advertised as a source of bytes.
        gg_storage::queries::p2p::delete_peers_of_worker(&mut tx, id).await?;
    }
    misc::audit(
        &mut tx,
        "admin",
        "worker.authorize",
        &id.to_string(),
        serde_json::json!({ "authorized": req.authorized }),
    )
    .await?;
    tx.commit().await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct ContributorInfo {
    pub contributor_id: uuid::Uuid,
    pub email: String,
    pub created_at: String,
    pub suspended: bool,
    pub suspended_reason: String,
    pub machines: usize,
    pub accepted_tasks: i64,
}

#[utoipa::path(get, path = "/admin/contributors", tag = "admin", responses((status = 200, body = Vec<ContributorInfo>)))]
async fn list_contributors(
    State(state): State<AppState>,
    _a: Admin,
) -> Result<Json<Vec<ContributorInfo>>, ApiError> {
    let mut c = state.db().pool().acquire().await?;
    let mut out = Vec::new();
    for a in gg_storage::queries::contributors::list(&mut c).await? {
        let machines = gg_storage::queries::contributors::machines(&mut c, a.id).await?;
        out.push(ContributorInfo {
            contributor_id: a.id,
            email: a.email,
            created_at: a.created_at.to_string(),
            suspended: a.suspended_at.is_some(),
            suspended_reason: a.suspended_reason,
            accepted_tasks: machines.iter().map(|m| m.accepted).sum(),
            machines: machines.len(),
        });
    }
    Ok(Json(out))
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct SuspendRequest {
    /// true to suspend, false to reinstate.
    pub suspended: bool,
    #[serde(default)]
    pub reason: String,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct Suspended {
    pub suspended: bool,
    /// Machines deauthorized (or authorized again).
    pub machines: usize,
}

/// Suspend a contributor account: every machine stops at once (deauthorized, sessions revoked, no
/// longer advertised to peers), and its enrollment code no longer works. Or reinstate it.
#[utoipa::path(post, path = "/admin/contributors/{id}/suspend", tag = "admin", request_body = SuspendRequest, responses((status = 200, body = Suspended)))]
async fn suspend_contributor(
    State(state): State<AppState>,
    _a: Admin,
    Path(id): Path<uuid::Uuid>,
    Json(req): Json<SuspendRequest>,
) -> Result<Json<Suspended>, ApiError> {
    if req.suspended && req.reason.trim().is_empty() {
        return Err(ApiError::BadRequest(
            "a reason is required to suspend".into(),
        ));
    }
    let machines = crate::workflows::contributors::set_suspended(
        &state,
        "admin",
        id,
        req.suspended,
        req.reason.trim(),
    )
    .await?;
    Ok(Json(Suspended {
        suspended: req.suspended,
        machines,
    }))
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct WithdrawRequest {
    /// Why it is withdrawn (recorded in the audit log and in the failure of what it cancels).
    pub reason: String,
}

/// Withdraw an artifact: its bytes are deleted, the jobs and runs that still need it are cancelled
/// with their budget released, and its hash is never served or accepted again.
#[utoipa::path(post, path = "/admin/artifacts/{hash}/withdraw", tag = "admin", request_body = WithdrawRequest, responses((status = 200, body = crate::workflows::withdraw::Withdrawn)))]
async fn withdraw_artifact(
    State(state): State<AppState>,
    _a: Admin,
    Path(hash): Path<Sha256Digest>,
    Json(req): Json<WithdrawRequest>,
) -> Result<Json<crate::workflows::withdraw::Withdrawn>, ApiError> {
    Ok(Json(
        crate::workflows::withdraw::withdraw_artifact(&state, "admin", hash, &req.reason).await?,
    ))
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct ValidatorAuthorized {
    pub authorized: bool,
    /// Verifications it held, handed back to the queue for another validator.
    pub released_verifications: u64,
}

/// Authorize a validator again, or withdraw it: its sessions end at once, the verifications it
/// holds go back to the queue for another validator, and it can no longer claim or decide. The
/// decisions it already made stay recorded and attributed (INCIDENTS.md, "compromised validator").
#[utoipa::path(post, path = "/admin/validators/{id}/authorize", tag = "admin", request_body = Authorize, responses((status = 200, body = ValidatorAuthorized)))]
async fn authorize_validator(
    State(state): State<AppState>,
    _a: Admin,
    Path(id): Path<ValidatorId>,
    Json(req): Json<Authorize>,
) -> Result<Json<ValidatorAuthorized>, ApiError> {
    let mut tx = state.db().begin().await?;
    if !identities::set_validator_authorized(&mut tx, id, req.authorized).await? {
        return Err(ApiError::NotFound("validator".into()));
    }
    let mut released = 0;
    if !req.authorized {
        identities::revoke_sessions(&mut tx, "validator", id.0).await?;
        released = tasks::release_all_verifications_of(&mut tx, id).await?;
    }
    misc::audit(
        &mut tx,
        "admin",
        "validator.authorize",
        &id.to_string(),
        serde_json::json!({ "authorized": req.authorized, "released_verifications": released }),
    )
    .await?;
    tx.commit().await?;
    if released > 0 {
        state.signals().verification.notify_waiters();
    }
    Ok(Json(ValidatorAuthorized {
        authorized: req.authorized,
        released_verifications: released,
    }))
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct WorkerInfo {
    pub worker_id: WorkerId,
    pub public_key: String,
    pub display_name: String,
    pub authorized: bool,
}

#[utoipa::path(get, path = "/admin/workers", tag = "admin", responses((status = 200, body = Vec<WorkerInfo>)))]
async fn list_workers(
    State(state): State<AppState>,
    _a: Admin,
) -> Result<Json<Vec<WorkerInfo>>, ApiError> {
    let mut c = state
        .db()
        .pool()
        .acquire()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let rows = identities::list_workers(&mut c).await?;
    Ok(Json(
        rows.into_iter()
            .map(|w| WorkerInfo {
                worker_id: WorkerId(w.id),
                public_key: hex::encode(&w.public_key),
                display_name: w.display_name,
                authorized: w.authorized,
            })
            .collect(),
    ))
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct CreateValidator {
    /// Ed25519 public key (hex).
    pub public_key: String,
    #[serde(default)]
    pub display_name: String,
    /// Operator this validator belongs to (name). Created on first use. Validation diversity is
    /// counted in operators, never in keys (ADR-0014).
    #[serde(default)]
    pub operator: String,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct ValidatorCreated {
    pub validator_id: ValidatorId,
    /// Operator the validator belongs to. Several keys of one operator count as one opinion
    /// (ADR-0014).
    pub operator_id: uuid::Uuid,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct CreateOperator {
    /// Unique name of the administrative entity running validators.
    pub name: String,
    /// Contact for the dispute procedure (no secrets).
    #[serde(default)]
    pub contact: String,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct OperatorView {
    pub operator_id: uuid::Uuid,
    pub name: String,
    pub contact: String,
    pub active: bool,
}

/// Register an operator: the administrative unit that validation diversity is counted in.
#[utoipa::path(post, path = "/admin/operators", tag = "admin", request_body = CreateOperator, responses((status = 200, body = OperatorView)))]
async fn create_operator(
    State(state): State<AppState>,
    _a: Admin,
    Json(req): Json<CreateOperator>,
) -> Result<Json<OperatorView>, ApiError> {
    if req.name.trim().is_empty() || req.name.len() > 64 || req.contact.len() > 256 {
        return Err(ApiError::Unprocessable(
            "name must be 1..=64 characters, contact at most 256".into(),
        ));
    }
    let mut tx = state.db().begin().await?;
    if let Some(existing) = federated::get_operator_by_name(&mut tx, &req.name).await? {
        return Ok(Json(OperatorView {
            operator_id: existing.id,
            name: existing.name,
            contact: existing.contact,
            active: existing.active,
        }));
    }
    let id = uuid::Uuid::now_v7();
    federated::insert_operator(&mut tx, id, &req.name, &req.contact).await?;
    misc::audit(
        &mut tx,
        "admin",
        "operator.create",
        &id.to_string(),
        serde_json::json!({ "name": req.name }),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(OperatorView {
        operator_id: id,
        name: req.name,
        contact: req.contact,
        active: true,
    }))
}

/// Operators and how many validators each runs.
#[utoipa::path(get, path = "/admin/operators", tag = "admin", responses((status = 200, body = Vec<OperatorView>)))]
async fn list_operators(
    State(state): State<AppState>,
    _a: Admin,
) -> Result<Json<Vec<OperatorView>>, ApiError> {
    let mut c = state
        .db()
        .pool()
        .acquire()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(
        federated::list_operators(&mut c)
            .await?
            .into_iter()
            .map(|o| OperatorView {
                operator_id: o.id,
                name: o.name,
                contact: o.contact,
                active: o.active,
            })
            .collect(),
    ))
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct DisputeView {
    pub dispute_id: uuid::Uuid,
    pub task_id: TaskId,
    pub attempt_id: AttemptId,
    pub state: String,
    /// Signed opinions that disagreed, with their validator and operator.
    #[schema(value_type = Object)]
    pub opinions: serde_json::Value,
    pub resolution: Option<String>,
    pub resolved_by: String,
    pub reason: String,
}

/// Disputes: disagreements between administratively distinct validators, waiting for an operator.
#[utoipa::path(get, path = "/admin/disputes", tag = "admin", responses((status = 200, body = Vec<DisputeView>)))]
async fn list_disputes(
    State(state): State<AppState>,
    _a: Admin,
    Query(q): Query<DisputeQuery>,
) -> Result<Json<Vec<DisputeView>>, ApiError> {
    let mut c = state
        .db()
        .pool()
        .acquire()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(
        federated::list_disputes(&mut c, q.open.unwrap_or(true), 200)
            .await?
            .into_iter()
            .map(|d| DisputeView {
                dispute_id: d.id,
                task_id: TaskId(d.task_id),
                attempt_id: AttemptId(d.attempt_id),
                state: d.state,
                opinions: d.outcomes,
                resolution: d.resolution,
                resolved_by: d.resolved_by,
                reason: d.reason,
            })
            .collect(),
    ))
}

#[derive(Deserialize, utoipa::IntoParams)]
pub struct DisputeQuery {
    /// Only open disputes (default true).
    pub open: Option<bool>,
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct ResolveDispute {
    /// `accepted`, `rejected` or `inconclusive`. Only an outcome a validator actually signed.
    pub outcome: VerificationOutcome,
    /// Why the operator settled this way (kept in the audit log).
    #[serde(default)]
    pub reason: String,
}

/// Settle a dispute. This is a human decision, recorded and audited; it is what moves credits.
#[utoipa::path(post, path = "/admin/disputes/{id}/resolve", tag = "admin", request_body = ResolveDispute, responses((status = 200, body = crate::workflows::federated::Resolved)))]
async fn resolve_dispute(
    State(state): State<AppState>,
    _a: Admin,
    Path(id): Path<uuid::Uuid>,
    Json(req): Json<ResolveDispute>,
) -> Result<Json<crate::workflows::federated::Resolved>, ApiError> {
    Ok(Json(
        crate::workflows::federated::resolve(&state, "admin", id, req.outcome, &req.reason).await?,
    ))
}

#[utoipa::path(post, path = "/admin/validators", tag = "admin", request_body = CreateValidator, responses((status = 200, body = ValidatorCreated)))]
async fn create_validator(
    State(state): State<AppState>,
    _a: Admin,
    Json(req): Json<CreateValidator>,
) -> Result<Json<ValidatorCreated>, ApiError> {
    let key: PublicKey = req.public_key.parse()?;
    let mut tx = state.db().begin().await?;
    let id = match identities::get_validator_by_key(&mut tx, &key.0).await? {
        Some(v) => ValidatorId(v.id),
        None => {
            let id = ValidatorId::new();
            identities::create_validator(&mut tx, id, &key.0, &req.display_name).await?;
            id
        }
    };
    identities::set_validator_authorized(&mut tx, id, true).await?;
    let operator_name = if req.operator.trim().is_empty() {
        "unassigned".to_owned()
    } else {
        req.operator.trim().to_owned()
    };
    let operator_id = match federated::get_operator_by_name(&mut tx, &operator_name).await? {
        Some(o) => o.id,
        None => {
            let oid = uuid::Uuid::now_v7();
            federated::insert_operator(&mut tx, oid, &operator_name, "").await?;
            oid
        }
    };
    federated::set_validator_operator(&mut tx, id, operator_id).await?;
    misc::audit(
        &mut tx,
        "admin",
        "validator.authorize",
        &id.to_string(),
        serde_json::json!({ "public_key": key, "operator": operator_name }),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(ValidatorCreated {
        validator_id: id,
        operator_id,
    }))
}

/// Prepare a batch of rewards to settle on a chain (ADR-0015). No funds move here: the batch is a
/// commitment, and only an operator's recorded observation of finality marks it settled.
#[utoipa::path(post, path = "/admin/reward-batches", tag = "admin", request_body = crate::workflows::rewards::PrepareBatch, responses((status = 200, body = crate::workflows::rewards::BatchView)))]
async fn prepare_reward_batch(
    State(state): State<AppState>,
    _a: Admin,
    Json(req): Json<crate::workflows::rewards::PrepareBatch>,
) -> Result<Json<crate::workflows::rewards::BatchView>, ApiError> {
    Ok(Json(
        crate::workflows::rewards::prepare(&state, "admin", req).await?,
    ))
}

/// Batches, newest first.
#[utoipa::path(get, path = "/admin/reward-batches", tag = "admin", responses((status = 200, body = Vec<crate::workflows::rewards::BatchView>)))]
async fn list_reward_batches(
    State(state): State<AppState>,
    _a: Admin,
) -> Result<Json<Vec<crate::workflows::rewards::BatchView>>, ApiError> {
    Ok(Json(crate::workflows::rewards::list(&state).await?))
}

/// One batch with its leaves and proofs: this is what a beneficiary needs to claim.
#[utoipa::path(get, path = "/admin/reward-batches/{batch_id}", tag = "admin", responses((status = 200, body = crate::workflows::rewards::BatchView)))]
async fn get_reward_batch(
    State(state): State<AppState>,
    _a: Admin,
    Path(batch_id): Path<String>,
) -> Result<Json<crate::workflows::rewards::BatchView>, ApiError> {
    Ok(Json(
        crate::workflows::rewards::view(&state, &batch_id, true).await?,
    ))
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct SettlementRequest {
    /// `submitted`, `observed`, `finalized` or `reconcile`.
    pub event: crate::workflows::rewards::SettlementEvent,
    /// Transaction hash (required for a submission).
    #[serde(default)]
    pub tx_hash: String,
    /// Block height, for an observation.
    #[serde(default)]
    pub height: Option<i64>,
    /// Root actually seen on chain (required for an observation).
    #[serde(default)]
    pub observed_root: Option<String>,
    /// Operator note, kept in the audit log.
    #[serde(default)]
    pub note: String,
}

/// Record a settlement event. The control plane never infers these from a chain: an operator
/// reports what was submitted, observed and finalized, and a transaction hash alone settles nothing.
#[utoipa::path(post, path = "/admin/reward-batches/{batch_id}/settlement", tag = "admin", request_body = SettlementRequest, responses((status = 200, body = crate::workflows::rewards::BatchView)))]
async fn settle_reward_batch(
    State(state): State<AppState>,
    _a: Admin,
    Path(batch_id): Path<String>,
    Json(req): Json<SettlementRequest>,
) -> Result<Json<crate::workflows::rewards::BatchView>, ApiError> {
    Ok(Json(
        crate::workflows::rewards::settle(
            &state,
            "admin",
            &batch_id,
            req.event,
            &req.tx_hash,
            req.height,
            req.observed_root.as_deref(),
            &req.note,
        )
        .await?,
    ))
}

#[derive(Deserialize, utoipa::ToSchema)]
pub struct SetAuditRate {
    /// Share of accepted submissions re-verified by a distinct operator, between 0 and 1.
    pub rate: f64,
    /// Why the operator changes it (kept in the audit log).
    #[serde(default)]
    pub reason: String,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct AuditRate {
    /// Rate in force after the call.
    pub audit_rate: f64,
    /// Rate configured at start-up, for comparison.
    pub configured_at_start: f64,
}

/// Read the audit rate in force.
#[utoipa::path(get, path = "/admin/config/audit-rate", tag = "admin", responses((status = 200, body = AuditRate)))]
async fn get_audit_rate(State(state): State<AppState>, _a: Admin) -> Json<AuditRate> {
    Json(AuditRate {
        audit_rate: state.audit_rate(),
        configured_at_start: state.config().audit_rate,
    })
}

/// Change the audit rate while the server runs (docs/DECISIONS.md, D2). The right rate depends on
/// observed behaviour, so it must not require a redeployment; the change is audited.
#[utoipa::path(post, path = "/admin/config/audit-rate", tag = "admin", request_body = SetAuditRate, responses((status = 200, body = AuditRate)))]
async fn set_audit_rate(
    State(state): State<AppState>,
    _a: Admin,
    Json(req): Json<SetAuditRate>,
) -> Result<Json<AuditRate>, ApiError> {
    if req.reason.len() > 500 {
        return Err(ApiError::Unprocessable("reason too long".into()));
    }
    let applied = state
        .set_audit_rate(req.rate)
        .map_err(ApiError::Unprocessable)?;
    let mut tx = state.db().begin().await?;
    misc::audit(
        &mut tx,
        "admin",
        "config.audit_rate",
        "server",
        serde_json::json!({ "rate": applied, "reason": req.reason }),
    )
    .await?;
    tx.commit().await?;
    tracing::info!(audit_rate = applied, reason = %req.reason, "audit rate changed by an operator");
    Ok(Json(AuditRate {
        audit_rate: applied,
        configured_at_start: state.config().audit_rate,
    }))
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct TaskSummary {
    pub by_state: Vec<(String, i64)>,
    pub outbox_pending: i64,
}

#[utoipa::path(get, path = "/admin/tasks/summary", tag = "admin", responses((status = 200, body = TaskSummary)))]
async fn task_summary(
    State(state): State<AppState>,
    _a: Admin,
) -> Result<Json<TaskSummary>, ApiError> {
    let mut c = state
        .db()
        .pool()
        .acquire()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(TaskSummary {
        by_state: jobs::count_tasks_by_state(&mut c).await?,
        outbox_pending: misc::outbox_pending_count(&mut c).await?,
    }))
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct AuditEntry {
    pub actor: String,
    pub action: String,
    pub subject: String,
    pub detail: serde_json::Value,
}

#[utoipa::path(get, path = "/admin/audit", tag = "admin", responses((status = 200, body = Vec<AuditEntry>)))]
async fn audit(
    State(state): State<AppState>,
    _a: Admin,
) -> Result<Json<Vec<AuditEntry>>, ApiError> {
    let mut c = state
        .db()
        .pool()
        .acquire()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let rows = misc::list_audit(&mut c, 200).await?;
    Ok(Json(
        rows.into_iter()
            .map(|(actor, action, subject, detail)| AuditEntry {
                actor,
                action,
                subject,
                detail,
            })
            .collect(),
    ))
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct ProjectInfo {
    pub project_id: ProjectId,
    pub name: String,
    pub state: String,
    pub allowed_kinds: Vec<String>,
    pub budget: Budget,
}

#[utoipa::path(get, path = "/admin/projects", tag = "admin", responses((status = 200, body = Vec<ProjectInfo>)))]
async fn list_projects(
    State(state): State<AppState>,
    _a: Admin,
) -> Result<Json<Vec<ProjectInfo>>, ApiError> {
    let mut c = state
        .db()
        .pool()
        .acquire()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let mut out = Vec::new();
    for p in identities::list_projects(&mut c).await? {
        let b = gg_ledger::budget(&mut c, ProjectId(p.id)).await?;
        out.push(ProjectInfo {
            project_id: ProjectId(p.id),
            name: p.name,
            state: p.state,
            allowed_kinds: p.allowed_kinds,
            budget: Budget {
                available: b.available,
                reserved: b.reserved,
                spent: b.spent,
            },
        });
    }
    Ok(Json(out))
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct ResearcherInfo {
    pub researcher_id: ResearcherId,
    pub display_name: String,
    pub orcid: Option<String>,
    /// Verified e-mail address, when the researcher signed in by e-mail.
    pub email: Option<String>,
    pub admission: String,
}

#[utoipa::path(get, path = "/admin/researchers", tag = "admin", responses((status = 200, body = Vec<ResearcherInfo>)))]
async fn list_researchers(
    State(state): State<AppState>,
    _a: Admin,
) -> Result<Json<Vec<ResearcherInfo>>, ApiError> {
    let mut c = state
        .db()
        .pool()
        .acquire()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let rows = identities::list_researchers(&mut c).await?;
    let mut emails = identities::researcher_emails(&mut c).await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| ResearcherInfo {
                researcher_id: ResearcherId(r.id),
                display_name: r.display_name,
                orcid: r.orcid,
                email: emails.remove(&r.id),
                admission: r.admission_state,
            })
            .collect(),
    ))
}

#[utoipa::path(post, path = "/admin/researchers/{id}/revoke", tag = "admin", responses((status = 200, body = Revoked)))]
async fn revoke_researcher(
    State(state): State<AppState>,
    _a: Admin,
    Path(id): Path<ResearcherId>,
) -> Result<Json<Revoked>, ApiError> {
    let mut tx = state.db().begin().await?;
    identities::get_researcher(&mut tx, id)
        .await?
        .ok_or_else(|| ApiError::NotFound("researcher".into()))?;
    let n = identities::revoke_researcher_access(&mut tx, id).await?;
    misc::audit(
        &mut tx,
        "admin",
        "researcher.revoke",
        &id.to_string(),
        serde_json::json!({ "revoked": n }),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(Revoked { revoked: n }))
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct Revoked {
    pub revoked: u64,
}

#[utoipa::path(get, path = "/admin/metrics", tag = "admin", responses((status = 200, description = "Prometheus text format", content_type = "text/plain")))]
async fn metrics(
    State(state): State<AppState>,
    _a: Admin,
) -> Result<axum::response::Response, ApiError> {
    let mut out = String::new();
    state.metrics().render_requests(&mut out);
    let mut c = state
        .db()
        .pool()
        .acquire()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    out.push_str("# TYPE gg_tasks gauge\n");
    for (st, n) in jobs::count_tasks_by_state(&mut c).await? {
        out.push_str(&format!("gg_tasks{{state=\"{st}\"}} {n}\n"));
    }
    out.push_str(&format!(
        "# TYPE gg_outbox_pending gauge\ngg_outbox_pending {}\n",
        misc::outbox_pending_count(&mut c).await?
    ));
    out.push_str(&format!(
        "# TYPE gg_uptime_seconds gauge\ngg_uptime_seconds {}\n",
        state.uptime_secs()
    ));
    axum::response::Response::builder()
        .status(200)
        .header("content-type", "text/plain; version=0.0.4")
        .body(axum::body::Body::from(out))
        .map_err(|e| ApiError::Internal(e.to_string()))
}

/// Routes.
pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(metrics))
        .routes(routes!(list_researchers))
        .routes(routes!(revoke_researcher))
        .routes(routes!(create_researcher))
        .routes(routes!(set_admission))
        .routes(routes!(create_project))
        .routes(routes!(list_projects))
        .routes(routes!(grant))
        .routes(routes!(add_member))
        .routes(routes!(authorize_worker))
        .routes(routes!(list_workers))
        .routes(routes!(create_validator))
        .routes(routes!(authorize_validator))
        .routes(routes!(withdraw_artifact))
        .routes(routes!(list_contributors))
        .routes(routes!(suspend_contributor))
        .routes(routes!(create_operator))
        .routes(routes!(list_operators))
        .routes(routes!(list_disputes))
        .routes(routes!(resolve_dispute))
        .routes(routes!(get_audit_rate))
        .routes(routes!(set_audit_rate))
        .routes(routes!(prepare_reward_batch))
        .routes(routes!(list_reward_batches))
        .routes(routes!(get_reward_batch))
        .routes(routes!(settle_reward_batch))
        .routes(routes!(task_summary))
        .routes(routes!(audit))
}
