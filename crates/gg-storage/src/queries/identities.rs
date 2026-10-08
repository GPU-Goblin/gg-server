//! Researchers, API tokens, projects, memberships, workers, validators, challenges, sessions.

use ::time::OffsetDateTime;
use gg_core::*;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::StorageError;
use crate::models::*;

/// Create a researcher.
pub async fn create_researcher(
    c: &mut PgConnection,
    id: ResearcherId,
    display_name: &str,
    admission: AdmissionState,
) -> Result<(), StorageError> {
    sqlx::query("INSERT INTO researchers (id, display_name, admission_state) VALUES ($1, $2, $3)")
        .bind(id.0)
        .bind(display_name)
        .bind(admission.as_str())
        .execute(c)
        .await?;
    Ok(())
}

/// Get a researcher.
pub async fn get_researcher(
    c: &mut PgConnection,
    id: ResearcherId,
) -> Result<Option<ResearcherRow>, StorageError> {
    Ok(sqlx::query_as::<_, ResearcherRow>(
        "SELECT id, display_name, orcid, admission_state, admission_note, created_at FROM researchers WHERE id = $1",
    )
    .bind(id.0)
    .fetch_optional(c)
    .await?)
}

/// Set admission state with a note.
pub async fn set_admission(
    c: &mut PgConnection,
    id: ResearcherId,
    state: AdmissionState,
    note: &str,
) -> Result<bool, StorageError> {
    let r = sqlx::query("UPDATE researchers SET admission_state = $2, admission_note = $3, updated_at = now() WHERE id = $1")
        .bind(id.0)
        .bind(state.as_str())
        .bind(note)
        .execute(c)
        .await?;
    Ok(r.rows_affected() == 1)
}

/// Insert a hashed API token.
pub async fn create_token(
    c: &mut PgConnection,
    token_hash: &Sha256Digest,
    role: &str,
    researcher_id: Option<ResearcherId>,
    label: &str,
    expires_at: Option<OffsetDateTime>,
) -> Result<(), StorageError> {
    sqlx::query("INSERT INTO api_tokens (token_hash, role, researcher_id, label, expires_at) VALUES ($1, $2, $3, $4, $5)")
        .bind(token_hash.as_bytes().as_slice())
        .bind(role)
        .bind(researcher_id.map(|r| r.0))
        .bind(label)
        .bind(expires_at)
        .execute(c)
        .await?;
    Ok(())
}

/// Look up a token by hash (caller checks expiry/revocation).
pub async fn lookup_token(
    c: &mut PgConnection,
    token_hash: &Sha256Digest,
) -> Result<Option<TokenRow>, StorageError> {
    Ok(sqlx::query_as::<_, TokenRow>(
        "SELECT role, researcher_id, label, expires_at, revoked_at FROM api_tokens WHERE token_hash = $1",
    )
    .bind(token_hash.as_bytes().as_slice())
    .fetch_optional(c)
    .await?)
}

/// Revoke a token.
pub async fn revoke_token(
    c: &mut PgConnection,
    token_hash: &Sha256Digest,
) -> Result<bool, StorageError> {
    let r = sqlx::query(
        "UPDATE api_tokens SET revoked_at = now() WHERE token_hash = $1 AND revoked_at IS NULL",
    )
    .bind(token_hash.as_bytes().as_slice())
    .execute(c)
    .await?;
    Ok(r.rows_affected() == 1)
}

/// Create a project with an empty budget row.
pub async fn create_project(
    c: &mut PgConnection,
    id: ProjectId,
    name: &str,
    state: ProjectState,
    allowed_kinds: &[String],
) -> Result<(), StorageError> {
    sqlx::query("INSERT INTO projects (id, name, state, allowed_kinds) VALUES ($1, $2, $3, $4)")
        .bind(id.0)
        .bind(name)
        .bind(state.as_str())
        .bind(allowed_kinds)
        .execute(&mut *c)
        .await?;
    sqlx::query("INSERT INTO project_budgets (project_id) VALUES ($1)")
        .bind(id.0)
        .execute(c)
        .await?;
    Ok(())
}

