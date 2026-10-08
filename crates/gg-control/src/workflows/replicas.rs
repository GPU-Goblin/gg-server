//! Verification by other contributors (ADR-0042): every task is computed twice, by two workers of
//! different contributor accounts, and the two results are compared.
//!
//! - **The replica** is a task of its own in a twin job (`replica_of`), with the same definition:
//!   the assignment carries no role and the spec differs only by its identifiers, so a worker
//!   cannot tell a replica from a first computation. It is created when the first computation is
//!   assigned, so both run at once.
//! - **Pairing** is the server's: of the computations of one task, a contributor account (a worker
//!   without one is its own) takes one only; the replica is offered first, for
//!   `pairing_reserve_secs`, to one worker drawn with a server-side secret among the workers of
//!   other accounts seen recently that can run it. An account with *k* of *N* such workers is drawn
//!   about *k/N* of the time and cannot choose.
//! - **Commitment**: a computation never reads another's outputs (they are not its inputs), and
//!   both are submitted, hashes signed, before they are compared.
//! - **Comparison** ([`gg_compute::replica::compare_replicas`]): the two outputs within the task's
//!   tolerance, both ways; signed ([`ComparisonV1`]) by the comparer, replayable by anyone holding
//!   both outputs. The server compares what it stores; a lab node compares what it holds.
//! - **Decision**: two that agree are both accepted (the first paid as primary, the replica as
//!   replica). Two that differ call a third computation, by a third account; the majority within
//!   tolerance is accepted, the odd one rejected (not paid); with no majority, a dispute opens for
//!   a human for each computation, and nothing is paid until it is settled. Once a computation of a
//!   task is accepted, a later one (a retry) is compared with it alone.

use std::collections::BTreeMap;
use std::path::PathBuf;

use gg_core::*;
use gg_protocol::canonical;
use gg_protocol::domain::Domain;
use gg_protocol::messages::{ComparedV1, ComparisonV1, ResultSubmissionV1, TaskSpecV1};
use gg_protocol::signed::Signed;
use gg_storage::models::{AttemptRow, SubmissionRow, TaskRow};
use gg_storage::queries::replicas::{self as rq, NewComparison};
use gg_storage::queries::{jobs, misc, tasks};
use uuid::Uuid;

use super::{decode_spec, now_ms};
use crate::error::ApiError;
use crate::state::AppState;

/// Workers seen this recently may be drawn for a replica.
const DRAW_WITHIN_SECS: i64 = 120;

/// Draw tag of the pairing (independent of the audit draws).
const PAIRING_TAG: &[u8] = b"gg/pairing/v1";

/// The worker drawn for computation `task` among `candidates`: the smallest keyed hash.
fn draw(secret: &[u8], task: TaskId, candidates: &[WorkerId]) -> Option<WorkerId> {
    candidates.iter().copied().min_by_key(|w| {
        let mut h = Sha256Digest::hasher();
        h.update(PAIRING_TAG);
        h.update(secret);
        h.update(task.0.as_bytes());
        h.update(w.0.as_bytes());
        h.finalize()
    })
}

/// After `task` (a first computation) is assigned: create its replica, if replicated
/// verification is on and it has none yet.
pub(crate) async fn ensure_replica(
    state: &AppState,
    tx: &mut gg_storage::PgConnection,
    task: &TaskRow,
    spec: &TaskSpecV1,
    claimer: WorkerId,
) -> Result<(), ApiError> {
    // A comparer's verdict is checked by the node or a second comparer, not replicated.
    if !state.config().replica_verification || task.compare_family.is_some() {
        return Ok(());
    }
    let family = task.replica_of.map_or(TaskId(task.id), TaskId);
    let members = rq::members(tx, family).await?;
    let root = members
        .iter()
        .find(|m| m.id == family.0)
        .cloned()
        .ok_or_else(|| ApiError::Internal("family without its first computation".into()))?;
    let root_spec = decode_spec(&root.spec_cbor)?;
    if task.replica_of.is_none() && members.len() == 1 {
        spawn(state, tx, task, spec, 1).await?;
    }
    // A spot check: drawn for a sample of the tasks, and for every task of an account in
    // probation. The third computation starts at once.
    let drawn = spot_drawn(state.audit_secret(), family, state.config().spot_check_rate);
    let account = rq::account_of(tx, claimer).await?;
    let probation = (rq::accepted_of_account(tx, account).await? as u64)
        < state.config().probation_tasks as u64;
    let members = rq::members(tx, family).await?;
    if (drawn || probation) && !members.iter().any(|m| m.replica_seq == 2) && members.len() < 3 {
        rq::set_spot_check(tx, family).await?;
        spawn(state, tx, &root, &root_spec, 2).await?;
        tracing::info!(%family, drawn, probation, "spot check: a third computation from the start");
    }
    Ok(())
}

/// Draw tag of the spot checks (independent of the pairing and the audits).
const SPOT_TAG: &[u8] = b"gg/spot-check/v1";

/// Whether `family` is spot-checked at `rate`: a draw from the server's secret, which the workers
/// cannot see.
fn spot_drawn(secret: &[u8], family: TaskId, rate: f64) -> bool {
    if rate.is_nan() || rate <= 0.0 {
        return false;
    }
    if rate >= 1.0 {
        return true;
    }
    let mut h = Sha256Digest::hasher();
    h.update(SPOT_TAG);
    h.update(secret);
    h.update(family.0.as_bytes());
    let d = h.finalize();
    let draw = u64::from_be_bytes(d.as_bytes()[..8].try_into().expect("32-byte digest"));
    (draw as f64) / 18_446_744_073_709_551_616.0 < rate
}

/// Whether `task` is one of several computations of a task, decided by comparing them.
pub(crate) async fn replicated(
    tx: &mut gg_storage::PgConnection,
    task: &TaskRow,
) -> Result<bool, ApiError> {
    // A comparer's verdict is decided by agreement with the node or a second comparer, too.
    Ok(task.replica_of.is_some()
        || task.compare_family.is_some()
        || rq::members(tx, TaskId(task.id)).await?.len() > 1)
}

