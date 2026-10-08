//! The optimizer step of the runs this node holds (ADR-0041, A6): the initial weights drawn from
//! the run's seed, then, each round, the mean of the accepted gradients and one optimizer step,
//! shard by shard, with the code the coordinator used (`init_parameter`, `aggregate_named`,
//! `apply_step_named`). Every new checkpoint is held here and reported to the server, which checks
//! it against the run's shard plan and opens the next round.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use gg_compute::grad::CoordOptim;
use gg_compute::graph::task::{Named, NamedOptimizerState, aggregate_named, apply_step_named};
use gg_core::{ProjectId, Sha256Digest};
use gg_protocol::messages::ArtifactRef;
use gg_protocol::tensor_file::{BundleF32, Dtype, TensorLimits};
use serde::{Deserialize, Serialize};

use crate::Node;

/// What the server says is due (`GET /node/steps`).
#[derive(Clone, Debug, Deserialize)]
struct StepWork {
    run_id: uuid::Uuid,
    project_id: ProjectId,
    step: i32,
    round_id: Option<uuid::Uuid>,
    config: RunConfig,
    checkpoint: CheckpointRefs,
    parts: Vec<StepPart>,
}

#[derive(Clone, Debug, Deserialize)]
struct StepPart {
    example_count: u32,
    /// Data-parallel: one gradient bundle per shard.
    #[serde(default)]
    outputs: BTreeMap<String, ArtifactRef>,
    /// Staged: partial gradient bundles in production order.
    #[serde(default)]
    partials: Vec<(String, ArtifactRef)>,
}

/// Rows to cut for a round (`GET /node/rows`).
#[derive(Clone, Debug, Deserialize)]
struct RowsDue {
    run_id: uuid::Uuid,
    project_id: ProjectId,
    step: i32,
    round_seed: u64,
    total_examples: u64,
    slices: Vec<RowSlice>,
    datasets: Vec<RowsDataset>,
}

#[derive(Clone, Debug, Deserialize)]
struct RowSlice {
    example_start: u64,
    example_count: u32,
}

#[derive(Clone, Debug, Deserialize)]
struct RowsDataset {
    name: String,
    shape: Vec<u64>,
    parts: Vec<ArtifactRef>,
}

/// Rows `idx` of a dataset of full shape `shape` stored as consecutive row ranges in `parts`,
/// read by seeking: never more than the micro-batch in memory. The same rows, in the same order,
/// as the coordinator cuts for a run it serves.
fn read_rows(
    parts: &[(std::path::PathBuf, u64)],
    shape: &[u64],
    idx: &[usize],
) -> anyhow::Result<gg_protocol::tensor_file::TensorF32> {
    use std::io::{Read, Seek};
    let row_elems: u64 = shape[1..].iter().product();
    let row_bytes = row_elems * 4;
    let header = 6 + 8 * shape.len() as u64;
    let mut bounds = Vec::with_capacity(parts.len());
    let mut first = 0u64;
    for (_, size) in parts {
        let rows = size.saturating_sub(header) / row_bytes.max(1);
        bounds.push((first, first + rows));
        first += rows;
    }
    if first != shape[0] {
        bail!(
            "the dataset's parts hold {first} rows, its shape says {}",
            shape[0]
        );
    }
    let mut files: Vec<Option<std::fs::File>> = (0..parts.len()).map(|_| None).collect();
    let mut data = Vec::with_capacity(idx.len() * row_elems as usize);
    let mut buf = vec![0u8; row_bytes as usize];
    for &i in idx {
        let i = i as u64;
        let k = bounds
            .iter()
            .position(|(a, b)| i >= *a && i < *b)
            .with_context(|| format!("row {i} outside the dataset"))?;
        if files[k].is_none() {
            files[k] = Some(std::fs::File::open(&parts[k].0)?);
        }
        let f = files[k].as_mut().expect("opened above");
        f.seek(std::io::SeekFrom::Start(
            header + (i - bounds[k].0) * row_bytes,
        ))?;
        f.read_exact(&mut buf)?;
        data.extend(
            buf.chunks_exact(4)
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])),
        );
    }
    let mut out_shape = shape.to_vec();
    out_shape[0] = idx.len() as u64;
    Ok(gg_protocol::tensor_file::TensorF32::new(out_shape, data)?)
}

/// The fields of a run's configuration the step needs.
#[derive(Clone, Debug, Deserialize)]
struct RunConfig {
    optimizer: String,
    learning_rate: f64,
    seed: u64,
    graph: GraphConfig,
}