/// Get a project.
pub async fn get_project(
    c: &mut PgConnection,
    id: ProjectId,
) -> Result<Option<ProjectRow>, StorageError> {
    Ok(sqlx::query_as::<_, ProjectRow>(
        "SELECT id, name, state, allowed_kinds, created_at FROM projects WHERE id = $1",
    )
    .bind(id.0)
    .fetch_optional(c)
    .await?)
}

/// List projects a researcher belongs to.
pub async fn list_projects_for(
    c: &mut PgConnection,
    researcher: ResearcherId,
) -> Result<Vec<ProjectRow>, StorageError> {
    Ok(sqlx::query_as::<_, ProjectRow>(
        "SELECT p.id, p.name, p.state, p.allowed_kinds, p.created_at FROM projects p
         JOIN project_memberships m ON m.project_id = p.id WHERE m.researcher_id = $1 ORDER BY p.created_at",
    )
    .bind(researcher.0)
    .fetch_all(c)
    .await?)
}

/// List all projects (admin).
pub async fn list_projects(c: &mut PgConnection) -> Result<Vec<ProjectRow>, StorageError> {
    Ok(sqlx::query_as::<_, ProjectRow>(
        "SELECT id, name, state, allowed_kinds, created_at FROM projects ORDER BY created_at",
    )
    .fetch_all(c)
    .await?)
}

/// Set project state.
pub async fn set_project_state(
    c: &mut PgConnection,
    id: ProjectId,
    state: ProjectState,
) -> Result<bool, StorageError> {
    let r = sqlx::query("UPDATE projects SET state = $2, updated_at = now() WHERE id = $1")
        .bind(id.0)
        .bind(state.as_str())
        .execute(c)
        .await?;
    Ok(r.rows_affected() == 1)
}

/// Set allowed compute kinds.
pub async fn set_allowed_kinds(
    c: &mut PgConnection,
    id: ProjectId,
    kinds: &[String],
) -> Result<bool, StorageError> {
    let r = sqlx::query("UPDATE projects SET allowed_kinds = $2, updated_at = now() WHERE id = $1")
        .bind(id.0)
        .bind(kinds)
        .execute(c)
        .await?;
    Ok(r.rows_affected() == 1)
}

/// Add or update a membership.
pub async fn upsert_membership(
    c: &mut PgConnection,
    project: ProjectId,
    researcher: ResearcherId,
    role: ProjectRole,
) -> Result<(), StorageError> {
    sqlx::query(
        "INSERT INTO project_memberships (project_id, researcher_id, role) VALUES ($1, $2, $3)
         ON CONFLICT (project_id, researcher_id) DO UPDATE SET role = EXCLUDED.role",
    )
    .bind(project.0)
    .bind(researcher.0)
    .bind(role.as_str())
    .execute(c)
    .await?;
    Ok(())
}

/// Membership role of a researcher on a project.
pub async fn membership_role(
    c: &mut PgConnection,
    project: ProjectId,
    researcher: ResearcherId,
) -> Result<Option<ProjectRole>, StorageError> {
    let r: Option<(String,)> = sqlx::query_as(
        "SELECT role FROM project_memberships WHERE project_id = $1 AND researcher_id = $2",
    )
    .bind(project.0)
    .bind(researcher.0)
    .fetch_optional(c)
    .await?;
    r.map(|(s,)| {
        s.parse::<ProjectRole>()
            .map_err(|e| StorageError::Corrupt(e.to_string()))
    })
    .transpose()
}

/// Register a worker key (unauthorized until an operator authorizes it).
pub async fn create_worker(
    c: &mut PgConnection,
    id: WorkerId,
    public_key: &[u8; 32],
    name: &str,
) -> Result<(), StorageError> {
    sqlx::query("INSERT INTO workers (id, public_key, display_name) VALUES ($1, $2, $3)")
        .bind(id.0)
        .bind(public_key.as_slice())
        .bind(name)
        .execute(c)
        .await?;
    Ok(())
}