/// Create computation `seq` of `family` (its first computation's task and spec), in the twin job,
/// offered first to a worker drawn among the other accounts.
async fn spawn(
    state: &AppState,
    tx: &mut gg_storage::PgConnection,
    family: &TaskRow,
    spec: &TaskSpecV1,
    seq: i16,
) -> Result<TaskId, ApiError> {
    let twin_job = match rq::replica_job(tx, family.job_id, seq).await? {
        Some(j) => j,
        None => {
            let j = JobId::new();
            jobs::create_job(
                tx,
                j,
                ProjectId(family.project_id),
                ExperimentId(family.experiment_id),
                None,
                &format!("replicas-{seq}-of-{}", family.job_id),
            )
            .await?;
            rq::set_replica_job(tx, j.0, family.job_id, seq, true).await?;
            j.0
        }
    };
    let task_id = TaskId::new();
    let twin = TaskSpecV1 {
        job_id: JobId(twin_job),
        task_id,
        ..spec.clone()
    };
    let def = &twin.definition;
    let spec_cbor = canonical::to_bytes(&twin)?;
    let spec_hash = gg_protocol::domain::hash_bytes(state.network(), Domain::TaskSpec, &spec_cbor);
    let definition_hash =
        gg_protocol::domain::hash_message(state.network(), Domain::TaskDefinition, def)?;
    let checked = gg_compute::validate_definition(def)?;
    let requirements = rq::requirements(tx, TaskId(family.id)).await?;
    jobs::insert_task(
        tx,
        &jobs::NewTask {
            id: task_id,
            job_id: JobId(twin_job),
            project_id: ProjectId(family.project_id),
            experiment_id: ExperimentId(family.experiment_id),
            index: family.index_in_job,
            compute_kind: &family.compute_kind,
            spec_hash,
            definition_hash,
            spec_cbor: &spec_cbor,
            max_attempts: family.max_attempts,
            priority: family.priority,
            reward_primary: def.reward_schedule.primary,
            verification_cost: def.reward_schedule.verification_cost,
            est_cpu_secs: checked.estimate.est_cpu_secs,
            requirements,
        },
    )
    .await?;
    let excluded = rq::family_accounts(tx, TaskId(family.id)).await?;
    let need = rq::Need {
        kind: &family.compute_kind,
        requirements,
        serves_peers: jobs::output_holder(tx, family.job_id).await?.is_some(),
    };
    let candidates = rq::candidates(tx, &need, &excluded, DRAW_WITHIN_SECS).await?;
    let designated = draw(state.audit_secret(), task_id, &candidates);
    rq::mark_replica(
        tx,
        task_id,
        TaskId(family.id),
        seq,
        designated,
        state.config().pairing_reserve_secs as i64,
    )
    .await?;
    misc::outbox_insert(
        tx,
        "replica.created",
        &serde_json::json!({ "family": family.id, "task_id": task_id, "seq": seq, "drawn_among": candidates.len(), "designated": designated }),
    )
    .await?;
    Ok(task_id)
}

/// One computation of a family, as far as the decision is concerned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Member {
    pub seq: i16,
    /// Its current submission, not decided yet.
    pub submitted: Option<Uuid>,
    /// Its accepted attempt.
    pub accepted: Option<Uuid>,
}

/// What to do with a family.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    /// Decide this attempt (of the member at this index), on these comparisons.
    Decide(usize, Uuid, VerificationOutcome, Vec<Uuid>),
    /// Two computations differ: create the third.
    SpawnThird,
    /// Three computations and no majority: disputes for a human, on these comparisons.
    Dispute(Vec<(usize, Uuid)>, Vec<Uuid>),
    /// A pair that agreed, contradicted by the spot check: collusion suspected. The pair's accounts
    /// are suspended pending a human, and every computation goes to a dispute.
    Collusion(Vec<(usize, Uuid)>, Vec<(usize, Uuid)>, Vec<Uuid>),
}

/// A recorded comparison: its id, the two attempts, the outcome.
pub(crate) type Compared = (Uuid, Uuid, Uuid, VerificationOutcome);

/// The pairs still to compare, and what can be decided from the comparisons recorded.
pub(crate) fn plan(
    members: &[Member],
    compared: &[Compared],
    spot: bool,
) -> (Vec<(Uuid, Uuid)>, Vec<Action>) {
    let find = |x: Uuid, y: Uuid| {
        compared
            .iter()
            .find(|(_, a, b, _)| (*a == x && *b == y) || (*a == y && *b == x))
    };
    let mut todo = Vec::new();
    let mut actions = Vec::new();
    // A computation of the task was accepted: it is the reference for any later one.
    if let Some(reference) = members.iter().find_map(|m| m.accepted) {
        for (i, m) in members.iter().enumerate() {
            let Some(s) = m.submitted else { continue };
            match find(s, reference) {
                Some((id, _, _, outcome)) => {
                    actions.push(Action::Decide(i, s, *outcome, vec![*id]));
                }
                None => todo.push((s, reference)),
            }
        }
        return (todo, actions);
    }
    let subs: Vec<(usize, Uuid)> = members
        .iter()
        .enumerate()
        .filter_map(|(i, m)| m.submitted.map(|s| (i, s)))
        .collect();
    if subs.len() < 2 {
        return (todo, actions);
    }
    let mut pending = false;
    for (k, (_, x)) in subs.iter().enumerate() {
        for (_, y) in &subs[k + 1..] {
            if find(*x, *y).is_none() {
                todo.push((*x, *y));
                pending = true;
            }
        }
    }
    // A spot check: nothing is decided before every computation is compared with every other.
    if spot {
        if subs.len() < members.len() || pending {
            return (todo, actions);
        }
        let of = |seq: i16| {
            members
                .iter()
                .enumerate()
                .find(|(_, m)| m.seq == seq)
                .and_then(|(i, m)| m.submitted.map(|s| (i, s)))
        };
        if let (Some(p), Some(r), Some(c)) = (of(0), of(1), of(2)) {
            let outcome = |x: Uuid, y: Uuid| find(x, y).map(|c| c.3);
            if outcome(p.1, r.1) == Some(VerificationOutcome::Accepted)
                && outcome(p.1, c.1) == Some(VerificationOutcome::Rejected)
                && outcome(r.1, c.1) == Some(VerificationOutcome::Rejected)
            {
                let all = compared.iter().map(|c| c.0).collect();
                actions.push(Action::Collusion(vec![p, r], vec![c], all));
                return (todo, actions);
            }
        }
    }
    // Agreement: every computation that agrees with another is accepted; one that differs from a
    // computation of an agreeing pair is outvoted, without waiting for its other comparisons.
    let agrees = |x: Uuid| {
        subs.iter()
            .filter(|(_, y)| *y != x)
            .filter_map(|(_, y)| find(x, *y))
            .filter(|c| c.3 == VerificationOutcome::Accepted)
            .map(|c| c.0)
            .collect::<Vec<_>>()
    };
    let majority: Vec<Uuid> = subs
        .iter()
        .filter(|(_, x)| !agrees(*x).is_empty())
        .map(|(_, x)| *x)
        .collect();
    if !majority.is_empty() {
        for (i, x) in &subs {
            let with = agrees(*x);
            if !with.is_empty() {
                actions.push(Action::Decide(*i, *x, VerificationOutcome::Accepted, with));
                continue;
            }
            // Rejected, not paid, when it differs from a computation of the majority.
            let against: Vec<Uuid> = subs
                .iter()
                .filter(|(_, y)| y != x)
                .filter_map(|(_, y)| find(*x, *y))
                .filter(|c| c.3 == VerificationOutcome::Rejected)
                .map(|c| c.0)
                .collect();
            let outvoted = majority
                .iter()
                .any(|m| find(*x, *m).is_some_and(|c| c.3 == VerificationOutcome::Rejected));
            if outvoted {
                actions.push(Action::Decide(
                    *i,
                    *x,
                    VerificationOutcome::Rejected,
                    against,
                ));
            }
        }
        return (todo, actions);
    }
    if pending {
        return (todo, actions);
    }
    let all: Vec<Uuid> = compared.iter().map(|c| c.0).collect();
    // Could not be compared (a profile not admitted, an output missing): computed again.
    let inconclusive = subs.iter().any(|(_, x)| {
        subs.iter()
            .filter(|(_, y)| y != x)
            .any(|(_, y)| find(*x, *y).is_some_and(|c| c.3 == VerificationOutcome::Inconclusive))
    });
    if inconclusive {
        for (i, x) in &subs {
            actions.push(Action::Decide(
                *i,
                *x,
                VerificationOutcome::Inconclusive,
                all.clone(),
            ));
        }
        return (todo, actions);
    }
    // Every pair differs.
    if members.len() < 3 {
        actions.push(Action::SpawnThird);
    } else if subs.len() >= 3 {
        actions.push(Action::Dispute(subs.clone(), all));
    }
    (todo, actions)
}