#[derive(Clone, Debug, Deserialize)]
struct GraphConfig {
    graph: ArtifactRef,
    parameters: u64,
    checkpoint_bytes: u64,
    gradient_bytes: u64,
    shard_bytes: u64,
    #[serde(default)]
    gradient_bf16: bool,
    /// Weights to start from, held here, instead of the seeded initialization.
    #[serde(default)]
    initial_checkpoint: Option<ArtifactRef>,
    /// The model's stages (`0 = b₀ < … < b_K`), empty when it is not staged.
    #[serde(default)]
    stage_bounds: Vec<u32>,
    /// The matrices tiled in `graph` (by this node), in order; empty when none was.
    #[serde(default)]
    tiling: Vec<gg_compute::graph::tiling::Tiled>,
    /// The description before tiling, when it was tiled.
    #[serde(default)]
    source_graph: Option<ArtifactRef>,
}

/// A checkpoint's references, as the server records them.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct CheckpointRefs {
    weights: BTreeMap<String, ArtifactRef>,
    #[serde(default)]
    opt_m: Option<BTreeMap<String, ArtifactRef>>,
    #[serde(default)]
    opt_v: Option<BTreeMap<String, ArtifactRef>>,
    #[serde(default)]
    opt_t: u32,
    #[serde(default)]
    opt_lag: u32,
}

impl Node {
    /// Apply the steps due on this node's runs and report them. Returns how many were reported. A
    /// round whose gradients are not all here yet is left for the next pass.
    pub async fn step_once(&self) -> anyhow::Result<usize> {
        Ok(self.step_pass(0).await?.0)
    }

    /// As [`Node::step_once`], waiting up to `wait_secs` for a step to become due. Returns the
    /// steps reported and those due but not ready (a gradient still on its way).
    async fn step_pass(&self, wait_secs: u64) -> anyhow::Result<(usize, usize)> {
        // The rows of the next round first: cut while the open round computes.
        self.rows_pass().await?;
        let path = format!("/node/steps?wait_secs={wait_secs}");
        let work: Vec<StepWork> = self
            .call(|token| {
                let path = path.clone();
                async move { self.api.get_json(&token, &path).await }
            })
            .await?;
        if !work.is_empty() {
            // A run created while waiting: the rows of its first round go before its step 0.
            self.rows_pass().await?;
        }
        let (mut done, mut waiting) = (0, 0);
        for w in work {
            let Some(ckpt) = self.compute_step(&w).await? else {
                waiting += 1;
                continue;
            };
            let path = format!("/node/runs/{}/steps", w.run_id);
            let body =
                serde_json::json!({ "step": w.step, "round_id": w.round_id, "checkpoint": ckpt });
            self.call(|token| {
                let (path, body) = (path.clone(), body.clone());
                async move {
                    self.api
                        .post_json::<serde_json::Value>(&token, &path, &body)
                        .await
                }
            })
            .await
            .with_context(|| format!("report step {} of run {}", w.step, w.run_id))?;
            tracing::info!(run = %w.run_id, step = w.step, "step applied and reported");
            done += 1;
        }
        Ok((done, waiting))
    }

