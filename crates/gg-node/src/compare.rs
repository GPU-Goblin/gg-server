//! Comparisons of computations (ADR-0042): the node pulls both computations of each task of the
//! jobs it holds, so it compares them, with the rule anyone holding both outputs would apply
//! (`gg_compute::replica`), and signs the result with its key. The server takes an agreement as it
//! is (the scientist pays for what the node accepts); a difference counts only once the server has
//! replayed it, and a node found to have misreported one is suspended pending a human.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use gg_core::{AttemptId, Sha256Digest, TaskId, UnixMillis};
use gg_protocol::domain::Domain;
use gg_protocol::messages::{ArtifactRef, ComparedV1, ComparisonV1, TaskDefinitionV1};
use gg_protocol::signed::Signed;
use serde::Deserialize;

use crate::Node;

#[derive(Clone, Debug, Deserialize)]
struct Side {
    task_id: TaskId,
    attempt_id: AttemptId,
    submission_hash: Sha256Digest,
    outputs: BTreeMap<String, ArtifactRef>,
    profile: String,
}

#[derive(Clone, Debug, Deserialize)]
struct Work {
    family: TaskId,
    definition: TaskDefinitionV1,
    a: Side,
    b: Side,
}

impl Node {
    /// Compare the computations the server asks this node for, waiting up to `wait_secs` for some,
    /// and report each signed comparison. Returns how many were reported.
    pub async fn compare_once(&self, wait_secs: u64) -> anyhow::Result<usize> {
        let path = format!("/node/comparisons?wait_secs={wait_secs}");
        let work: Vec<Work> = self
            .call(|token| {
                let path = path.clone();
                async move { self.api.get_json(&token, &path).await }
            })
            .await?;
        let mut done = 0;
        for w in work {
            let signed = self.compare(&w).await?;
            let body = serde_json::to_value(&signed)?;
            self.call(|token| {
                let body = body.clone();
                async move {
                    self.api
                        .post_json::<serde_json::Value>(&token, "/node/comparisons", &body)
                        .await
                }
            })
            .await
            .with_context(|| format!("report the comparison of task {}", w.family))?;
            tracing::info!(family = %w.family, outcome = %signed.body.outcome, "computations compared");
            done += 1;
        }
        Ok(done)
    }

    async fn compare(&self, w: &Work) -> anyhow::Result<Signed<ComparisonV1>> {
        let task = gg_compute::validate_definition(&w.definition)?;
        let files = |s: &Side| -> BTreeMap<String, PathBuf> {
            s.outputs
                .iter()
                .map(|(n, r)| (n.clone(), self.cache.path_of(&r.hash)))
                .collect()
        };
        let (a, b) = (w.a.clone(), w.b.clone());
        let (fa, fb) = (files(&a), files(&b));
        let verdict = tokio::task::spawn_blocking(move || {
            gg_compute::replica::compare_replicas(
                &task,
                &gg_compute::replica::Side {
                    outputs: &a.outputs,
                    files: &fa,
                    profile: &a.profile,
                },
                &gg_compute::replica::Side {
                    outputs: &b.outputs,
                    files: &fb,
                    profile: &b.profile,
                },
            )
        })
        .await??;
        let side = |s: &Side| ComparedV1 {
            task_id: s.task_id,
            attempt_id: s.attempt_id,
            submission_hash: s.submission_hash,
        };
        let body = ComparisonV1 {
            protocol_version: gg_protocol::PROTOCOL_VERSION,
            network: self.info.network_id.clone(),
            family: w.family,
            a: side(&w.a),
            b: side(&w.b),
            outcome: verdict.outcome,
            checks: verdict.checks,
            compared_at: UnixMillis::now(),
        };
        Ok(Signed::sign(
            &self.key,
            &self.info.network_id,
            Domain::Comparison,
            body,
        )?)
    }

    /// Compare as soon as the server asks (it answers a waiting request at once), until `stop`
    /// turns true.
    pub async fn keep_comparing(
        self: Arc<Self>,
        retry: Duration,
        mut stop: tokio::sync::watch::Receiver<bool>,
    ) {
        loop {
            tokio::select! {
                r = self.compare_once(10) => {
                    if let Err(e) = r {
                        tracing::warn!(error = %e, "comparing computations failed, retrying later");
                        tokio::time::sleep(retry).await;
                    }
                }
                _ = stop.changed() => { if *stop.borrow() { return; } }
            }
        }
    }
}