/// Worker by key.
pub async fn get_worker_by_key(
    c: &mut PgConnection,
    public_key: &[u8; 32],
) -> Result<Option<WorkerRow>, StorageError> {
    Ok(sqlx::query_as::<_, WorkerRow>(
        "SELECT id, public_key, display_name, authorized, payout_address, created_at FROM workers WHERE public_key = $1",
    )
    .bind(public_key.as_slice())
    .fetch_optional(c)
    .await?)
}

/// Worker by id.
pub async fn get_worker(
    c: &mut PgConnection,
    id: WorkerId,
) -> Result<Option<WorkerRow>, StorageError> {
    Ok(sqlx::query_as::<_, WorkerRow>(
        "SELECT id, public_key, display_name, authorized, payout_address, created_at FROM workers WHERE id = $1",
    )
    .bind(id.0)
    .fetch_optional(c)
    .await?)
}

/// Record the data rule a project accepted (docs/DECISIONS.md, D4).
pub async fn set_project_data_policy(
    c: &mut PgConnection,
    id: ProjectId,
    policy: &str,
) -> Result<(), StorageError> {
    sqlx::query("UPDATE projects SET data_policy = $2, updated_at = now() WHERE id = $1")
        .bind(id.0)
        .bind(policy)
        .execute(c)
        .await?;
    Ok(())
}

/// Set the address a contributor wants to be paid at. The platform never chooses it.
pub async fn set_payout_address(
    c: &mut PgConnection,
    id: WorkerId,
    address: &str,
) -> Result<bool, StorageError> {
    Ok(
        sqlx::query("UPDATE workers SET payout_address = $2 WHERE id = $1")
            .bind(id.0)
            .bind(address)
            .execute(c)
            .await?
            .rows_affected()
            == 1,
    )
}

/// List workers.
pub async fn list_workers(c: &mut PgConnection) -> Result<Vec<WorkerRow>, StorageError> {
    Ok(sqlx::query_as::<_, WorkerRow>(
        "SELECT id, public_key, display_name, authorized, payout_address, created_at FROM workers ORDER BY created_at",
    )
    .fetch_all(c)
    .await?)
}

/// Authorize or revoke a worker.
pub async fn set_worker_authorized(
    c: &mut PgConnection,
    id: WorkerId,
    authorized: bool,
) -> Result<bool, StorageError> {
    let r = sqlx::query("UPDATE workers SET authorized = $2 WHERE id = $1")
        .bind(id.0)
        .bind(authorized)
        .execute(c)
        .await?;
    Ok(r.rows_affected() == 1)
}

/// Touch last_seen.
pub async fn touch_worker(c: &mut PgConnection, id: WorkerId) -> Result<(), StorageError> {
    sqlx::query("UPDATE workers SET last_seen_at = now() WHERE id = $1")
        .bind(id.0)
        .execute(c)
        .await?;
    Ok(())
}

/// Record what a worker announced at its last claim (C1).
pub async fn announce_capabilities(
    c: &mut PgConnection,
    id: WorkerId,
    caps: &super::jobs::WorkerCapabilities,
) -> Result<(), StorageError> {
    sqlx::query(
        "UPDATE workers SET announced_ram_bytes = $2, announced_device_bytes = $3, announced_profile = $4,
                announced_engine = $5, announced_kinds = $6, announced_serves_peers = $7,
                announced_at = now() WHERE id = $1",
    )
    .bind(id.0)
    .bind(caps.ram_bytes)
    .bind(caps.device_bytes)
    .bind(caps.profile)
    .bind(caps.engine.as_deref())
    .bind(&caps.kinds)
    .bind(caps.serves_peers)
    .execute(c)
    .await?;
    Ok(())
}

/// Announced RAM, device memory, profile and engine of one worker.
type CapabilityRow = (
    Option<i64>,
    Option<i64>,
    Option<i32>,
    Option<String>,
    Option<Vec<String>>,
);

