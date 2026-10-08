//! Retention of a collective run's intermediate artifacts (ADR-0040).
//!
//! Every round leaves artifacts behind: the tasks' outputs (gradients, activations), the
//! micro-batch rows the coordinator cut, the new weights and the optimizer moments. For SmolLM2 that
//! is about 2 GB per round, kept forever until now. Once a round is settled for good, what nobody can
//! still need is removed:
//!
//! - a round is examined once it is closed, `retention_after_secs` have passed, and every task is
//!   accepted, failed or cancelled with every verification decided and no dispute open; while a run
//!   goes on, the round that produced its current checkpoint is left whole (its gradients rebuild
//!   the optimizer moments after a restart, ADR-0036);
//! - candidates: the inputs and outputs of its tasks, and what the checkpoint it produced references;
//! - kept: anything a run of the project still names (configuration, current checkpoint), the
//!   weights of the checkpoints kept (step 0, every `checkpoint_keep_every` steps, the current one),
//!   the inputs and outputs of the run's rounds not examined yet, anything a task not yet decided
//!   names, and every artifact a worker or the coordinator did not produce (researchers' uploads);
//! - a removed artifact keeps its row (hash, size, owner) with the time it expired, and a download
//!   says it expired.

use std::collections::BTreeSet;

use gg_core::*;
use gg_protocol::messages::{ResultSubmissionV1, TaskSpecV1};
use gg_protocol::signed::Signed;
use gg_storage::queries::collective as cq;
use gg_storage::queries::{artifacts, jobs, tasks};
use uuid::Uuid;

use super::collective::CheckpointRefs;
use crate::error::ApiError;
use crate::state::AppState;

/// What a pass removed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pruned {
    /// Rounds examined.
    pub rounds: usize,
    /// Artifacts removed.
    pub artifacts: usize,
    /// Bytes freed.
    pub bytes: u64,
}

/// Rounds examined per pass.
const ROUNDS_PER_PASS: i64 = 50;