/// A computation with its current submission (or accepted one).
struct Loaded {
    task: TaskRow,
    submitted: Option<(AttemptRow, SubmissionRow)>,
    accepted: Option<(AttemptRow, SubmissionRow)>,
}

async fn load(c: &mut gg_storage::PgConnection, family: TaskId) -> Result<Vec<Loaded>, ApiError> {
    let mut out = Vec::new();
    for t in rq::members(c, family).await? {
        let attempts = tasks::list_attempts(c, TaskId(t.id)).await?;
        let mut submitted = None;
        let mut accepted = None;
        for a in attempts {
            let slot = match a.state.as_str() {
                "submitted" if a.generation == t.generation && t.state == "submitted" => {
                    &mut submitted
                }
                "accepted" => &mut accepted,
                _ => continue,
            };
            if let Some(sub) = tasks::get_submission_by_attempt(c, AttemptId(a.id)).await? {
                *slot = Some((a, sub));
            }
        }
        out.push(Loaded {
            task: t,
            submitted,
            accepted,
        });
    }
    Ok(out)
}

fn members_of(loaded: &[Loaded]) -> Vec<Member> {
    loaded
        .iter()
        .map(|l| Member {
            seq: l.task.replica_seq,
            submitted: l.submitted.as_ref().map(|(a, _)| a.id),
            accepted: l.accepted.as_ref().map(|(a, _)| a.id),
        })
        .collect()
}

/// The comparisons that count: a node's finding of a difference counts once the server replayed it.
fn compared_of(rows: &[rq::ComparisonRow]) -> Result<Vec<Compared>, ApiError> {
    let mut out = Vec::new();
    for r in rows {
        if let Some(o) = r.counting()? {
            out.push((r.id, r.attempt_a, r.attempt_b, o));
        }
    }
    Ok(out)
}

/// The attempt `id` among the loaded computations, with its task and submission.
fn find_attempt(loaded: &[Loaded], id: Uuid) -> Option<(&TaskRow, &AttemptRow, &SubmissionRow)> {
    loaded.iter().find_map(|l| {
        [&l.submitted, &l.accepted]
            .into_iter()
            .flatten()
            .find(|(a, _)| a.id == id)
            .map(|(a, s)| (&l.task, a, s))
    })
}

/// Compare two computations whose outputs the server stores, and record the signed comparison.
/// Returns `false` when it does not compare them: a lab node holds the job's outputs (the node
/// compares, contributors check it: the server reads none of them), or they are not stored yet.
async fn compare_on_server(
    state: &AppState,
    family: &TaskRow,
    x: (&TaskRow, &AttemptRow, &SubmissionRow),
    y: (&TaskRow, &AttemptRow, &SubmissionRow),
) -> Result<bool, ApiError> {
    {
        let mut c = state.db().pool().acquire().await?;
        if jobs::output_holder(&mut c, family.job_id).await?.is_some() {
            return Ok(false);
        }
    }
    let spec = decode_spec(&family.spec_cbor)?;
    let task = gg_compute::validate_definition(&spec.definition)?;
    let mut sides = Vec::with_capacity(2);
    for (t, a, s) in [x, y] {
        let signed: Signed<ResultSubmissionV1> = Signed::from_canonical_bytes(&s.submission_cbor)
            .map_err(|e| ApiError::Internal(e.to_string()))?;
        let mut files = BTreeMap::new();
        for (name, r) in &signed.body.outputs {
            if !state.objects().exists(&r.hash) {
                return Ok(false);
            }
            files.insert(name.clone(), state.objects().path_for(&r.hash));
        }
        let side = ComparedV1 {
            task_id: TaskId(t.id),
            attempt_id: AttemptId(a.id),
            submission_hash: s.digest().map_err(|e| ApiError::Internal(e.to_string()))?,
        };
        sides.push((side, signed.body, files));
    }
    let (sb, bb, fb) = sides.pop().expect("two sides");
    let (sa, ba, fa) = sides.pop().expect("two sides");
    let verdict = tokio::task::spawn_blocking(move || {
        let files_a: BTreeMap<String, PathBuf> = fa;
        let files_b: BTreeMap<String, PathBuf> = fb;
        gg_compute::replica::compare_replicas(
            &task,
            &gg_compute::replica::Side {
                outputs: &ba.outputs,
                files: &files_a,
                profile: &ba.execution_profile.numeric_profile,
            },
            &gg_compute::replica::Side {
                outputs: &bb.outputs,
                files: &files_b,
                profile: &bb.execution_profile.numeric_profile,
            },
        )
    })
    .await
    .map_err(|e| ApiError::Internal(e.to_string()))??;
    let body = ComparisonV1 {
        protocol_version: gg_protocol::PROTOCOL_VERSION,
        network: state.network().clone(),
        family: TaskId(family.id),
        a: sa,
        b: sb,
        outcome: verdict.outcome,
        checks: verdict.checks,
        compared_at: now_ms(),
    };
    let signed = Signed::sign(
        state.server_key(),
        state.network(),
        Domain::Comparison,
        body,
    )?;
    record(state, "server", signed).await?;
    Ok(true)
}