/// Capabilities announced by authorized workers during the last `within_secs` seconds, largest
/// accelerator memory first.
pub async fn recent_capabilities(
    c: &mut PgConnection,
    within_secs: i64,
) -> Result<Vec<super::jobs::WorkerCapabilities>, StorageError> {
    let rows: Vec<CapabilityRow> = sqlx::query_as(
        "SELECT announced_ram_bytes, announced_device_bytes, announced_profile, announced_engine, announced_kinds
           FROM workers WHERE authorized AND announced_at > now() - make_interval(secs => $1)
          ORDER BY announced_device_bytes DESC NULLS LAST",
    )
    .bind(within_secs as f64)
    .fetch_all(c)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(ram, device, profile, engine, kinds)| super::jobs::WorkerCapabilities {
                ram_bytes: ram,
                device_bytes: device,
                profile: profile.unwrap_or(1),
                engine,
                kinds: kinds.unwrap_or_default(),
                // Not recorded: planning looks at memory and profile only.
                serves_peers: false,
            },
        )
        .collect())
}

/// Register a validator key.
pub async fn create_validator(
    c: &mut PgConnection,
    id: ValidatorId,
    public_key: &[u8; 32],
    name: &str,
) -> Result<(), StorageError> {
    sqlx::query("INSERT INTO validators (id, public_key, display_name) VALUES ($1, $2, $3)")
        .bind(id.0)
        .bind(public_key.as_slice())
        .bind(name)
        .execute(c)
        .await?;
    Ok(())
}

/// Validator by key.
pub async fn get_validator_by_key(
    c: &mut PgConnection,
    public_key: &[u8; 32],
) -> Result<Option<ValidatorRow>, StorageError> {
    Ok(sqlx::query_as::<_, ValidatorRow>(
        "SELECT id, public_key, display_name, authorized, created_at FROM validators WHERE public_key = $1",
    )
    .bind(public_key.as_slice())
    .fetch_optional(c)
    .await?)
}

/// Validator by id.
pub async fn get_validator(
    c: &mut PgConnection,
    id: ValidatorId,
) -> Result<Option<ValidatorRow>, StorageError> {
    Ok(sqlx::query_as::<_, ValidatorRow>(
        "SELECT id, public_key, display_name, authorized, created_at FROM validators WHERE id = $1",
    )
    .bind(id.0)
    .fetch_optional(c)
    .await?)
}

/// Authorize a validator.
pub async fn set_validator_authorized(
    c: &mut PgConnection,
    id: ValidatorId,
    authorized: bool,
) -> Result<bool, StorageError> {
    let r = sqlx::query("UPDATE validators SET authorized = $2 WHERE id = $1")
        .bind(id.0)
        .bind(authorized)
        .execute(c)
        .await?;
    Ok(r.rows_affected() == 1)
}

/// Insert a single-use login challenge.
pub async fn insert_challenge(
    c: &mut PgConnection,
    nonce: &[u8; 32],
    action: &str,
    public_key: &[u8; 32],
    ttl_secs: i64,
) -> Result<(), StorageError> {
    sqlx::query(
        "INSERT INTO login_challenges (nonce, action, public_key, expires_at) VALUES ($1, $2, $3, now() + make_interval(secs => $4))",
    )
    .bind(nonce.as_slice())
    .bind(action)
    .bind(public_key.as_slice())
    .bind(ttl_secs as f64)
    .execute(c)
    .await?;
    Ok(())
}

/// Consume a challenge atomically; returns false if unknown, expired, consumed or mismatched.
pub async fn consume_challenge(
    c: &mut PgConnection,
    nonce: &[u8],
    action: &str,
    public_key: &[u8; 32],
) -> Result<bool, StorageError> {
    let r = sqlx::query(
        "UPDATE login_challenges SET consumed_at = now()
         WHERE nonce = $1 AND action = $2 AND public_key = $3 AND consumed_at IS NULL AND expires_at > now()",
    )
    .bind(nonce)
    .bind(action)
    .bind(public_key.as_slice())
    .execute(c)
    .await?;
    Ok(r.rows_affected() == 1)
}