    /// Apply steps as soon as the server says they are due (it answers a waiting request at
    /// once), until `stop` turns true. After a failure, waits `retry` before trying again.
    pub async fn keep_stepping(
        self: Arc<Self>,
        retry: Duration,
        mut stop: tokio::sync::watch::Receiver<bool>,
    ) {
        loop {
            tokio::select! {
                r = self.step_pass(10) => {
                    match r {
                        // Due, but a gradient is still on its way: the server would answer at once.
                        Ok((0, waiting)) if waiting > 0 => {
                            tokio::time::sleep(Duration::from_millis(50)).await;
                        }
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!(error = %e, "a step failed, retrying later");
                            tokio::time::sleep(retry).await;
                        }
                    }
                }
                _ = stop.changed() => { if *stop.borrow() { return; } }
            }
        }
    }

    /// Cut, hold and declare the micro-batch rows due on this node's runs (the cutting by the
    /// node): each task's rows of every dataset, read from the datasets held here. Returns how many
    /// steps' rows were declared.
    pub async fn rows_pass(&self) -> anyhow::Result<usize> {
        let due: Vec<RowsDue> = self
            .call(|token| async move { self.api.get_json(&token, "/node/rows").await })
            .await?;
        let mut declared = 0;
        for d in due {
            let rows = self.cut_rows(&d).await?;
            let path = format!("/node/runs/{}/rows", d.run_id);
            let body = serde_json::json!({ "step": d.step, "rows": rows });
            self.call(|token| {
                let (path, body) = (path.clone(), body.clone());
                async move {
                    self.api
                        .post_json::<serde_json::Value>(&token, &path, &body)
                        .await
                }
            })
            .await
            .with_context(|| format!("declare the rows of step {} of run {}", d.step, d.run_id))?;
            tracing::info!(run = %d.run_id, step = d.step, tasks = rows.len(), "rows cut and declared");
            declared += 1;
        }
        Ok(declared)
    }

    /// The rows of every task of `d`, held here, in slice order.
    async fn cut_rows(&self, d: &RowsDue) -> anyhow::Result<Vec<BTreeMap<String, ArtifactRef>>> {
        let mut sources = Vec::with_capacity(d.datasets.len());
        for ds in &d.datasets {
            let mut parts = Vec::with_capacity(ds.parts.len());
            for r in &ds.parts {
                let path = self.cache.path_of(&r.hash);
                let len = std::fs::metadata(&path)
                    .with_context(|| format!("dataset {} is not held by this node", ds.name))?
                    .len();
                if len != r.size {
                    bail!(
                        "dataset {} part {} does not match its size",
                        ds.name,
                        r.hash
                    );
                }
                parts.push((path, r.size));
            }
            sources.push((ds.name.clone(), ds.shape.clone(), parts));
        }
        let slices: Vec<(u64, u32)> = d
            .slices
            .iter()
            .map(|s| (s.example_start, s.example_count))
            .collect();
        let (seed, total) = (d.round_seed, d.total_examples);
        // The rows of one round: no more than the round's batch in memory.
        let cut = tokio::task::spawn_blocking(move || {
            slices
                .iter()
                .map(|&(start, count)| {
                    let idx = gg_compute::graph::task::round_indices(seed, total, start, count);
                    sources
                        .iter()
                        .map(|(name, shape, parts)| {
                            Ok((name.clone(), read_rows(parts, shape, &idx)?.encode()))
                        })
                        .collect::<anyhow::Result<Vec<_>>>()
                })
                .collect::<anyhow::Result<Vec<_>>>()
        })
        .await??;
        let mut holds = Vec::new();
        for (task, tensors) in cut.into_iter().enumerate() {
            for (name, bytes) in tensors {
                holds.push(async move {
                    let r = self.hold_bytes(d.project_id, bytes, "ggt1-f32").await?;
                    anyhow::Ok((task, name, r))
                });
            }
        }
        let mut out = vec![BTreeMap::new(); d.slices.len()];
        for (task, name, r) in futures_util::future::try_join_all(holds).await? {
            out[task].insert(name, r);
        }
        Ok(out)
    }

    /// The bytes of an artifact in the store, or `None` when it is not here (yet).
    fn local(&self, r: &ArtifactRef) -> anyhow::Result<Option<Vec<u8>>> {
        let path = self.cache.path_of(&r.hash);
        match std::fs::read(&path) {
            Ok(b) if b.len() as u64 == r.size && Sha256Digest::of_bytes(&b) == r.hash => {
                Ok(Some(b))
            }
            Ok(_) => bail!("stored artifact {} does not match its hash", r.hash),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// The run's description: kept by the server (it is small), fetched once into the store.
    async fn description(&self, r: &ArtifactRef) -> anyhow::Result<Vec<u8>> {
        self.obtain(r)
            .await?
            .context("the server does not serve the run's description")
    }

    /// An artifact of the node's projects: from its store, or else from the server when the server
    /// keeps it (a description; an output small enough for the server to relay), kept in the store
    /// afterwards. `None`: nobody has it yet (an output the node is still pulling).
    async fn obtain(&self, r: &ArtifactRef) -> anyhow::Result<Option<Vec<u8>>> {
        if let Some(b) = self.local(r)? {
            return Ok(Some(b));
        }
        let dir = self.cache.root().join(".incoming");
        std::fs::create_dir_all(&dir)?;
        let dest = dir.join(format!("graph-{}", r.hash));
        let token = self.token.lock().await.clone();
        match self
            .api
            .download(
                &token,
                &format!("/node/artifacts/{}", r.hash),
                r,
                r.size,
                &dest,
            )
            .await
        {
            Ok(()) => {}
            Err(gg_client::client::ClientError::Status { status: 409, .. }) => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        let stored = self.cache.store(&r.hash, &dest);
        let _ = std::fs::remove_file(&dest);
        stored?;
        Ok(Some(
            self.local(r)?
                .context("artifact missing right after it was stored")?,
        ))
    }

    /// The new checkpoint of `w`, held by this node; `None` when a gradient is not here yet.
    async fn compute_step(&self, w: &StepWork) -> anyhow::Result<Option<CheckpointRefs>> {
        let cfg = &w.config;
        let g = &cfg.graph;
        let optim = CoordOptim::parse(&cfg.optimizer)
            .with_context(|| format!("optimizer '{}'", cfg.optimizer))?;
        let model: gg_compute::graph::ModelGraphV1 =
            serde_json::from_slice(&self.description(&g.graph).await?)?;
        let report = gg_compute::graph::validate(&model).map_err(|e| anyhow::anyhow!("{e}"))?;
        let plan = if g.stage_bounds.is_empty() {
            gg_compute::graph::shards::plan(&model, &report, g.shard_bytes)
        } else {
            gg_compute::graph::stages::stages(&model, &report, &g.stage_bounds)
                .and_then(|st| gg_compute::graph::shards::plan_stages(&model, &st))
        }
        .map_err(|e| anyhow::anyhow!("{e}"))?;
        if plan.checkpoint_bytes() != g.checkpoint_bytes
            || plan.gradient_bytes() != g.gradient_bytes
        {
            bail!("the shard plan does not match the run");
        }
        let limits = TensorLimits {
            max_elements: g.parameters,
        };
        let decode = |bytes: &[u8], dtype: Dtype| -> anyhow::Result<Named> {
            Ok(BundleF32::decode_as(bytes, &limits, dtype)?.entries)
        };
        let mut out = CheckpointRefs::default();

        if w.round_id.is_none() {
            // Step 0: the weights supplied (held here, checked against the description), or drawn
            // from the seed as the coordinator draws them.
            let mut supplied = match &g.initial_checkpoint {
                Some(r) => {
                    let bytes = self
                        .local(r)?
                        .context("the initial checkpoint is not held by this node")?;
                    let bundle = BundleF32::decode(&bytes, &limits)?;
                    if g.tiling.is_empty() {
                        gg_compute::graph::task::check_bundle_matches(
                            &model,
                            &bundle,
                            "initial_checkpoint",
                        )
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                        if !plan.sharded {
                            // One piece: the bundle is the checkpoint as it is.
                            out.weights.insert(plan.checkpoint_name(0), *r);
                            return Ok(Some(out));
                        }
                        Some(bundle.entries)
                    } else {
                        // Weights of the model before tiling: checked against its description,
                        // then cut into the tiles this node cut the description into.
                        let source: gg_compute::graph::ModelGraphV1 = serde_json::from_slice(
                            &self
                                .description(
                                    g.source_graph
                                        .as_ref()
                                        .context("a tiled run without its source")?,
                                )
                                .await?,
                        )?;
                        let source = gg_compute::graph::functions::inline(&source)
                            .map_err(|e| anyhow::anyhow!("{e}"))?;
                        gg_compute::graph::task::check_bundle_matches(
                            &source,
                            &bundle,
                            "initial_checkpoint",
                        )
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                        Some(
                            gg_compute::graph::tiling::split_weights(&bundle.entries, &g.tiling)
                                .map_err(|e| anyhow::anyhow!("{e}"))?,
                        )
                    }
                }
                None => None,
            };
            for (i, shard) in plan.shards.iter().enumerate() {
                let mut named = Named::new();
                for name in &shard.params {
                    if let Some(all) = supplied.as_mut() {
                        let t = all
                            .remove(name)
                            .with_context(|| format!("initial_checkpoint lacks '{name}'"))?;
                        named.insert(name.clone(), t);
                        continue;
                    }
                    let p = model
                        .parameters
                        .iter()
                        .find(|p| &p.name == name)
                        .with_context(|| format!("shard names '{name}'"))?;
                    named.insert(
                        name.clone(),
                        gg_compute::graph::exec::init_parameter(p, cfg.seed),
                    );
                }
                let bytes = BundleF32::encode_entries_as(&named, Dtype::F32)?;
                let r = self.hold_bytes(w.project_id, bytes, "ggb1-f32").await?;
                if r.size != shard.checkpoint_bytes {
                    bail!(
                        "shard {i} is {} bytes, the plan says {}",
                        r.size,
                        shard.checkpoint_bytes
                    );
                }
                out.weights.insert(plan.checkpoint_name(i), r);
            }
            return Ok(Some(out));
        }

        let prev = &w.checkpoint;
        let dtype = if g.gradient_bf16 {
            Dtype::Bf16
        } else {
            Dtype::F32
        };
        let mut opt_m = BTreeMap::new();
        let mut opt_v = BTreeMap::new();
        let mut t_after = prev.opt_t;
        for (i, shard) in plan.shards.iter().enumerate() {
            let key = plan.checkpoint_name(i);
            let w_ref = prev
                .weights
                .get(&key)
                .with_context(|| format!("checkpoint lacks {key}"))?;
            if shard.gradient_bytes == 0 {
                // No trained parameter: the shard stays as it is.
                out.weights.insert(key, *w_ref);
                continue;
            }
            let grad_name = plan.gradient_name(i);
            let mut parts = Vec::with_capacity(w.parts.len());
            for part in &w.parts {
                if g.stage_bounds.is_empty() {
                    let r = part
                        .outputs
                        .get(&grad_name)
                        .with_context(|| format!("output {grad_name} missing"))?;
                    let Some(bytes) = self.obtain(r).await? else {
                        tracing::debug!(run = %w.run_id, hash = %r.hash, "gradient not pulled yet");
                        return Ok(None);
                    };
                    parts.push((part.example_count, decode(&bytes, dtype)?));
                    continue;
                }
                // Staged: the partial gradients of every stage that used a parameter of the
                // shard, summed in production order, as the coordinator sums them.
                let mut sum = Named::new();
                for (_, r) in part.partials.iter().filter(|(n, _)| *n == grad_name) {
                    let Some(bytes) = self.obtain(r).await? else {
                        tracing::debug!(run = %w.run_id, hash = %r.hash, "gradient not pulled yet");
                        return Ok(None);
                    };
                    for (pn, t) in decode(&bytes, dtype)? {
                        match sum.get_mut(&pn) {
                            Some(acc) => {
                                acc.data.iter_mut().zip(&t.data).for_each(|(x, y)| *x += *y)
                            }
                            None => {
                                sum.insert(pn, t);
                            }
                        }
                    }
                }
                // A trained parameter no stage used gets a zero gradient.
                for pn in &shard.params {
                    if let Some(p) = model.parameters.iter().find(|q| &q.name == pn)
                        && p.trainable
                        && !sum.contains_key(pn)
                    {
                        let n: u64 = p.shape.iter().product();
                        sum.insert(
                            pn.clone(),
                            gg_protocol::tensor_file::TensorF32::new(
                                p.shape.clone(),
                                vec![0.0; n as usize],
                            )?,
                        );
                    }
                }
                parts.push((part.example_count, sum));
            }
            let weights = decode(
                &self
                    .local(w_ref)?
                    .with_context(|| format!("{key} is not here"))?,
                Dtype::F32,
            )?;
            let moment =
                |map: &Option<BTreeMap<String, ArtifactRef>>| -> anyhow::Result<Option<Named>> {
                    match map.as_ref().and_then(|m| m.get(&key)) {
                        Some(r) => Ok(Some(decode(
                            &self
                                .local(r)?
                                .with_context(|| format!("moments of {key} are not here"))?,
                            Dtype::F32,
                        )?)),
                        None => Ok(None),
                    }
                };
            let opt = NamedOptimizerState {
                m: moment(&prev.opt_m)?,
                v: moment(&prev.opt_v)?,
                t: prev.opt_t,
            };
            let lr = cfg.learning_rate;
            let (weights, opt) = tokio::task::spawn_blocking(move || {
                let grad = aggregate_named(&parts)?;
                drop(parts);
                let (mut weights, mut opt) = (weights, opt);
                apply_step_named(optim, lr, &mut weights, &grad, &mut opt)?;
                anyhow::Ok((weights, opt))
            })
            .await??;
            t_after = opt.t;
            let w_new = self
                .hold_bytes(
                    w.project_id,
                    BundleF32::encode_entries_as(&weights, Dtype::F32)?,
                    "ggb1-f32",
                )
                .await?;
            out.weights.insert(key.clone(), w_new);
            if let Some(m) = &opt.m {
                let r = self
                    .hold_bytes(
                        w.project_id,
                        BundleF32::encode_entries_as(m, Dtype::F32)?,
                        "ggb1-f32",
                    )
                    .await?;
                opt_m.insert(key.clone(), r);
            }
            if let Some(v) = &opt.v {
                let r = self
                    .hold_bytes(
                        w.project_id,
                        BundleF32::encode_entries_as(v, Dtype::F32)?,
                        "ggb1-f32",
                    )
                    .await?;
                opt_v.insert(key.clone(), r);
            }
        }
        out.opt_t = t_after;
        out.opt_m = (!opt_m.is_empty()).then_some(opt_m);
        out.opt_v = (!opt_v.is_empty()).then_some(opt_v);
        Ok(Some(out))
    }
}
