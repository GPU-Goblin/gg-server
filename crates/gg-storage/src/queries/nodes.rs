//! Lab nodes (ADR-0041): the researcher's machines that hold, serve and aggregate their runs'
//! artifacts.

use gg_core::{NodeId, ProjectId, ResearcherId};
use sqlx::PgConnection;

use crate::StorageError;
use crate::models::LabNodeRow;

const COLS: &str =
    "id, researcher_id, public_key, display_name, created_at, revoked_at, last_seen_at";

/// Register a node key for a researcher. A key already registered (to anyone) is refused.
pub async fn insert(
    c: &mut PgConnection,
    id: NodeId,
    researcher: ResearcherId,
    public_key: &[u8; 32],
    display_name: &str,
) -> Result<(), StorageError> {
    sqlx::query(
        "INSERT INTO lab_nodes (id, researcher_id, public_key, display_name) VALUES ($1, $2, $3, $4)",
    )
    .bind(id.0)
    .bind(researcher.0)
    .bind(public_key.as_slice())
    .bind(display_name)
    .execute(c)
    .await?;
    Ok(())
}

/// Node by key.
pub async fn get_by_key(
    c: &mut PgConnection,
    public_key: &[u8; 32],
) -> Result<Option<LabNodeRow>, StorageError> {
    Ok(sqlx::query_as::<_, LabNodeRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM lab_nodes WHERE public_key = $1"
    )))
    .bind(public_key.as_slice())
    .fetch_optional(c)
    .await?)
}

/// Node by id.
pub async fn get(c: &mut PgConnection, id: NodeId) -> Result<Option<LabNodeRow>, StorageError> {
    Ok(sqlx::query_as::<_, LabNodeRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM lab_nodes WHERE id = $1"
    )))
    .bind(id.0)
    .fetch_optional(c)
    .await?)
}

/// A researcher's nodes, newest first.
pub async fn list_of(
    c: &mut PgConnection,
    researcher: ResearcherId,
) -> Result<Vec<LabNodeRow>, StorageError> {
    Ok(sqlx::query_as::<_, LabNodeRow>(sqlx::AssertSqlSafe(format!(
        "SELECT {COLS} FROM lab_nodes WHERE researcher_id = $1 ORDER BY created_at DESC"
    )))
    .bind(researcher.0)
    .fetch_all(c)
    .await?)
}

/// Revoke a researcher's node: its sessions end and it cannot sign in again. Returns whether it
/// was active.
pub async fn revoke(
    c: &mut PgConnection,
    researcher: ResearcherId,
    id: NodeId,
) -> Result<bool, StorageError> {
    let r = sqlx::query(
        "UPDATE lab_nodes SET revoked_at = now()
         WHERE id = $1 AND researcher_id = $2 AND revoked_at IS NULL",
    )
    .bind(id.0)
    .bind(researcher.0)
    .execute(&mut *c)
    .await?;
    sqlx::query(
        "UPDATE sessions SET revoked_at = now()
         WHERE principal_kind = 'node' AND principal_id = $1 AND revoked_at IS NULL",
    )
    .bind(id.0)
    .execute(&mut *c)
    .await?;
    if r.rows_affected() == 1 {
        // Readers stop being sent to it at once.
        sqlx::query("DELETE FROM node_peers WHERE node_id = $1")
            .bind(id.0)
            .execute(&mut *c)
            .await?;
    }
    Ok(r.rows_affected() == 1)
}

/// Record that a node signed in.
pub async fn touch(c: &mut PgConnection, id: NodeId) -> Result<(), StorageError> {
    sqlx::query("UPDATE lab_nodes SET last_seen_at = now() WHERE id = $1")
        .bind(id.0)
        .execute(c)
        .await?;
    Ok(())
}

/// Allow `researcher` to use a node (to attach it to their projects).
pub async fn grant(
    c: &mut PgConnection,
    id: NodeId,
    researcher: ResearcherId,
) -> Result<(), StorageError> {
    sqlx::query(
        "INSERT INTO node_users (node_id, researcher_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
    )
    .bind(id.0)
    .bind(researcher.0)
    .execute(c)
    .await?;
    Ok(())
}

/// Withdraw a researcher's use of a node. The projects they attached stay attached until a member
/// detaches them. Returns whether the researcher was allowed.
pub async fn ungrant(
    c: &mut PgConnection,
    id: NodeId,
    researcher: ResearcherId,
) -> Result<bool, StorageError> {
    Ok(
        sqlx::query("DELETE FROM node_users WHERE node_id = $1 AND researcher_id = $2")
            .bind(id.0)
            .bind(researcher.0)
            .execute(c)
            .await?
            .rows_affected()
            == 1,
    )
}