/// Create a session.
pub async fn create_session(
    c: &mut PgConnection,
    token_hash: &Sha256Digest,
    principal_kind: &str,
    principal_id: Uuid,
    ttl_secs: i64,
) -> Result<(), StorageError> {
    sqlx::query(
        "INSERT INTO sessions (token_hash, principal_kind, principal_id, expires_at) VALUES ($1, $2, $3, now() + make_interval(secs => $4))",
    )
    .bind(token_hash.as_bytes().as_slice())
    .bind(principal_kind)
    .bind(principal_id)
    .bind(ttl_secs as f64)
    .execute(c)
    .await?;
    Ok(())
}

/// Look up a live session.
pub async fn lookup_session(
    c: &mut PgConnection,
    token_hash: &Sha256Digest,
) -> Result<Option<SessionRow>, StorageError> {
    Ok(sqlx::query_as::<_, SessionRow>(
        "SELECT principal_kind, principal_id, expires_at, revoked_at FROM sessions
         WHERE token_hash = $1 AND revoked_at IS NULL AND expires_at > now()",
    )
    .bind(token_hash.as_bytes().as_slice())
    .fetch_optional(c)
    .await?)
}

/// Revoke all sessions of a principal.
pub async fn revoke_sessions(
    c: &mut PgConnection,
    principal_kind: &str,
    principal_id: Uuid,
) -> Result<u64, StorageError> {
    let r = sqlx::query("UPDATE sessions SET revoked_at = now() WHERE principal_kind = $1 AND principal_id = $2 AND revoked_at IS NULL")
        .bind(principal_kind)
        .bind(principal_id)
        .execute(c)
        .await?;
    Ok(r.rows_affected())
}

/// Researcher by ORCID iD.
pub async fn get_researcher_by_orcid(
    c: &mut PgConnection,
    orcid: &str,
) -> Result<Option<ResearcherRow>, StorageError> {
    Ok(sqlx::query_as::<_, ResearcherRow>(
        "SELECT id, display_name, orcid, admission_state, admission_note, created_at FROM researchers WHERE orcid = $1",
    )
    .bind(orcid)
    .fetch_optional(c)
    .await?)
}

/// Create a researcher linked to an ORCID iD (admission pending).
pub async fn create_researcher_with_orcid(
    c: &mut PgConnection,
    id: ResearcherId,
    display_name: &str,
    orcid: &str,
) -> Result<(), StorageError> {
    sqlx::query(
        "INSERT INTO researchers (id, display_name, orcid, orcid_name, orcid_linked_at, admission_state)
         VALUES ($1, $2, $3, $2, now(), 'pending')",
    )
    .bind(id.0)
    .bind(display_name)
    .bind(orcid)
    .execute(c)
    .await?;
    Ok(())
}

/// Refresh the name reported by ORCID at login.
pub async fn touch_orcid_login(
    c: &mut PgConnection,
    id: ResearcherId,
    name: &str,
) -> Result<(), StorageError> {
    sqlx::query("UPDATE researchers SET orcid_name = $2, orcid_linked_at = now(), updated_at = now() WHERE id = $1")
        .bind(id.0)
        .bind(name)
        .execute(c)
        .await?;
    Ok(())
}

/// List researchers (admin).
pub async fn list_researchers(c: &mut PgConnection) -> Result<Vec<ResearcherRow>, StorageError> {
    Ok(sqlx::query_as::<_, ResearcherRow>(
        "SELECT id, display_name, orcid, admission_state, admission_note, created_at FROM researchers ORDER BY created_at",
    )
    .fetch_all(c)
    .await?)
}

/// Store a single-use OAuth state bound to the login cookie.
pub async fn insert_oauth_state(
    c: &mut PgConnection,
    state_hash: &Sha256Digest,
    cookie_hash: &Sha256Digest,
    provider: &str,
    ttl_secs: i64,
) -> Result<(), StorageError> {
    sqlx::query(
        "INSERT INTO oauth_states (state_hash, cookie_hash, provider, expires_at) VALUES ($1, $2, $3, now() + make_interval(secs => $4))",
    )
    .bind(state_hash.as_bytes().as_slice())
    .bind(cookie_hash.as_bytes().as_slice())
    .bind(provider)
    .bind(ttl_secs as f64)
    .execute(c)
    .await?;
    Ok(())
}