/// A task's current submission, if it has one not decided yet.
async fn current(
    c: &mut gg_storage::PgConnection,
    t: &TaskRow,
) -> Result<Option<(AttemptRow, SubmissionRow)>, ApiError> {
    if t.state != "submitted" {
        return Ok(None);
    }
    for a in tasks::list_attempts(c, TaskId(t.id)).await? {
        if a.state == "submitted" && a.generation == t.generation {
            return Ok(tasks::get_submission_by_attempt(c, AttemptId(a.id))
                .await?
                .map(|s| (a, s)));
        }
    }
    Ok(None)
}

/// A comparer's task, its current submission, and the verdict it wrote (read from the server's
/// store: a verdict is a few kilobytes, uploaded like any small output).
struct Comparer {
    task: TaskRow,
    attempt: AttemptRow,
    sub: SubmissionRow,
    verdict: VerificationOutcome,
}

async fn comparer_of(
    state: &AppState,
    c: &mut gg_storage::PgConnection,
    t: &TaskRow,
) -> Result<Option<Comparer>, ApiError> {
    let Some((attempt, sub)) = current(c, t).await? else {
        return Ok(None);
    };
    let signed: Signed<ResultSubmissionV1> = Signed::from_canonical_bytes(&sub.submission_cbor)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let verdict = match signed.body.outputs.get(gg_compute::replica::VERDICT) {
        Some(r) if state.objects().exists(&r.hash) => {
            let bytes = state.objects().read_bytes(&r.hash, r.size).await?;
            gg_compute::replica::read_verdict(&bytes)
                .map(|v| v.outcome)
                .unwrap_or(VerificationOutcome::Inconclusive)
        }
        Some(_) => return Ok(None),
        None => VerificationOutcome::Inconclusive,
    };
    Ok(Some(Comparer {
        task: t.clone(),
        attempt,
        sub,
        verdict,
    }))
}

/// Create a comparer of computations `x` and `y` of `family` (`replica.compare.v1`), offered first
/// to a worker drawn among the accounts that took no part in the task.
async fn spawn_comparer(
    state: &AppState,
    tx: &mut gg_storage::PgConnection,
    family: &TaskRow,
    x: (&AttemptRow, &SubmissionRow),
    y: (&AttemptRow, &SubmissionRow),
) -> Result<TaskId, ApiError> {
    const RANK: i16 = 3;
    let compared = decode_spec(&family.spec_cbor)?.definition;
    let outs = |s: &SubmissionRow| -> Result<
        (BTreeMap<String, gg_protocol::messages::ArtifactRef>, String),
        ApiError,
    > {
        let signed: Signed<ResultSubmissionV1> = Signed::from_canonical_bytes(&s.submission_cbor)
            .map_err(|e| ApiError::Internal(e.to_string()))?;
        Ok((
            signed.body.outputs,
            signed.body.execution_profile.numeric_profile,
        ))
    };
    let (ox, px) = outs(x.1)?;
    let (oy, py) = outs(y.1)?;
    let def = gg_compute::catalog::replica_compare_definition(&compared, (&ox, &px), (&oy, &py))?;
    let job = match rq::replica_job(tx, family.job_id, RANK).await? {
        Some(j) => j,
        None => {
            let j = JobId::new();
            jobs::create_job(
                tx,
                j,
                ProjectId(family.project_id),
                ExperimentId(family.experiment_id),
                None,
                &format!("comparers-of-{}", family.job_id),
            )
            .await?;
            // Its verdicts are small: uploaded, not pulled by the lab node.
            rq::set_replica_job(tx, j.0, family.job_id, RANK, false).await?;
            j.0
        }
    };
    let task_id = TaskId::new();
    let index = rq::next_index(tx, job).await?;
    let spec = TaskSpecV1 {
        spec_version: 1,
        network: state.network().clone(),
        project_id: ProjectId(family.project_id),
        experiment_id: ExperimentId(family.experiment_id),
        job_id: JobId(job),
        task_id,
        index: index as u32,
        definition: def.clone(),
    };
    let spec_cbor = canonical::to_bytes(&spec)?;
    let spec_hash = gg_protocol::domain::hash_bytes(state.network(), Domain::TaskSpec, &spec_cbor);
    let definition_hash =
        gg_protocol::domain::hash_message(state.network(), Domain::TaskDefinition, &def)?;
    let checked = gg_compute::validate_definition(&def)?;
    let requirements = jobs::TaskRequirements {
        ram_bytes: def.resource_limits.max_ram_bytes as i64,
        ..Default::default()
    };
    jobs::insert_task(
        tx,
        &jobs::NewTask {
            id: task_id,
            job_id: JobId(job),
            project_id: ProjectId(family.project_id),
            experiment_id: ExperimentId(family.experiment_id),
            index,
            compute_kind: "replica.compare.v1",
            spec_hash,
            definition_hash,
            spec_cbor: &spec_cbor,
            max_attempts: def.max_attempts as i32,
            priority: family.priority,
            reward_primary: def.reward_schedule.primary,
            verification_cost: def.reward_schedule.verification_cost,
            est_cpu_secs: checked.estimate.est_cpu_secs,
            requirements,
        },
    )
    .await?;
    let excluded = rq::family_accounts(tx, TaskId(family.id)).await?;
    let need = rq::Need {
        kind: "replica.compare.v1",
        requirements,
        serves_peers: false,
    };
    let candidates = rq::candidates(tx, &need, &excluded, DRAW_WITHIN_SECS).await?;
    let designated = draw(state.audit_secret(), task_id, &candidates);
    rq::mark_comparer(
        tx,
        task_id,
        TaskId(family.id),
        (AttemptId(x.0.id), AttemptId(y.0.id)),
        designated,
        state.config().pairing_reserve_secs as i64,
    )
    .await?;
    misc::outbox_insert(
        tx,
        "comparer.created",
        &serde_json::json!({ "family": family.id, "task_id": task_id, "a": x.0.id, "b": y.0.id, "designated": designated }),
    )
    .await?;
    Ok(task_id)
}