/// Whether `researcher` may use the node: its owner, or granted by it.
pub async fn may_use(
    c: &mut PgConnection,
    id: NodeId,
    researcher: ResearcherId,
) -> Result<bool, StorageError> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM lab_nodes WHERE id = $1 AND researcher_id = $2)
             OR EXISTS (SELECT 1 FROM node_users WHERE node_id = $1 AND researcher_id = $2)",
    )
    .bind(id.0)
    .bind(researcher.0)
    .fetch_one(c)
    .await?)
}

/// Researchers granted the use of a node (its owner aside).
pub async fn users(c: &mut PgConnection, id: NodeId) -> Result<Vec<ResearcherId>, StorageError> {
    Ok(sqlx::query_scalar::<_, uuid::Uuid>(
        "SELECT researcher_id FROM node_users WHERE node_id = $1 ORDER BY granted_at",
    )
    .bind(id.0)
    .fetch_all(c)
    .await?
    .into_iter()
    .map(ResearcherId)
    .collect())
}

/// Nodes a researcher owns or may use, not revoked, most recent first.
pub async fn usable_by(
    c: &mut PgConnection,
    researcher: ResearcherId,
) -> Result<Vec<LabNodeRow>, StorageError> {
    Ok(sqlx::query_as::<_, LabNodeRow>(
        "SELECT n.id, n.researcher_id, n.public_key, n.display_name, n.created_at, n.revoked_at, n.last_seen_at
           FROM lab_nodes n
          WHERE n.researcher_id = $1
             OR EXISTS (SELECT 1 FROM node_users u WHERE u.node_id = n.id AND u.researcher_id = $1)
          ORDER BY n.created_at DESC",
    )
    .bind(researcher.0)
    .fetch_all(c)
    .await?)
}

/// Attach a node to a project: it may then hold the project's artifacts and step its runs.
pub async fn attach(
    c: &mut PgConnection,
    id: NodeId,
    project: ProjectId,
    by: ResearcherId,
) -> Result<bool, StorageError> {
    Ok(sqlx::query(
        "INSERT INTO node_projects (node_id, project_id, attached_by) VALUES ($1, $2, $3)
         ON CONFLICT DO NOTHING",
    )
    .bind(id.0)
    .bind(project.0)
    .bind(by.0)
    .execute(c)
    .await?
    .rows_affected()
        == 1)
}

/// Detach a node from a project. Returns whether it was attached.
pub async fn detach(
    c: &mut PgConnection,
    id: NodeId,
    project: ProjectId,
) -> Result<bool, StorageError> {
    Ok(
        sqlx::query("DELETE FROM node_projects WHERE node_id = $1 AND project_id = $2")
            .bind(id.0)
            .bind(project.0)
            .execute(c)
            .await?
            .rows_affected()
            == 1,
    )
}

/// Whether a node is attached to a project.
pub async fn attached(
    c: &mut PgConnection,
    id: NodeId,
    project: ProjectId,
) -> Result<bool, StorageError> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM node_projects WHERE node_id = $1 AND project_id = $2)",
    )
    .bind(id.0)
    .bind(project.0)
    .fetch_one(c)
    .await?)
}

/// Projects a node is attached to.
pub async fn projects_of(c: &mut PgConnection, id: NodeId) -> Result<Vec<ProjectId>, StorageError> {
    Ok(sqlx::query_scalar::<_, uuid::Uuid>(
        "SELECT project_id FROM node_projects WHERE node_id = $1 ORDER BY attached_at",
    )
    .bind(id.0)
    .fetch_all(c)
    .await?
    .into_iter()
    .map(ProjectId)
    .collect())
}

/// Nodes attached to a project.
pub async fn nodes_of_project(
    c: &mut PgConnection,
    project: ProjectId,
) -> Result<Vec<LabNodeRow>, StorageError> {
    Ok(sqlx::query_as::<_, LabNodeRow>(
        "SELECT n.id, n.researcher_id, n.public_key, n.display_name, n.created_at, n.revoked_at, n.last_seen_at
           FROM lab_nodes n JOIN node_projects p ON p.node_id = n.id
          WHERE p.project_id = $1 ORDER BY p.attached_at",
    )
    .bind(project.0)
    .fetch_all(c)
    .await?)
}

/// What a node still does for a project: running runs it steps, unsettled jobs whose outputs it
/// pulls. A node cannot be detached while there is any.
pub async fn busy_for_project(
    c: &mut PgConnection,
    id: NodeId,
    project: ProjectId,
) -> Result<i64, StorageError> {
    Ok(sqlx::query_scalar::<_, i64>(
        "SELECT (SELECT count(*) FROM collective_runs
                  WHERE holder_node = $1 AND project_id = $2 AND state = 'running')
              + (SELECT count(*) FROM jobs j
                  WHERE j.output_holder = $1 AND j.project_id = $2
                    AND EXISTS (SELECT 1 FROM tasks t WHERE t.job_id = j.id
                                 AND t.state IN ('queued', 'assigned', 'submitted', 'disputed')))",
    )
    .bind(id.0)
    .bind(project.0)
    .fetch_one(c)
    .await?)
}