/// Consume an OAuth state; false if unknown, expired, consumed or bound to another cookie.
pub async fn consume_oauth_state(
    c: &mut PgConnection,
    state_hash: &Sha256Digest,
    cookie_hash: &Sha256Digest,
) -> Result<bool, StorageError> {
    let r = sqlx::query(
        "UPDATE oauth_states SET consumed_at = now()
         WHERE state_hash = $1 AND cookie_hash = $2 AND consumed_at IS NULL AND expires_at > now()",
    )
    .bind(state_hash.as_bytes().as_slice())
    .bind(cookie_hash.as_bytes().as_slice())
    .execute(c)
    .await?;
    Ok(r.rows_affected() == 1)
}

/// Create a portal session.
pub async fn create_portal_session(
    c: &mut PgConnection,
    session_hash: &Sha256Digest,
    researcher: ResearcherId,
    ttl_secs: i64,
    user_agent: &str,
) -> Result<(), StorageError> {
    sqlx::query(
        "INSERT INTO portal_sessions (session_hash, researcher_id, expires_at, user_agent) VALUES ($1, $2, now() + make_interval(secs => $3), $4)",
    )
    .bind(session_hash.as_bytes().as_slice())
    .bind(researcher.0)
    .bind(ttl_secs as f64)
    .bind(user_agent)
    .execute(c)
    .await?;
    Ok(())
}

/// Researcher of a live portal session.
pub async fn lookup_portal_session(
    c: &mut PgConnection,
    session_hash: &Sha256Digest,
) -> Result<Option<ResearcherId>, StorageError> {
    let r: Option<(Uuid,)> = sqlx::query_as(
        "SELECT researcher_id FROM portal_sessions WHERE session_hash = $1 AND revoked_at IS NULL AND expires_at > now()",
    )
    .bind(session_hash.as_bytes().as_slice())
    .fetch_optional(c)
    .await?;
    Ok(r.map(|(u,)| ResearcherId(u)))
}

/// Revoke one portal session.
pub async fn revoke_portal_session(
    c: &mut PgConnection,
    session_hash: &Sha256Digest,
) -> Result<(), StorageError> {
    sqlx::query("UPDATE portal_sessions SET revoked_at = now() WHERE session_hash = $1 AND revoked_at IS NULL")
        .bind(session_hash.as_bytes().as_slice())
        .execute(c)
        .await?;
    Ok(())
}

/// Revoke every portal session and API token of a researcher (admin).
pub async fn revoke_researcher_access(
    c: &mut PgConnection,
    researcher: ResearcherId,
) -> Result<u64, StorageError> {
    let a = sqlx::query("UPDATE portal_sessions SET revoked_at = now() WHERE researcher_id = $1 AND revoked_at IS NULL")
        .bind(researcher.0)
        .execute(&mut *c)
        .await?;
    let b = sqlx::query(
        "UPDATE api_tokens SET revoked_at = now() WHERE researcher_id = $1 AND revoked_at IS NULL",
    )
    .bind(researcher.0)
    .execute(c)
    .await?;
    Ok(a.rows_affected() + b.rows_affected())
}

/// Record a single-use e-mail sign-in link (token hashed).
pub async fn insert_email_link(
    c: &mut PgConnection,
    token_hash: &Sha256Digest,
    email: &str,
    ttl_secs: i64,
) -> Result<(), StorageError> {
    sqlx::query(
        "INSERT INTO email_login_links (token_hash, email, expires_at)
         VALUES ($1, $2, now() + make_interval(secs => $3))",
    )
    .bind(token_hash.as_bytes().as_slice())
    .bind(email)
    .bind(ttl_secs as f64)
    .execute(c)
    .await?;
    Ok(())
}