/// Suspend the account of `worker` pending a human (ADR-0042, owner's decision of 2026-09-26): its
/// contributor account, or the worker alone when it has none.
async fn suspend_account(
    tx: &mut gg_storage::PgConnection,
    worker: WorkerId,
    reason: &str,
) -> Result<(), ApiError> {
    let account = rq::account_of(tx, worker).await?;
    match (account != worker.0).then_some(account) {
        Some(account) => {
            super::contributors::set_suspended_in(tx, "server", account, true, reason).await?;
        }
        None => {
            gg_storage::queries::identities::set_worker_authorized(tx, worker, false).await?;
            gg_storage::queries::identities::revoke_sessions(tx, "worker", worker.0).await?;
            gg_storage::queries::p2p::delete_peers_of_worker(tx, worker).await?;
            misc::audit(
                tx,
                "server",
                "worker.suspend",
                &worker.to_string(),
                serde_json::json!({ "reason": reason }),
            )
            .await?;
        }
    }
    tracing::warn!(%worker, reason, "account suspended pending a human");
    Ok(())
}

/// Accept or reject a comparer's computation.
async fn decide_comparer(
    state: &AppState,
    tx: &mut gg_storage::PgConnection,
    c: &Comparer,
    outcome: VerificationOutcome,
    via: Sha256Digest,
) -> Result<(), ApiError> {
    let task = jobs::lock_task(tx, TaskId(c.task.id))
        .await?
        .ok_or_else(|| ApiError::Internal("task missing".into()))?;
    super::decide::finalize(
        state,
        tx,
        &task,
        &c.attempt,
        &c.sub,
        outcome,
        via,
        &[via],
        TaskEvent::Decide(outcome),
        Authority::Server,
    )
    .await?;
    Ok(())
}

fn sub_digest(s: &SubmissionRow) -> Result<Sha256Digest, ApiError> {
    s.digest().map_err(|e| ApiError::Internal(e.to_string()))
}

/// The contributors' check of what a lab node compared, for the pairs of `family` still to decide
/// (under the family's lock). A difference the node reports counts once a comparer drawn by the
/// server finds the same; if the comparer finds otherwise, a second one settles it: the side two
/// agree on counts, and whoever misreported (the node, or the first comparer's account) is
/// suspended pending a human. The computations of a suspended node's jobs are compared by a
/// comparer alone. Returns whether anything changed.
async fn check_comparisons(
    state: &AppState,
    tx: &mut gg_storage::PgConnection,
    root: &TaskRow,
    loaded: &[Loaded],
    rows: &[rq::ComparisonRow],
) -> Result<bool, ApiError> {
    let Some(node) = jobs::output_holder(tx, root.job_id).await? else {
        return Ok(false);
    };
    let node_works = !rq::node_suspended(tx, node).await?;
    let spot = rq::spot_checked(tx, TaskId(root.id)).await?;
    let (todo, _) = plan(&members_of(loaded), &compared_of(rows)?, spot);
    let mut changed = false;
    for (x, y) in todo {
        let (Some((_, ax, sx)), Some((_, ay, sy))) =
            (find_attempt(loaded, x), find_attempt(loaded, y))
        else {
            continue;
        };
        let row = rows.iter().find(|r| r.of(x, y));
        let claimed = match row {
            // A node's finding of a difference, not checked yet.
            Some(r) if r.by_node() && r.replayed_outcome.is_none() => Some(r.outcome()?),
            Some(_) => continue,
            // Nobody compared them: the node will, unless it is suspended.
            None if node_works => continue,
            None => None,
        };
        let tasks_of = rq::comparers(tx, x, y).await?;
        let mut cs = Vec::new();
        for t in &tasks_of {
            cs.push(comparer_of(state, tx, t).await?);
        }
        let spawn_next = |n: usize| tasks_of.len() <= n;
        match (claimed, cs.first()) {
            (_, None) => {
                if spawn_next(0) {
                    spawn_comparer(state, tx, root, (ax, sx), (ay, sy)).await?;
                    changed = true;
                }
            }
            (_, Some(None)) => {} // the first comparer has not submitted
            // A suspended node's job: the comparer's verdict is the comparison.
            (None, Some(Some(c1))) => {
                let hash = sub_digest(&c1.sub)?;
                rq::insert_comparison(
                    tx,
                    &NewComparison {
                        family: TaskId(root.id),
                        attempt_a: AttemptId(x),
                        attempt_b: AttemptId(y),
                        comparer: &format!("worker:{}", c1.attempt.worker_id),
                        outcome: c1.verdict,
                        hash: &hash,
                        signed_cbor: &c1.sub.submission_cbor,
                    },
                )
                .await?;
                decide_comparer(state, tx, c1, VerificationOutcome::Accepted, hash).await?;
                changed = true;
            }
            (Some(claimed), Some(Some(c1))) if c1.verdict == claimed => {
                let hash = sub_digest(&c1.sub)?;
                let r = row.expect("a claimed outcome comes from a row");
                rq::set_replay(
                    tx,
                    r.id,
                    claimed,
                    &hash,
                    &format!("worker:{}", c1.attempt.worker_id),
                )
                .await?;
                decide_comparer(state, tx, c1, VerificationOutcome::Accepted, hash).await?;
                changed = true;
            }
            (Some(claimed), Some(Some(c1))) => match cs.get(1) {
                None => {
                    if spawn_next(1) {
                        spawn_comparer(state, tx, root, (ax, sx), (ay, sy)).await?;
                        changed = true;
                    }
                }
                Some(None) => {}
                Some(Some(c2)) => {
                    let r = row.expect("a claimed outcome comes from a row");
                    let hash = sub_digest(&c2.sub)?;
                    let by = format!("worker:{}", c2.attempt.worker_id);
                    if c2.verdict == claimed {
                        // The node was right; the first comparer misreported.
                        rq::set_replay(tx, r.id, claimed, &hash, &by).await?;
                        decide_comparer(state, tx, c2, VerificationOutcome::Accepted, hash).await?;
                        decide_comparer(state, tx, c1, VerificationOutcome::Rejected, hash).await?;
                        suspend_account(
                            tx,
                            WorkerId(c1.attempt.worker_id),
                            &format!("comparison {} misreported (task {})", r.id, c1.task.id),
                        )
                        .await?;
                    } else if c2.verdict == c1.verdict {
                        // Two comparers of other accounts contradict the node.
                        rq::set_replay(tx, r.id, c1.verdict, &hash, &by).await?;
                        decide_comparer(state, tx, c1, VerificationOutcome::Accepted, hash).await?;
                        decide_comparer(state, tx, c2, VerificationOutcome::Accepted, hash).await?;
                        let why = format!(
                            "comparison {} reported {claimed}, two comparers found {}",
                            r.id, c1.verdict
                        );
                        rq::suspend_node(tx, node, &why).await?;
                        misc::audit(
                            tx,
                            "server",
                            "node.suspended",
                            &node.to_string(),
                            serde_json::json!({ "comparison": r.id, "reported": claimed, "found": c1.verdict }),
                        )
                        .await?;
                        tracing::warn!(%node, comparison = %r.id, "a lab node misreported a comparison: suspended pending a human");
                    } else {
                        // Three different answers: computed again.
                        decide_comparer(state, tx, c1, VerificationOutcome::Inconclusive, hash)
                            .await?;
                        decide_comparer(state, tx, c2, VerificationOutcome::Inconclusive, hash)
                            .await?;
                    }
                    changed = true;
                }
            },
        }
    }
    Ok(changed)
}