/// The SHA-256 digests (64 lowercase hex characters) a JSON text mentions: what a configuration or
/// a checkpoint references.
fn digests_in(text: &str, out: &mut BTreeSet<String>) {
    let bytes = text.as_bytes();
    let hex = |b: u8| b.is_ascii_digit() || (b'a'..=b'f').contains(&b);
    let mut i = 0;
    while i < bytes.len() {
        if !hex(bytes[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && hex(bytes[i]) {
            i += 1;
        }
        if i - start == 64 {
            out.insert(text[start..i].to_owned());
        }
    }
}

/// Every job of a round: its own, for a staged round each wave's, and their twin jobs (the other
/// computations of their tasks and their comparers, ADR-0042).
async fn round_jobs(
    c: &mut sqlx::PgConnection,
    round: &cq::CollectiveRoundRow,
) -> Result<Vec<Uuid>, ApiError> {
    let mut out = vec![round.job_id];
    if let Some(v) = cq::stage_state(c, round.id).await?
        && let Ok(st) = serde_json::from_value::<super::staged::StageState>(v)
    {
        for j in st.jobs() {
            if !out.contains(&j) {
                out.push(j);
            }
        }
    }
    for j in gg_storage::queries::replicas::twin_jobs(c, &out).await? {
        if !out.contains(&j) {
            out.push(j);
        }
    }
    Ok(out)
}

/// The inputs of these jobs' tasks and the outputs of every submission they received.
async fn task_digests(
    c: &mut sqlx::PgConnection,
    job_ids: &[Uuid],
    out: &mut BTreeSet<String>,
) -> Result<(), ApiError> {
    for job in job_ids {
        for t in jobs::list_tasks(c, JobId(*job)).await? {
            let spec: TaskSpecV1 = super::decode_spec(&t.spec_cbor)?;
            out.extend(spec.definition.inputs.values().map(|r| r.hash.to_hex()));
            for a in tasks::list_attempts(c, TaskId(t.id)).await? {
                if let Some(sub) = tasks::get_submission_by_attempt(c, AttemptId(a.id)).await? {
                    let signed: Signed<ResultSubmissionV1> =
                        Signed::from_canonical_bytes(&sub.submission_cbor)
                            .map_err(|e| ApiError::Internal(e.to_string()))?;
                    out.extend(signed.body.outputs.values().map(|r| r.hash.to_hex()));
                }
            }
        }
    }
    Ok(())
}

/// One pass: examine the settled rounds due, remove what nobody can still need.
pub async fn prune_once(state: &AppState) -> Result<Pruned, ApiError> {
    let cfg = state.config();
    let mut done = Pruned::default();
    if !cfg.retention {
        return Ok(done);
    }
    let mut c = state
        .db()
        .pool()
        .acquire()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let due = cq::prunable_rounds(&mut c, cfg.retention_after_secs as i64, ROUNDS_PER_PASS).await?;
    let mut runs: Vec<Uuid> = due.iter().map(|r| r.run_id).collect();
    runs.sort();
    runs.dedup();
    for run_id in runs {
        let Some(run) = cq::get_run(&mut c, run_id).await? else {
            continue;
        };
        // While the run goes on, the round that produced its current checkpoint stays whole: its
        // gradients rebuild the optimizer moments after a restart (ADR-0036). Once the run has
        // ended, its last checkpoint records its moments itself.
        let current = if run.state == "running" {
            cq::checkpoint_round(&mut c, run.id, run.step)
                .await?
                .map(|r| r.id)
        } else {
            None
        };
        let mut chosen = Vec::new();
        for round in due.iter().filter(|r| r.run_id == run.id) {
            if Some(round.id) == current {
                continue;
            }
            let job_ids = round_jobs(&mut c, round).await?;
            if tasks::jobs_settled(&mut c, &job_ids).await? {
                chosen.push((round.clone(), job_ids));
            }
        }
        if chosen.is_empty() {
            continue;
        }

        // What must stay.
        let mut live = BTreeSet::new();
        for text in cq::project_run_references(&mut c, run.project_id).await? {
            digests_in(&text, &mut live);
        }
        let keep_every = cfg.checkpoint_keep_every.max(1) as i32;
        let kept: Vec<i32> = cq::checkpoint_steps(&mut c, run.id)
            .await?
            .into_iter()
            .filter(|s| s % keep_every == 0 || *s == run.step)
            .collect();
        for (_, ck) in cq::checkpoints_at(&mut c, run.id, &kept).await? {
            // A kept checkpoint keeps its weights; only the current one keeps its moments (above).
            if let Ok(refs) = serde_json::from_value::<CheckpointRefs>(ck) {
                live.extend(refs.weights.values().map(|r| r.hash.to_hex()));
            }
        }
        let chosen_ids: Vec<Uuid> = chosen.iter().map(|(r, _)| r.id).collect();
        for other in cq::unpruned_rounds(&mut c, run.id).await? {
            if !chosen_ids.contains(&other.id) {
                let job_ids = round_jobs(&mut c, &other).await?;
                task_digests(&mut c, &job_ids, &mut live).await?;
            }
        }

        for (round, job_ids) in chosen {
            let mut candidates = BTreeSet::new();
            task_digests(&mut c, &job_ids, &mut candidates).await?;
            if let Some((_, ck)) = cq::checkpoint_of_round(&mut c, round.id).await? {
                digests_in(&ck.to_string(), &mut candidates);
            }
            for hex in candidates.difference(&live) {
                let Ok(hash) = hex.parse::<Sha256Digest>() else {
                    continue;
                };
                if !artifacts::retention_may_expire(&mut c, &hash).await?
                    || artifacts::named_by_unfinished_task(&mut c, &hash).await?
                {
                    continue;
                }
                let size = state.objects().size(&hash).await.unwrap_or(0);
                // Recorded as expired before the bytes go: a download never finds a live row
                // without its object.
                let mut tx = state.db().begin().await?;
                let expired = artifacts::expire(&mut tx, &hash).await?;
                tx.commit().await?;
                if expired && state.objects().remove(&hash).await? {
                    done.artifacts += 1;
                    done.bytes += size;
                }
            }
            cq::mark_round_pruned(&mut c, round.id).await?;
            done.rounds += 1;
        }
    }
    Ok(done)
}

/// Run the retention policy every `retention_interval_secs` until `shutdown` resolves.
pub async fn run(state: AppState, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    let interval = std::time::Duration::from_secs(state.config().retention_interval_secs.max(1));
    loop {
        tokio::select! {
            _ = tokio::time::sleep(interval) => {
                match prune_once(&state).await {
                    Ok(p) if p.rounds > 0 => tracing::info!(rounds = p.rounds, artifacts = p.artifacts, bytes = p.bytes, "retention pass"),
                    Ok(_) => {}
                    Err(e) => tracing::warn!(error = %e, "retention pass failed"),
                }
            }
            _ = shutdown.changed() => { if *shutdown.borrow() { break; } }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::digests_in;

    #[test]
    fn only_whole_64_hex_runs_are_digests() {
        let a = "a".repeat(64);
        let b = "0123456789abcdef".repeat(4);
        let text = format!(
            r#"{{"hash":"{a}","other":"{b}","long":"{a}0","upper":"{}","short":"abc"}}"#,
            "A".repeat(64)
        );
        let mut out = std::collections::BTreeSet::new();
        digests_in(&text, &mut out);
        assert_eq!(out.into_iter().collect::<Vec<_>>(), vec![b, a]);
    }
}