/// Links sent to `email` during the last `window_secs` seconds (rate limit per address).
pub async fn count_recent_email_links(
    c: &mut PgConnection,
    email: &str,
    window_secs: i64,
) -> Result<i64, StorageError> {
    Ok(sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM email_login_links
         WHERE lower(email) = lower($1) AND created_at > now() - make_interval(secs => $2)",
    )
    .bind(email)
    .bind(window_secs as f64)
    .fetch_one(c)
    .await?)
}

/// Consume a link: returns its address if it was unused and not expired, and marks it used.
pub async fn consume_email_link(
    c: &mut PgConnection,
    token_hash: &Sha256Digest,
) -> Result<Option<String>, StorageError> {
    Ok(sqlx::query_scalar::<_, String>(
        "UPDATE email_login_links SET consumed_at = now()
         WHERE token_hash = $1 AND consumed_at IS NULL AND expires_at > now()
         RETURNING email",
    )
    .bind(token_hash.as_bytes().as_slice())
    .fetch_optional(c)
    .await?)
}

/// Researcher signed in with this e-mail address, if any (case-insensitive).
pub async fn get_researcher_by_email(
    c: &mut PgConnection,
    email: &str,
) -> Result<Option<ResearcherRow>, StorageError> {
    Ok(sqlx::query_as::<_, ResearcherRow>(
        "SELECT id, display_name, orcid, admission_state, admission_note, created_at FROM researchers WHERE lower(email) = lower($1)",
    )
    .bind(email)
    .fetch_optional(c)
    .await?)
}

/// Create a researcher from a verified e-mail address, pending admission.
pub async fn create_researcher_with_email(
    c: &mut PgConnection,
    id: ResearcherId,
    email: &str,
) -> Result<(), StorageError> {
    sqlx::query(
        "INSERT INTO researchers (id, display_name, email, email_verified_at, admission_state)
         VALUES ($1, $2, $2, now(), 'pending')",
    )
    .bind(id.0)
    .bind(email)
    .execute(c)
    .await?;
    Ok(())
}

/// Create a researcher who asked for access from the public site: a verified e-mail address, the
/// name to show, the laboratory and what they want to compute; admission pending.
pub async fn create_access_request(
    c: &mut PgConnection,
    id: ResearcherId,
    email: &str,
    name: &str,
    lab: &str,
    purpose: &str,
) -> Result<(), StorageError> {
    sqlx::query(
        "INSERT INTO researchers (id, display_name, email, email_verified_at, admission_state, lab, purpose, requested_at)
         VALUES ($1, $2, $3, now(), 'pending', $4, $5, now())",
    )
    .bind(id.0)
    .bind(name)
    .bind(email)
    .bind(lab)
    .bind(purpose)
    .execute(c)
    .await?;
    Ok(())
}

/// The access requests of researchers (laboratory, purpose), by id; empty for those without one.
pub async fn access_requests(
    c: &mut PgConnection,
) -> Result<std::collections::HashMap<uuid::Uuid, (String, String)>, StorageError> {
    Ok(sqlx::query_as::<_, (uuid::Uuid, String, String)>(
        "SELECT id, lab, purpose FROM researchers WHERE requested_at IS NOT NULL",
    )
    .fetch_all(c)
    .await?
    .into_iter()
    .map(|(id, lab, purpose)| (id, (lab, purpose)))
    .collect())
}

/// Record a new sign-in by e-mail.
pub async fn touch_email_login(c: &mut PgConnection, id: ResearcherId) -> Result<(), StorageError> {
    sqlx::query("UPDATE researchers SET email_verified_at = now() WHERE id = $1")
        .bind(id.0)
        .execute(c)
        .await?;
    Ok(())
}

/// Verified e-mail addresses of researchers, by id.
pub async fn researcher_emails(
    c: &mut PgConnection,
) -> Result<std::collections::HashMap<uuid::Uuid, String>, StorageError> {
    Ok(sqlx::query_as::<_, (uuid::Uuid, String)>(
        "SELECT id, email FROM researchers WHERE email IS NOT NULL",
    )
    .fetch_all(c)
    .await?
    .into_iter()
    .collect())
}