/// Record a signed comparison.
pub(crate) async fn record(
    state: &AppState,
    comparer: &str,
    signed: Signed<ComparisonV1>,
) -> Result<bool, ApiError> {
    let hash = signed.body_hash(state.network(), Domain::Comparison)?;
    let cbor = signed.to_canonical_bytes()?;
    let mut c = state.db().pool().acquire().await?;
    let recorded = rq::insert_comparison(
        &mut c,
        &NewComparison {
            family: signed.body.family,
            attempt_a: signed.body.a.attempt_id,
            attempt_b: signed.body.b.attempt_id,
            comparer,
            outcome: signed.body.outcome,
            hash: &hash,
            signed_cbor: &cbor,
        },
    )
    .await?;
    Ok(recorded)
}

/// One dispute per computation `subs`, citing the comparisons `hashes`; each waits for a human.
async fn open_disputes(
    tx: &mut gg_storage::PgConnection,
    loaded: &[Loaded],
    subs: &[(usize, Uuid)],
    hashes: &[Sha256Digest],
) -> Result<(), ApiError> {
    for (i, attempt) in subs {
        let l = &loaded[*i];
        let Some((_, sub)) = l.submitted.as_ref().filter(|(a, _)| a.id == *attempt) else {
            continue;
        };
        let created = gg_storage::queries::federated::insert_dispute(
            tx,
            Uuid::now_v7(),
            SubmissionId(sub.id),
            TaskId(l.task.id),
            AttemptId(*attempt),
            &serde_json::json!({ "comparisons": hashes }),
        )
        .await?;
        if created {
            let t_state = l
                .task
                .task_state()
                .map_err(|e| ApiError::Internal(e.to_string()))?;
            jobs::update_task_state(
                tx,
                TaskId(l.task.id),
                t_state.next(TaskEvent::Dispute, Authority::Server)?,
            )
            .await?;
        }
    }
    Ok(())
}

/// Compare and decide what can be for one family. Returns whether anything was decided.
async fn settle(state: &AppState, family: TaskId) -> Result<bool, ApiError> {
    // Comparisons first, outside any transaction: they read the outputs.
    let (loaded, rows, spot) = {
        let mut c = state.db().pool().acquire().await?;
        let loaded = load(&mut c, family).await?;
        let rows = rq::comparisons(&mut c, family).await?;
        let spot = rq::spot_checked(&mut c, family).await?;
        (loaded, rows, spot)
    };
    let compared = compared_of(&rows)?;
    let Some(root) = loaded.first().map(|l| l.task.clone()) else {
        return Ok(false);
    };
    let (todo, _) = plan(&members_of(&loaded), &compared, spot);
    for (x, y) in todo {
        if let (Some(ax), Some(ay)) = (find_attempt(&loaded, x), find_attempt(&loaded, y)) {
            compare_on_server(state, &root, ax, ay).await?;
        }
    }
    // Then the decisions, under the family's lock.
    let mut tx = state.db().begin().await?;
    let Some(root) = jobs::lock_task(&mut tx, family).await? else {
        return Ok(false);
    };
    let loaded = load(&mut tx, family).await?;
    let rows = rq::comparisons(&mut tx, family).await?;
    let checked = check_comparisons(state, &mut tx, &root, &loaded, &rows).await?;
    let rows = if checked {
        rq::comparisons(&mut tx, family).await?
    } else {
        rows
    };
    let compared = compared_of(&rows)?;
    let (_, actions) = plan(&members_of(&loaded), &compared, spot);
    if actions.is_empty() {
        if checked {
            tx.commit().await?;
            state.signal_work();
            return Ok(true);
        }
        tx.rollback().await?;
        return Ok(false);
    }
    let hash_of = |ids: &[Uuid]| -> Result<Vec<Sha256Digest>, ApiError> {
        ids.iter()
            .filter_map(|id| rows.iter().find(|r| r.id == *id))
            .map(|r| r.digest().map_err(|e| ApiError::Internal(e.to_string())))
            .collect()
    };
    for action in actions {
        match action {
            Action::Decide(i, attempt, outcome, via) => {
                let l = &loaded[i];
                let Some((a, sub)) = l.submitted.as_ref().filter(|(a, _)| a.id == attempt) else {
                    continue;
                };
                let task = jobs::lock_task(&mut tx, TaskId(l.task.id))
                    .await?
                    .ok_or_else(|| ApiError::Internal("task missing".into()))?;
                let hashes = hash_of(&via)?;
                let supporting = *hashes
                    .first()
                    .ok_or_else(|| ApiError::Internal("a decision without comparison".into()))?;
                super::decide::finalize(
                    state,
                    &mut tx,
                    &task,
                    a,
                    sub,
                    outcome,
                    supporting,
                    &hashes,
                    TaskEvent::Decide(outcome),
                    Authority::Server,
                )
                .await?;
                tracing::info!(family = %family, task = %task.id, seq = task.replica_seq, %outcome, "computation decided by comparison");
            }
            Action::SpawnThird => {
                let spec = decode_spec(&root.spec_cbor)?;
                let third = spawn(state, &mut tx, &root, &spec, 2).await?;
                tracing::warn!(family = %family, %third, "two computations differ: a third is asked for");
            }
            Action::Collusion(pair, check, via) => {
                for (i, _) in &pair {
                    if let Some((a, _)) = &loaded[*i].submitted {
                        suspend_account(
                            &mut tx,
                            WorkerId(a.worker_id),
                            &format!(
                                "computation of task {} agreed with another account's and was contradicted by a spot check (collusion suspected)",
                                loaded[*i].task.id
                            ),
                        )
                        .await?;
                    }
                }
                let subs: Vec<(usize, Uuid)> = pair.into_iter().chain(check).collect();
                open_disputes(&mut tx, &loaded, &subs, &hash_of(&via)?).await?;
                tracing::warn!(family = %family, "a pair contradicted by a spot check: accounts suspended, disputes opened");
            }
            Action::Dispute(subs, via) => {
                open_disputes(&mut tx, &loaded, &subs, &hash_of(&via)?).await?;
                tracing::warn!(family = %family, "three computations, no majority: disputes opened");
            }
        }
    }
    // Decided with an accepted computation: a replica still waiting is no longer needed.
    let settled = load(&mut tx, family).await?;
    if settled.iter().any(|l| l.accepted.is_some()) {
        for l in &settled {
            if l.task.replica_of.is_some() && l.task.state == "queued" {
                let t_state = l
                    .task
                    .task_state()
                    .map_err(|e| ApiError::Internal(e.to_string()))?;
                jobs::update_task_state(
                    &mut tx,
                    TaskId(l.task.id),
                    t_state.next(TaskEvent::Cancel, Authority::Server)?,
                )
                .await?;
            }
        }
    }
    let _ = root;
    tx.commit().await?;
    state.signal_decision();
    Ok(true)
}

/// One pass over the families with a computation to compare or decide.
pub async fn compare_once(state: &AppState) -> Result<usize, ApiError> {
    if !state.config().replica_verification {
        return Ok(0);
    }
    let families = {
        let mut c = state.db().pool().acquire().await?;
        rq::families_to_compare(&mut c, 50).await?
    };
    let mut n = 0;
    for f in families {
        match settle(state, f).await {
            Ok(true) => n += 1,
            Ok(false) => {}
            Err(e) => tracing::warn!(family = %f, error = %e, "replicas: settling a family failed"),
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use VerificationOutcome::*;

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn m(seq: i16, submitted: Option<u128>, accepted: Option<u128>) -> Member {
        Member {
            seq,
            submitted: submitted.map(id),
            accepted: accepted.map(id),
        }
    }

    #[test]
    fn two_that_agree_are_both_accepted() {
        let members = [m(0, Some(1), None), m(1, Some(2), None)];
        let (todo, actions) = plan(&members, &[], false);
        assert_eq!(todo, vec![(id(1), id(2))]);
        assert!(actions.is_empty());
        let (todo, actions) = plan(&members, &[(id(9), id(1), id(2), Accepted)], false);
        assert!(todo.is_empty());
        assert_eq!(
            actions,
            vec![
                Action::Decide(0, id(1), Accepted, vec![id(9)]),
                Action::Decide(1, id(2), Accepted, vec![id(9)]),
            ]
        );
    }

    #[test]
    fn one_alone_waits() {
        let (todo, actions) = plan(&[m(0, Some(1), None), m(1, None, None)], &[], false);
        assert!(todo.is_empty() && actions.is_empty());
    }

    #[test]
    fn two_that_differ_call_a_third_and_the_majority_wins() {
        let two = [m(0, Some(1), None), m(1, Some(2), None)];
        let c12 = (id(9), id(1), id(2), Rejected);
        assert_eq!(plan(&two, &[c12], false).1, vec![Action::SpawnThird]);
        // The third exists but has not submitted: wait.
        let waiting = [m(0, Some(1), None), m(1, Some(2), None), m(2, None, None)];
        assert!(plan(&waiting, &[c12], false).1.is_empty());
        // It agrees with the first: first and third accepted, the replica rejected.
        let three = [
            m(0, Some(1), None),
            m(1, Some(2), None),
            m(2, Some(3), None),
        ];
        let (todo, _) = plan(&three, &[c12], false);
        assert_eq!(todo, vec![(id(1), id(3)), (id(2), id(3))]);
        let c13 = (id(10), id(1), id(3), Accepted);
        let c23 = (id(11), id(2), id(3), Rejected);
        let (_, actions) = plan(&three, &[c12, c13, c23], false);
        assert_eq!(
            actions,
            vec![
                Action::Decide(0, id(1), Accepted, vec![id(10)]),
                Action::Decide(1, id(2), Rejected, vec![id(9), id(11)]),
                Action::Decide(2, id(3), Accepted, vec![id(10)]),
            ]
        );
        // No majority: disputes.
        let c13x = (id(10), id(1), id(3), Rejected);
        let (_, actions) = plan(&three, &[c12, c13x, c23], false);
        assert!(matches!(&actions[..], [Action::Dispute(s, _)] if s.len() == 3));
    }

    #[test]
    fn a_later_computation_is_compared_with_the_accepted_one() {
        let members = [
            m(0, Some(5), None),
            m(1, None, Some(2)),
            m(2, None, Some(3)),
        ];
        let (todo, actions) = plan(&members, &[], false);
        assert_eq!(todo, vec![(id(5), id(2))]);
        assert!(actions.is_empty());
        let (_, actions) = plan(&members, &[(id(9), id(2), id(5), Rejected)], false);
        assert_eq!(
            actions,
            vec![Action::Decide(0, id(5), Rejected, vec![id(9)])]
        );
    }

    #[test]
    fn a_spot_check_waits_and_catches_a_pair_it_contradicts() {
        let three = [m(0, Some(1), None), m(1, Some(2), None), m(2, None, None)];
        let c12 = (id(9), id(1), id(2), Accepted);
        // The pair agrees: without a spot check it is accepted; with one it waits.
        assert_eq!(plan(&three[..2], &[c12], false).1.len(), 2);
        assert!(plan(&three, &[c12], true).1.is_empty());
        let done = [
            m(0, Some(1), None),
            m(1, Some(2), None),
            m(2, Some(3), None),
        ];
        let c13 = (id(10), id(1), id(3), Rejected);
        let c23 = (id(11), id(2), id(3), Rejected);
        let (_, actions) = plan(&done, &[c12, c13, c23], true);
        assert!(
            matches!(&actions[..], [Action::Collusion(pair, check, _)]
                if pair.len() == 2 && check == &vec![(2, id(3))]),
            "{actions:?}"
        );
        // All three agree: all accepted.
        let a13 = (id(10), id(1), id(3), Accepted);
        let a23 = (id(11), id(2), id(3), Accepted);
        let (_, actions) = plan(&done, &[c12, a13, a23], true);
        assert_eq!(actions.len(), 3);
        assert!(
            actions
                .iter()
                .all(|a| matches!(a, Action::Decide(_, _, Accepted, _)))
        );
        // The check sides with one of a pair that differs: the usual majority.
        let d12 = (id(9), id(1), id(2), Rejected);
        let (_, actions) = plan(&done, &[d12, a13, c23], true);
        assert!(actions.contains(&Action::Decide(1, id(2), Rejected, vec![id(9), id(11)])));
    }

    #[test]
    fn spot_checks_follow_the_rate() {
        let n = (0..2000u128)
            .filter(|i| spot_drawn(b"secret", TaskId(id(*i)), 0.05))
            .count();
        assert!((60..=140).contains(&n), "{n}");
        assert!(!spot_drawn(b"secret", TaskId(id(1)), 0.0));
        assert!(spot_drawn(b"secret", TaskId(id(1)), 1.0));
    }

    #[test]
    fn the_draw_is_the_servers_and_stable() {
        let ws: Vec<WorkerId> = (1..=5).map(|n| WorkerId(id(n))).collect();
        let t = TaskId(id(77));
        let a = draw(b"secret", t, &ws);
        assert_eq!(a, draw(b"secret", t, &ws));
        let mut rev = ws.clone();
        rev.reverse();
        assert_eq!(
            a,
            draw(b"secret", t, &rev),
            "not the order of the candidates"
        );
        assert!(draw(b"secret", t, &[]).is_none());
        // Another secret draws independently (over many tasks, not always the same worker).
        let differs = (0..32u128)
            .any(|n| draw(b"secret", TaskId(id(n)), &ws) != draw(b"other", TaskId(id(n)), &ws));
        assert!(differs);
    }
}

/// One side of a comparison a lab node is asked for.
#[derive(Clone, Debug, serde::Serialize, utoipa::ToSchema)]
pub struct CompareSide {
    pub task_id: TaskId,
    pub attempt_id: AttemptId,
    pub submission_hash: Sha256Digest,
    /// The outputs it declared, all held by the node.
    #[schema(value_type = Object)]
    pub outputs: BTreeMap<String, gg_protocol::messages::ArtifactRef>,
    /// The numeric profile it declared.
    pub profile: String,
}

/// A comparison a lab node is asked for: two computations of a task of a job it holds.
#[derive(Clone, Debug, serde::Serialize, utoipa::ToSchema)]
pub struct ComparisonWork {
    pub family: TaskId,
    /// The task's definition (its tolerance, its kind).
    #[schema(value_type = Object)]
    pub definition: gg_protocol::messages::TaskDefinitionV1,
    pub a: CompareSide,
    pub b: CompareSide,
}

fn side_of((t, a, s): (&TaskRow, &AttemptRow, &SubmissionRow)) -> Result<CompareSide, ApiError> {
    let signed: Signed<ResultSubmissionV1> = Signed::from_canonical_bytes(&s.submission_cbor)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(CompareSide {
        task_id: TaskId(t.id),
        attempt_id: AttemptId(a.id),
        submission_hash: s.digest().map_err(|e| ApiError::Internal(e.to_string()))?,
        outputs: signed.body.outputs,
        profile: signed.body.execution_profile.numeric_profile,
    })
}

/// The comparisons `node` is asked for: pairs of computations of the jobs it holds, not compared
/// yet, whose outputs it has all pulled.
pub async fn node_work(state: &AppState, node: NodeId) -> Result<Vec<ComparisonWork>, ApiError> {
    let mut c = state.db().pool().acquire().await?;
    if !state.config().replica_verification || rq::node_suspended(&mut c, node).await? {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for family in rq::node_families(&mut c, node, 50).await? {
        let loaded = load(&mut c, family).await?;
        let rows = rq::comparisons(&mut c, family).await?;
        let (todo, _) = plan(&members_of(&loaded), &compared_of(&rows)?, false);
        let Some(root) = loaded.first() else { continue };
        let definition = decode_spec(&root.task.spec_cbor)?.definition;
        'pair: for (x, y) in todo {
            if rows.iter().any(|r| r.of(x, y)) {
                continue; // compared already: the server replays it
            }
            let (Some(ax), Some(ay)) = (find_attempt(&loaded, x), find_attempt(&loaded, y)) else {
                continue;
            };
            let (a, b) = (side_of(ax)?, side_of(ay)?);
            for r in a.outputs.values().chain(b.outputs.values()) {
                if gg_storage::queries::artifacts::held_by_node(&mut c, &r.hash).await?
                    != Some(node)
                {
                    continue 'pair; // not pulled yet
                }
            }
            out.push(ComparisonWork {
                family,
                definition: definition.clone(),
                a,
                b,
            });
        }
    }
    Ok(out)
}

/// A comparison `node` signed (with `key`, its registered key): recorded if it compares two
/// current computations of a task of a job the node holds, as they were submitted.
pub async fn accept_node_comparison(
    state: &AppState,
    node: NodeId,
    key: &gg_protocol::keys::PublicKey,
    signed: Signed<ComparisonV1>,
) -> Result<bool, ApiError> {
    if signed.signer != *key
        || signed.verify(state.network(), Domain::Comparison).is_err()
        || signed.body.network != *state.network()
    {
        return Err(ApiError::Unprocessable(
            "a comparison signed by this node for this network is expected".into(),
        ));
    }
    let family = signed.body.family;
    {
        let mut c = state.db().pool().acquire().await?;
        if rq::node_suspended(&mut c, node).await? {
            return Err(ApiError::Forbidden(
                "this node is suspended: its comparisons are not taken".into(),
            ));
        }
        let root = jobs::get_task(&mut c, family)
            .await?
            .ok_or_else(|| ApiError::NotFound("task".into()))?;
        if jobs::output_holder(&mut c, root.job_id).await? != Some(node) {
            return Err(ApiError::NotFound("task".into()));
        }
        let loaded = load(&mut c, family).await?;
        for side in [&signed.body.a, &signed.body.b] {
            let Some((t, _, s)) = find_attempt(&loaded, side.attempt_id.0) else {
                return Err(ApiError::Conflict(
                    "not a current computation of this task".into(),
                ));
            };
            if t.id != side.task_id.0
                || s.digest().map_err(|e| ApiError::Internal(e.to_string()))?
                    != side.submission_hash
            {
                return Err(ApiError::Unprocessable(
                    "the comparison does not name the computation as it was submitted".into(),
                ));
            }
        }
        if signed.body.a.attempt_id == signed.body.b.attempt_id {
            return Err(ApiError::Unprocessable(
                "one computation compared with itself".into(),
            ));
        }
    }
    let recorded = record(state, &format!("node:{node}"), signed).await?;
    state.signal_submission();
    Ok(recorded)
}
