//! `gg`: command line for researchers, operators and contributors.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand};
use gg_client::client::Api;
use gg_core::*;
use gg_protocol::domain::Domain;
use gg_protocol::keys::{PublicKey, SecretKey};
use gg_protocol::messages::{ArtifactRef, ContributionReceiptV1};
use gg_protocol::signed::Signed;
use gg_protocol::tensor_file::TensorF32;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Parser)]
#[command(name = "gg", about = "GPU Goblin CLI", version)]
struct Cli {
    /// Control plane URL (overrides the saved configuration; env GG_SERVER_URL).
    #[arg(long, global = true, env = "GG_SERVER_URL")]
    server: Option<String>,
    /// Bearer token (env GG_TOKEN).
    #[arg(long, global = true, env = "GG_TOKEN", hide_env_values = true)]
    token: Option<String>,
    #[command(subcommand)]
    cmd: Family,
}

#[derive(Subcommand)]
enum Family {
    /// Save credentials, show identity.
    Auth {
        #[command(subcommand)]
        cmd: AuthCmd,
    },
    /// Projects and budgets.
    Project {
        #[command(subcommand)]
        cmd: ProjectCmd,
    },
    /// Jobs and tasks.
    Job {
        #[command(subcommand)]
        cmd: JobCmd,
    },
    /// Contributor keys and receipts.
    Worker {
        #[command(subcommand)]
        cmd: WorkerCmd,
    },
    /// Signed releases of the distributed executables.
    Release {
        #[command(subcommand)]
        cmd: ReleaseCmd,
    },
    /// Receipt export and offline verification.
    Receipt {
        #[command(subcommand)]
        cmd: ReceiptCmd,
    },
    /// Credits.
    Ledger {
        #[command(subcommand)]
        cmd: LedgerCmd,
    },
    /// Operator commands (admin token required).
    Admin {
        #[command(subcommand)]
        cmd: AdminCmd,
    },
    /// Test data generation.
    Data {
        #[command(subcommand)]
        cmd: DataCmd,
    },
}

#[derive(Subcommand)]
enum AuthCmd {
    /// Save server URL and token in the user configuration (mode 0600).
    Login {
        #[arg(long)]
        server: String,
        #[arg(long)]
        token: String,
    },
    /// Show the authenticated identity.
    Whoami,
}

#[derive(Subcommand)]
enum ProjectCmd {
    /// List projects of the researcher.
    List,
    /// Show a project with its budget.
    Show { id: ProjectId },
    /// Show ledger entries of a project.
    Ledger { id: ProjectId },
    /// Export receipts of a project.
    Receipts {
        id: ProjectId,
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

#[derive(Args)]
struct SubmitArgs {
    #[arg(long)]
    project: ProjectId,
    /// Compute kind (catalog).
    #[arg(long, default_value = "tensor.matmul.v1")]
    kind: String,
    /// Named input files, e.g. `--input a=a.ggt --input b=b.ggt`.
    #[arg(long = "input")]
    inputs: Vec<String>,
    #[arg(long, default_value = "")]
    name: String,
    #[arg(long, default_value_t = 0)]
    priority: i32,
    /// Plan only: show the split and estimate without reserving anything.
    #[arg(long)]
    dry_run: bool,
    /// Idempotency key (safe retries).
    #[arg(long)]
    idempotency_key: Option<String>,
    #[command(flatten)]
    mlp: MlpArgs,
}

/// Hyperparameters of `mlp.train.v1` (ignored for other kinds).
#[derive(Args, Clone)]
struct MlpArgs {
    #[arg(long, default_value_t = 64)]
    hidden: u32,
    #[arg(long, default_value = "adam")]
    optimizer: String,
    #[arg(long, default_value_t = 0.02)]
    lr: f64,
    #[arg(long, default_value_t = 64)]
    batch: u32,
    #[arg(long, default_value_t = 300)]
    steps: u32,
    #[arg(long, default_value_t = 1)]
    seed: u64,
}

#[derive(Args)]
struct SweepArgs {
    #[arg(long)]
    project: ProjectId,
    /// Named input files: train_x, train_y, eval_x, eval_y.
    #[arg(long = "input")]
    inputs: Vec<String>,
    #[arg(long, default_value = "sweep")]
    name: String,
    /// Hidden widths, comma-separated.
    #[arg(long, default_value = "32,64", value_delimiter = ',')]
    hidden: Vec<u32>,
    /// Learning rates, comma-separated.
    #[arg(long, default_value = "0.02,0.05", value_delimiter = ',')]
    lr: Vec<f64>,
    #[arg(long, default_value = "adam")]
    optimizer: String,
    #[arg(long, default_value_t = 64)]
    batch: u32,
    #[arg(long, default_value_t = 300)]
    steps: u32,
    /// Number of seeds per configuration (seeds 1..=n).
    #[arg(long, default_value_t = 3)]
    seeds: u64,
    #[arg(long, default_value_t = 0)]
    priority: i32,
    #[arg(long)]
    dry_run: bool,
    #[arg(long)]
    idempotency_key: Option<String>,
}

#[derive(Args)]
struct CollectiveArgs {
    #[arg(long)]
    project: ProjectId,
    /// Named input files: train_x, train_y (MLP run).
    #[arg(long = "input")]
    inputs: Vec<String>,
    /// Graph run: a `gg-graph/v1` description (see `gg job check-model`). With it, `--dataset`
    /// gives one GGT1 file per graph input and one for the loss target, and `--hidden` is ignored.
    #[arg(long)]
    graph: Option<PathBuf>,
    /// Graph run: `name=file.ggt`, one per graph input and one for the loss target.
    #[arg(long = "dataset")]
    datasets: Vec<String>,
    /// Graph run: start from this GGB1 bundle of parameters instead of the seeded initialization.
    #[arg(long)]
    init_checkpoint: Option<PathBuf>,
    /// Graph run: segments of activation recomputation (1 keeps every activation). Less memory for
    /// about a third more compute, same gradient (ADR-0019).
    #[arg(long, default_value_t = 1)]
    segments: u32,
    /// Graph run: upload parameters segment by segment instead of all at once.
    #[arg(long)]
    offload: bool,
    #[arg(long, default_value = "collective")]
    name: String,
    #[arg(long, default_value_t = 64)]
    hidden: u32,
    #[arg(long, default_value = "adam")]
    optimizer: String,
    #[arg(long, default_value_t = 0.02)]
    lr: f64,
    /// Examples per round (effective batch, split over the round's tasks).
    #[arg(long, default_value_t = 256)]
    batch: u32,
    /// Micro-batch tasks per round.
    #[arg(long, default_value_t = 4)]
    tasks: u32,
    /// Rounds (optimizer steps).
    #[arg(long, default_value_t = 20)]
    rounds: u32,
    #[arg(long, default_value_t = 1)]
    seed: u64,
    /// Retries of a failed round before the run fails.
    #[arg(long, default_value_t = 2)]
    retries: u32,
    #[arg(long, default_value_t = 0)]
    priority: i32,
}

#[derive(Subcommand)]
enum JobCmd {
    /// Start a collective training run (`mlp.grad.v1` rounds aggregated by the server, ADR-0011).
    Collective(CollectiveArgs),
    /// State, rounds and current checkpoint of a collective run.
    CollectiveStatus { run: uuid::Uuid },
    /// Download the current checkpoint of a graph run (a GGB1 bundle of named parameters).
    CollectiveCheckpoint {
        run: uuid::Uuid,
        /// Output file.
        #[arg(long)]
        out: PathBuf,
    },
    /// Cancel a collective run (requires --yes).
    CollectiveCancel {
        run: uuid::Uuid,
        #[arg(long)]
        yes: bool,
    },
    /// Recompute the run locally in one process and compare with the published checkpoint.
    CollectiveCheck {
        run: uuid::Uuid,
        /// Named input files: train_x, train_y (the same data the run was created with).
        #[arg(long = "input")]
        inputs: Vec<String>,
        /// Absolute tolerance.
        #[arg(long, default_value_t = 1e-4)]
        abs: f64,
        /// Relative tolerance.
        #[arg(long, default_value_t = 1e-3)]
        rel: f64,
        /// Write the local reference weights (`w1.ggt`, ...) to this directory.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Submit a grid of independent `mlp.train.v1` trainings as one job.
    Sweep(SweepArgs),
    /// Compare the accepted results of a job's tasks (eval loss per configuration).
    Compare {
        job: JobId,
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Check a model description (`gg-graph/v1`) against the operator profile, without a server.
    ///
    /// Names the missing operator or the shape that does not fit, before any budget is reserved.
    CheckModel {
        /// JSON file describing the graph.
        model: PathBuf,
        /// Batch size used for the memory estimate.
        #[arg(long, default_value_t = 128)]
        batch: u64,
        /// Print the full report as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Plan a distributed training of a model description on contributor machines, without a server.
    ///
    /// Cuts the graph so that each stage fits one machine, then says what sets the pace (the
    /// executor or the links), how long the whole run takes and how many bytes cross the Internet.
    Plan(PlanArgs),
    /// Write a generated model skeleton (for sizing; see gg-core/docs/MODELS.md for what it lacks).
    Template(TemplateArgs),
    /// Evaluate a model description offline: forward pass only, on the CPU reference profile.
    ///
    /// Writes the requested tensor as GGT1. Serves to evaluate a trained checkpoint locally and to
    /// check that a captured PyTorch module computes the same function as its description.
    EvalModel {
        /// JSON file describing the graph.
        model: PathBuf,
        /// GGB1 bundle of parameters; without it, the seeded initialization of the description.
        #[arg(long)]
        checkpoint: Option<PathBuf>,
        /// Seed of the initialization when no checkpoint is given.
        #[arg(long, default_value_t = 1)]
        seed: u64,
        /// `name=file.ggt`, one per graph input (integer indices as integral f32).
        #[arg(long = "input")]
        inputs: Vec<String>,
        /// Tensor to return; defaults to the prediction the loss reads.
        #[arg(long)]
        output: Option<String>,
        /// Output GGT1 file.
        #[arg(long)]
        out: PathBuf,
    },
    /// List the operators this server accepts in a model description.
    Ops {
        /// Print the server answer as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Upload inputs and submit a job.
    Submit(SubmitArgs),
    /// Job status.
    Status { job: JobId },
    /// Jobs of a project.
    List {
        #[arg(long)]
        project: ProjectId,
    },
    /// Cancel a job (requires --yes).
    Cancel {
        job: JobId,
        #[arg(long)]
        yes: bool,
    },
    /// Task detail (spec, attempts, decision).
    Task { task: TaskId },
    /// Download accepted outputs of a task.
    Results {
        task: TaskId,
        #[arg(long)]
        out: PathBuf,
    },
}

#[derive(Subcommand)]
enum WorkerCmd {
    /// Generate an Ed25519 key file (mode 0600).
    Keygen {
        #[arg(long)]
        out: PathBuf,
    },
    /// Register a worker key with the server.
    Register {
        #[arg(long)]
        key: PathBuf,
        #[arg(long, default_value = "worker")]
        name: String,
    },
    /// Declare the address this contributor wants to be paid at (P7 settlement).
    Payout {
        #[arg(long)]
        key: PathBuf,
        #[arg(long)]
        address: String,
    },
    /// List receipts of a worker key (challenge login).
    Receipts {
        #[arg(long)]
        key: PathBuf,
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum ReleaseCmd {
    /// Create the release signing key (mode 0600) on the offline machine and print its public forms.
    /// Refuses to overwrite an existing file.
    Keygen {
        /// Key file to create (typically on the removable medium).
        #[arg(long)]
        out: PathBuf,
    },
    /// Sign an unsigned manifest on an offline machine. This machine needs nothing but this binary
    /// and the key: no build, no network (docs/DECISIONS.md, D3). Next to `--out` it also writes
    /// `SHA256SUMS` and its OpenSSH signature `SHA256SUMS.sshsig`, which a contributor checks with
    /// `ssh-keygen -Y verify` and `sha256sum -c`, without running any downloaded binary.
    Sign {
        /// Path of `release.unsigned.json`, brought from the build machine.
        manifest: PathBuf,
        /// Ed25519 key file (mode 0600), created beforehand with `gg release keygen`. A missing file
        /// is an error: an unmounted medium must not silently yield a new key.
        #[arg(long)]
        key: PathBuf,
        /// Where to write the signed manifest (`release.json`).
        #[arg(long)]
        out: PathBuf,
    },
    /// Add files to an unsigned manifest (for example the GPU build of the worker, renamed
    /// `gg-worker-gpu`). The files must sit next to the manifest; nothing is signed here.
    Add {
        /// Path of `release.unsigned.json`, rewritten in place.
        manifest: PathBuf,
        /// File names, relative to the manifest's directory.
        #[arg(required = true)]
        files: Vec<String>,
    },
    /// Print the `allowed_signers` line (OpenSSH) of a release public key given in hex, for
    /// `ssh-keygen -Y verify`.
    SshKey {
        /// Public key (hex), as printed by `gg release keygen` or `gg release sign`.
        key: String,
    },
    /// Check a signed release manifest and the files next to it, against keys you chose to trust.
    Verify {
        /// Path of `release.json`.
        manifest: PathBuf,
        /// Public key (hex) you trust to publish releases. Repeat for several.
        #[arg(long = "key", required = true)]
        keys: Vec<String>,
        /// Public key (hex) you no longer trust. Repeat for several.
        #[arg(long = "revoked")]
        revoked: Vec<String>,
        /// Network the build targets.
        #[arg(long, default_value = "gg-dev-local")]
        network: String,
        /// Directory holding the binaries (default: next to the manifest).
        #[arg(long)]
        dir: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum ReceiptCmd {
    /// Verify an exported receipts file offline against the server key.
    Verify {
        file: PathBuf,
        #[arg(long)]
        server_key: String,
        #[arg(long)]
        network: String,
    },
}

#[derive(Subcommand)]
enum LedgerCmd {
    /// Budget of a project.
    Balance {
        #[arg(long)]
        project: ProjectId,
    },
}

#[derive(Subcommand)]
enum AdminCmd {
    /// Create a researcher and print its API token once.
    ResearcherCreate {
        #[arg(long)]
        name: String,
        #[arg(long)]
        approve: bool,
    },
    /// List researchers: admission state, ORCID iD or verified e-mail address.
    ResearcherList,
    /// List contributor accounts: address, machines, accepted tasks, suspension.
    ContributorList,
    /// Suspend a contributor account (--reason, --yes): all its machines stop at once. With
    /// --reinstate, authorize them again.
    ContributorSuspend {
        id: uuid::Uuid,
        #[arg(long, default_value = "")]
        reason: String,
        #[arg(long)]
        reinstate: bool,
        #[arg(long)]
        yes: bool,
    },
    /// Set a researcher's admission state.
    ResearcherAdmission {
        id: ResearcherId,
        #[arg(long)]
        state: AdmissionState,
        #[arg(long, default_value = "")]
        note: String,
    },
    /// Create a project.
    ProjectCreate {
        #[arg(long)]
        name: String,
        #[arg(long = "kind")]
        kinds: Vec<String>,
        #[arg(long)]
        owner: Option<ResearcherId>,
        /// State that the submitter accepts the data rule of this deployment (`GET /v1/info`).
        /// Required: a contributor will be able to see everything this project submits.
        #[arg(long)]
        ack_data_policy: bool,
    },
    /// Grant credits to a project.
    ProjectGrant {
        id: ProjectId,
        #[arg(long)]
        amount: Credits,
        #[arg(long, default_value = "")]
        reference: String,
    },
    /// Add a member to a project.
    ProjectMember {
        id: ProjectId,
        #[arg(long)]
        researcher: ResearcherId,
        #[arg(long)]
        role: ProjectRole,
    },
    /// List projects with budgets.
    ProjectList,
    /// List workers.
    WorkerList,
    /// Authorize (or revoke with --revoke --yes) a worker.
    WorkerAuthorize {
        id: WorkerId,
        #[arg(long)]
        revoke: bool,
        #[arg(long)]
        yes: bool,
    },
    /// Withdraw a validator (--revoke --yes): its sessions end and the verifications it holds go
    /// back to the queue. Without --revoke, authorize it again.
    ValidatorAuthorize {
        id: ValidatorId,
        #[arg(long)]
        revoke: bool,
        #[arg(long)]
        yes: bool,
    },
    /// Withdraw an artifact (harmful or published by mistake): its bytes are deleted and the jobs and
    /// runs that still need it are cancelled.
    ArtifactWithdraw {
        hash: Sha256Digest,
        #[arg(long)]
        reason: String,
        #[arg(long)]
        yes: bool,
    },
    /// Register and authorize a validator key.
    ValidatorAdd {
        #[arg(long)]
        key: String,
        #[arg(long, default_value = "validator")]
        name: String,
        /// Operator running this validator. Diversity is counted in operators, not in keys: two
        /// keys of the same operator never verify the same submission (ADR-0014).
        #[arg(long, default_value = "unassigned")]
        operator: String,
    },
    /// Register an operator (administrative entity running validators).
    OperatorCreate {
        #[arg(long)]
        name: String,
        #[arg(long, default_value = "")]
        contact: String,
    },
    /// List operators.
    OperatorList,
    /// Disputes between validators awaiting an operator decision.
    DisputeList {
        /// Include resolved ones.
        #[arg(long)]
        all: bool,
    },
    /// Read or change the share of results re-verified by a distinct operator (0 to 1).
    AuditRate {
        /// New value; omit to read the current one.
        #[arg(long)]
        set: Option<f64>,
        #[arg(long, default_value = "")]
        reason: String,
    },
    /// Prepare a reward batch from accepted contributions (Merkle commitment, ADR-0015).
    BatchCreate {
        #[arg(long)]
        batch_id: String,
        #[arg(long)]
        chain_id: String,
        #[arg(long)]
        contract: String,
        #[arg(long, default_value = "utrg")]
        asset: String,
        #[arg(long, default_value_t = 500)]
        max_leaves: u32,
        /// Show what would be committed without writing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// List reward batches.
    BatchList,
    /// Export a batch with its leaves and proofs (what a beneficiary claims with).
    BatchExport {
        batch_id: String,
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Record a settlement event: submitted, observed, finalized or reconcile.
    BatchSettle {
        batch_id: String,
        #[arg(long)]
        event: String,
        #[arg(long, default_value = "")]
        tx_hash: String,
        #[arg(long)]
        height: Option<i64>,
        /// Root actually seen on chain (required for an observation).
        #[arg(long)]
        observed_root: Option<String>,
        #[arg(long, default_value = "")]
        note: String,
    },
    /// Settle a dispute with an outcome a validator actually signed.
    DisputeResolve {
        dispute: uuid::Uuid,
        /// `accepted`, `rejected` or `inconclusive`.
        #[arg(long)]
        outcome: String,
        #[arg(long, default_value = "")]
        reason: String,
        #[arg(long)]
        yes: bool,
    },
    /// Task counts by state.
    TasksSummary,
    /// Recent audit events.
    Audit,
}

#[derive(Subcommand)]
enum DataCmd {
    /// Print the shapes held by a GGT1 tensor file or a GGB1 bundle.
    Inspect {
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Generate a synthetic regression dataset (train_x/train_y/eval_x/eval_y GGT1 files).
    GenMlp {
        #[arg(long, default_value_t = 16)]
        d_input: u32,
        #[arg(long, default_value_t = 4)]
        d_output: u32,
        #[arg(long, default_value_t = 32)]
        d_teacher: u32,
        #[arg(long, default_value_t = 4096)]
        n_train: u32,
        #[arg(long, default_value_t = 1024)]
        n_eval: u32,
        #[arg(long, default_value_t = 7)]
        seed: u64,
        #[arg(long, default_value_t = 0.05)]
        noise: f32,
        #[arg(long)]
        out_dir: PathBuf,
    },
    /// Generate two GGT1 matrices with the protocol DRBG.
    GenMatmul {
        #[arg(long)]
        m: u32,
        #[arg(long)]
        k: u32,
        #[arg(long)]
        n: u32,
        #[arg(long, default_value_t = 1)]
        seed: u64,
        #[arg(long)]
        out_a: PathBuf,
        #[arg(long)]
        out_b: PathBuf,
    },
}

#[derive(Serialize, Deserialize, Default)]
struct SavedConfig {
    server_url: Option<String>,
    token: Option<String>,
}

fn config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("gpu-goblin").join("cli.toml")
}

fn load_saved() -> SavedConfig {
    std::fs::read_to_string(config_path())
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default()
}

fn save(cfg: &SavedConfig) -> anyhow::Result<()> {
    let p = config_path();
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = toml::to_string(cfg)?;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    use std::io::Write;
    opts.open(&p)?.write_all(text.as_bytes())?;
    println!("saved {}", p.display());
    Ok(())
}

struct Ctx {
    api: Api,
    token: String,
}

impl Ctx {
    fn new(server: Option<String>, token: Option<String>) -> anyhow::Result<Self> {
        let saved = load_saved();
        let server = server.or(saved.server_url).ok_or_else(|| {
            anyhow::anyhow!("no server configured: use --server or `gg auth login`")
        })?;
        let token = token.or(saved.token).unwrap_or_default();
        Ok(Self {
            api: api_for(&server)?,
            token,
        })
    }
    async fn get(&self, path: &str) -> anyhow::Result<Value> {
        Ok(self.api.get_json(&self.token, path).await?)
    }
    async fn post(&self, path: &str, body: Value) -> anyhow::Result<Value> {
        Ok(self
            .api
            .post_json::<Value>(&self.token, path, &body)
            .await?
            .unwrap_or(Value::Null))
    }
}

fn print(v: &Value) {
    println!("{}", serde_json::to_string_pretty(v).unwrap());
}

fn parse_inputs(specs: &[String]) -> anyhow::Result<BTreeMap<String, PathBuf>> {
    let mut m = BTreeMap::new();
    for s in specs {
        let (name, path) = s
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("input must be name=path: {s}"))?;
        m.insert(name.to_owned(), PathBuf::from(path));
    }
    Ok(m)
}

async fn upload_file(
    ctx: &Ctx,
    project: ProjectId,
    path: &Path,
    schema: &str,
) -> anyhow::Result<ArtifactRef> {
    let bytes = std::fs::read(path)?;
    let hash = Sha256Digest::of_bytes(&bytes);
    let size = bytes.len() as u64;
    ctx.api
        .upload(
            &ctx.token,
            &format!("/researcher/projects/{project}/artifacts/{hash}"),
            path,
            &hash,
            size,
            schema,
        )
        .await?;
    Ok(ArtifactRef { hash, size })
}

async fn upload_named_inputs(
    ctx: &Ctx,
    project: ProjectId,
    inputs: &BTreeMap<String, PathBuf>,
    dry_run: bool,
) -> anyhow::Result<BTreeMap<String, ArtifactRef>> {
    let mut refs = BTreeMap::new();
    for (name, path) in inputs {
        let t = TensorF32::read_from_path(path, &Default::default())?;
        t.check_finite()?;
        let r = if dry_run {
            let bytes = t.encode();
            ArtifactRef {
                hash: Sha256Digest::of_bytes(&bytes),
                size: bytes.len() as u64,
            }
        } else {
            upload_file(ctx, project, path, "ggt1-f32").await?
        };
        refs.insert(name.clone(), r);
    }
    Ok(refs)
}

async fn post_job(
    ctx: &Ctx,
    project: ProjectId,
    body: Value,
    idempotency_key: Option<&str>,
) -> anyhow::Result<Value> {
    let path = format!("/researcher/projects/{project}/jobs");
    match idempotency_key {
        Some(key) => {
            let http = reqwest_client()?;
            let r = http
                .post(format!("{}/v1{path}", server_base(ctx)?))
                .bearer_auth(&ctx.token)
                .header("Idempotency-Key", key)
                .json(&body)
                .send()
                .await?;
            let status = r.status();
            let v: Value = r.json().await?;
            anyhow::ensure!(status.is_success(), "server returned {status}: {v}");
            Ok(v)
        }
        None => ctx.post(&path, body).await,
    }
}

fn mlp_params(
    m: &MlpArgs,
    d_input: u32,
    d_output: u32,
    seed: u64,
) -> gg_protocol::messages::MlpTrainParamsV1 {
    gg_protocol::messages::MlpTrainParamsV1 {
        d_input,
        d_hidden: m.hidden,
        d_output,
        optimizer: m.optimizer.clone(),
        learning_rate: m.lr,
        batch_size: m.batch,
        steps: m.steps,
        seed,
    }
}

fn dataset_widths(inputs: &BTreeMap<String, PathBuf>) -> anyhow::Result<(u32, u32)> {
    for n in ["train_x", "train_y", "eval_x", "eval_y"] {
        anyhow::ensure!(inputs.contains_key(n), "--input {n}=FILE required");
    }
    let x = TensorF32::read_from_path(&inputs["train_x"], &Default::default())?;
    let y = TensorF32::read_from_path(&inputs["train_y"], &Default::default())?;
    anyhow::ensure!(
        x.shape.len() == 2 && y.shape.len() == 2 && x.shape[0] == y.shape[0],
        "train_x/train_y must be [n, d] tensors with the same n"
    );
    Ok((x.shape[1] as u32, y.shape[1] as u32))
}

async fn sweep(ctx: &Ctx, a: SweepArgs) -> anyhow::Result<()> {
    let inputs = parse_inputs(&a.inputs)?;
    let (d_input, d_output) = dataset_widths(&inputs)?;
    let refs = upload_named_inputs(ctx, a.project, &inputs, a.dry_run).await?;
    let mut tasks = Vec::new();
    for h in &a.hidden {
        for lr in &a.lr {
            for seed in 1..=a.seeds {
                let m = MlpArgs {
                    hidden: *h,
                    optimizer: a.optimizer.clone(),
                    lr: *lr,
                    batch: a.batch,
                    steps: a.steps,
                    seed,
                };
                tasks.push(gg_compute::catalog::mlp_definition(
                    mlp_params(&m, d_input, d_output, seed),
                    refs.clone(),
                )?);
            }
        }
    }
    anyhow::ensure!(
        !tasks.is_empty() && tasks.len() <= 256,
        "a sweep must contain 1..=256 tasks (got {})",
        tasks.len()
    );
    let body =
        json!({ "name": a.name, "priority": a.priority, "tasks": tasks, "dry_run": a.dry_run });
    print(&post_job(ctx, a.project, body, a.idempotency_key.as_deref()).await?);
    Ok(())
}

async fn collective(ctx: &Ctx, a: CollectiveArgs) -> anyhow::Result<()> {
    let common = json!({
        "name": a.name, "optimizer": a.optimizer, "learning_rate": a.lr, "batch_per_round": a.batch,
        "tasks_per_round": a.tasks, "rounds": a.rounds, "seed": a.seed, "max_round_retries": a.retries,
        "priority": a.priority,
    });
    let mut body = common;
    if let Some(graph_path) = &a.graph {
        // Validate locally first: a rejection here costs neither an upload nor any credits.
        let text = std::fs::read_to_string(graph_path)?;
        let model: gg_compute::graph::ModelGraphV1 = serde_json::from_str(&text)?;
        // Inline functions exactly as the server does (ADR-0025).
        let model = gg_compute::graph::functions::inline(&model)
            .map_err(|e| anyhow::anyhow!("model rejected: {e}"))?;
        let report = gg_compute::graph::validate(&model)
            .map_err(|e| anyhow::anyhow!("model rejected: {e}"))?;
        let datasets = parse_inputs(&a.datasets)?;
        let mut expected: Vec<&str> = model.inputs.iter().map(|i| i.name.as_str()).collect();
        expected.push(model.loss.target().name.as_str());
        for n in &expected {
            anyhow::ensure!(
                datasets.contains_key(*n),
                "--dataset {n}=FILE required by the graph"
            );
        }
        let graph_ref = upload_file(ctx, a.project, graph_path, "gg-graph-v1").await?;
        let dataset_refs = upload_named_inputs(ctx, a.project, &datasets, false).await?;
        body["graph"] = json!(graph_ref);
        body["datasets"] = json!(dataset_refs);
        body["segments"] = json!(a.segments.max(1));
        body["offload_parameters"] = json!(a.offload);
        if let Some(ck) = &a.init_checkpoint {
            let bundle = gg_protocol::tensor_file::BundleF32::read_from_path(
                ck,
                &gg_protocol::tensor_file::TensorLimits {
                    max_elements: report.parameters,
                },
            )?;
            bundle.check_finite()?;
            body["initial_checkpoint"] = json!(upload_file(ctx, a.project, ck, "ggb1-f32").await?);
        }
        eprintln!(
            "graph \"{}\": {} parameters ({} trainable), {} nodes, {} datasets{}",
            report.name,
            report.parameters,
            report.trainable_parameters,
            report.order.len(),
            expected.len(),
            match (a.segments.max(1), a.offload) {
                (1, false) => String::new(),
                (s, o) => format!(
                    "; {s} recomputation segment(s){}",
                    if o { ", parameters offloaded" } else { "" }
                ),
            }
        );
    } else {
        let inputs = parse_inputs(&a.inputs)?;
        for n in ["train_x", "train_y"] {
            anyhow::ensure!(
                inputs.contains_key(n),
                "--input {n}=FILE required (or --graph)"
            );
        }
        let refs = upload_named_inputs(ctx, a.project, &inputs, false).await?;
        body["train_x"] = json!(refs["train_x"]);
        body["train_y"] = json!(refs["train_y"]);
        body["d_hidden"] = json!(a.hidden);
    }
    print(
        &ctx.post(
            &format!("/researcher/projects/{}/collective", a.project),
            body,
        )
        .await?,
    );
    Ok(())
}

async fn download_weights(
    ctx: &Ctx,
    project: &str,
    refs: &Value,
    shapes: &[(&str, Vec<u64>)],
) -> anyhow::Result<gg_compute::mlp::Weights> {
    let mut got: BTreeMap<String, TensorF32> = BTreeMap::new();
    for (name, shape) in shapes {
        let r: ArtifactRef = serde_json::from_value(
            json!({ "hash": refs[name]["hash"], "size": refs[name]["size"] }),
        )
        .map_err(|e| anyhow::anyhow!("checkpoint lacks {name}: {e}"))?;
        let tmp = std::env::temp_dir().join(format!("gg-ckpt-{}-{name}.ggt", r.hash));
        ctx.api
            .download(
                &ctx.token,
                &format!("/researcher/projects/{project}/artifacts/{}", r.hash),
                &r,
                1 << 30,
                &tmp,
            )
            .await?;
        let t = TensorF32::read_from_path(&tmp, &gg_compute::catalog::tensor_limits_for(shape))?;
        let _ = std::fs::remove_file(&tmp);
        anyhow::ensure!(
            t.shape == *shape,
            "{name}: expected shape {shape:?}, got {:?}",
            t.shape
        );
        got.insert((*name).to_owned(), t);
    }
    Ok(gg_compute::mlp::Weights {
        w1: got["w1"].clone(),
        b1: got["b1"].clone(),
        w2: got["w2"].clone(),
        b2: got["b2"].clone(),
    })
}

/// Replays, in a single process, the training of a model described by a graph and compares the
/// result with the published checkpoint. Everything is downloaded from the server: the
/// description, the datasets and the checkpoint. The researcher needs to keep nothing locally in
/// order to verify.
async fn graph_check(
    ctx: &Ctx,
    run: uuid::Uuid,
    v: &Value,
    abs: f64,
    rel: f64,
    out: Option<&Path>,
) -> anyhow::Result<()> {
    use gg_compute::grad::CoordOptim;
    use gg_compute::graph::ModelGraphV1;
    use gg_compute::graph::exec::{self, Prepared};
    use gg_compute::graph::task::{
        NamedOptimizerState, aggregate_named, apply_step_named, dropout_seed, round_indices,
        select_rows,
    };
    use gg_protocol::messages::GraphGradParamsV1;
    use gg_protocol::tensor_file::BundleF32;

    let cfg = &v["config"];
    let g = &cfg["graph"];
    let project = v["project_id"].as_str().unwrap_or_default().to_owned();
    let step = v["step"].as_u64().unwrap_or(0);
    let state = v["state"].as_str().unwrap_or_default();
    let seed = cfg["seed"].as_u64().unwrap_or(0);
    let total = cfg["total_examples"].as_u64().unwrap_or(0);
    let batch = cfg["batch_per_round"].as_u64().unwrap_or(0) as u32;
    let tasks = cfg["tasks_per_round"].as_u64().unwrap_or(0) as u32;
    let lr = cfg["learning_rate"].as_f64().unwrap_or(0.0);
    let optim = CoordOptim::parse(cfg["optimizer"].as_str().unwrap_or_default())
        .ok_or_else(|| anyhow::anyhow!("unknown optimizer in run config"))?;
    // Memory levers do not change the gradient, but the replay reuses them: recomputing without
    // them would require memory the workers did not have (ADR-0019).
    let segments = g["segments"].as_u64().unwrap_or(1).max(1) as u32;
    let offload = g["offload_parameters"].as_bool().unwrap_or(false);

    let fetch = |r: ArtifactRef, max: u64| {
        let project = project.clone();
        async move {
            let tmp = std::env::temp_dir().join(format!("gg-check-{}.bin", r.hash));
            ctx.api
                .download(
                    &ctx.token,
                    &format!("/researcher/projects/{project}/artifacts/{}", r.hash),
                    &r,
                    max,
                    &tmp,
                )
                .await?;
            let bytes = std::fs::read(&tmp)?;
            let _ = std::fs::remove_file(&tmp);
            anyhow::Ok(bytes)
        }
    };
    let as_ref_of = |value: &Value| -> anyhow::Result<ArtifactRef> {
        serde_json::from_value(value.clone()).map_err(|e| anyhow::anyhow!("artifact ref: {e}"))
    };

    let graph_bytes = fetch(
        as_ref_of(&g["graph"])?,
        gg_compute::graph::task::MAX_GRAPH_BYTES,
    )
    .await?;
    let model: ModelGraphV1 = serde_json::from_slice(&graph_bytes)?;
    let report = gg_compute::graph::validate(&model).map_err(|e| anyhow::anyhow!("{e}"))?;
    let prepared = Prepared::new(&model).map_err(|e| anyhow::anyhow!("{e}"))?;

    let mut datasets: BTreeMap<String, TensorF32> = BTreeMap::new();
    for d in g["datasets"].as_array().cloned().unwrap_or_default() {
        let name = d["name"].as_str().unwrap_or_default().to_owned();
        let shape: Vec<u64> = serde_json::from_value(d["shape"].clone())?;
        let limits = gg_compute::catalog::tensor_limits_for(&shape);
        // A single artifact, or consecutive row slices (C4) stitched back together.
        let refs: Vec<ArtifactRef> = match g["dataset_parts"].get(&name) {
            Some(list) => serde_json::from_value(list.clone())?,
            None => vec![as_ref_of(&g["dataset_refs"][&name])?],
        };
        let mut data = Vec::new();
        for r in refs {
            let bytes = fetch(r, gg_compute::graph::task::MAX_DATASET_BYTES).await?;
            data.extend(TensorF32::decode(&bytes, &limits)?.data);
        }
        datasets.insert(name, TensorF32::new(shape, data)?);
    }
    // The published checkpoint, whole or in shards (C2), reassembled.
    let mut published = BTreeMap::new();
    for (name, value) in v["checkpoint"]["weights"]
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("run view lacks checkpoint weights"))?
    {
        anyhow::ensure!(
            name.starts_with("checkpoint"),
            "unexpected checkpoint part '{name}'"
        );
        let bytes = fetch(
            as_ref_of(value)?,
            gg_compute::graph::shards::MAX_SHARD_BYTES,
        )
        .await?;
        published.extend(
            BundleF32::decode(
                &bytes,
                &gg_protocol::tensor_file::TensorLimits {
                    max_elements: report.parameters,
                },
            )?
            .entries,
        );
    }

    // Starting point: the initialization drawn from the seed, as the server did it. A run that
    // started from a supplied checkpoint cannot be replayed this way: it is rejected here rather
    // than compared against a wrong reference.
    let mut weights = exec::init_parameters(&model, seed);
    for (name, t) in &weights {
        let p = published
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("the published checkpoint lacks '{name}'"))?;
        anyhow::ensure!(
            p.shape == t.shape,
            "'{name}': published shape {:?}, the description expects {:?}",
            p.shape,
            t.shape
        );
    }
    let mut state_opt = NamedOptimizerState::default();
    let started = std::time::Instant::now();
    for s in 0..step {
        let round_seed = seed.wrapping_add(s);
        let mut parts = Vec::new();
        for (start, count) in gg_control_slices(batch, tasks) {
            let idx = round_indices(round_seed, total, start, count);
            let mut micro = BTreeMap::new();
            for (name, t) in &datasets {
                micro.insert(name.clone(), select_rows(t, &idx));
            }
            let p = GraphGradParamsV1 {
                graph_hash: String::new(),
                graph_bytes: 0,
                parameters: report.parameters,
                checkpoint_bytes: 0,
                gradient_bytes: 0,
                segments: segments.max(1),
                offload_parameters: offload,
                training_flops_per_example: 0,
                activation_bytes_per_example: 0,
                datasets: vec![],
                round_seed,
                example_start: start,
                example_count: count,
                total_examples: total,
                shard_bytes: 0,
                batch_inputs: false,
                gradient_bf16: false,
            };
            let r = exec::gradient_cpu_with(
                &prepared,
                &weights,
                &micro,
                dropout_seed(&p),
                gg_compute::graph::task::exec_options(&p),
            )
            .map_err(|e| anyhow::anyhow!("recompute: {e}"))?;
            parts.push((count, r.gradients));
        }
        let grad = aggregate_named(&parts)?;
        apply_step_named(optim, lr, &mut weights, &grad, &mut state_opt)?;
    }
    let elapsed = started.elapsed();
    println!(
        "run {run}: state {state}, model \"{}\" ({} parameters), {step} rounds recomputed locally in {:.2}s",
        report.name,
        report.parameters,
        elapsed.as_secs_f64()
    );
    let tol = gg_protocol::messages::ToleranceV1 { abs, rel };
    let mut all_ok = true;
    let mut worst = 0.0f64;
    let mut worst_name = String::new();
    for (name, reference) in &weights {
        let cand = published
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("the published checkpoint lacks '{name}'"))?;
        let cmp = gg_compute::compare::compare_f32(name, cand, reference, tol);
        let measured = cmp
            .checks
            .iter()
            .find(|c| c.name.ends_with("max_abs_diff"))
            .and_then(|c| c.measured)
            .unwrap_or(0.0);
        if measured > worst {
            worst = measured;
            worst_name = name.clone();
        }
        all_ok &= cmp.passed;
        if !cmp.passed {
            for c in cmp.checks.iter().filter(|c| !c.passed) {
                println!("  {} : {}", c.name, c.detail);
            }
        }
    }
    println!(
        "  {} tensors compared, largest absolute difference {worst:.3e} on '{worst_name}' (tolerance abs {abs} + rel {rel}·|ref|)",
        weights.len()
    );
    if let Some(dir) = out {
        std::fs::create_dir_all(dir)?;
        let path = dir.join("reference.ggb");
        std::fs::write(
            &path,
            gg_protocol::tensor_file::BundleF32::new(weights)?.encode(),
        )?;
        println!("  local reference written to {}", path.display());
    }
    println!(
        "collective-check: {}",
        if all_ok { "EQUIVALENT" } else { "DIFFERENT" }
    );
    anyhow::ensure!(
        all_ok,
        "the published checkpoint differs from the local reference"
    );
    Ok(())
}

/// Same slices as the coordinator (`gg_control::workflows::collective::slices`), redefined here so
/// that the CLI does not depend on the server.
fn gg_control_slices(batch: u32, k: u32) -> Vec<(u64, u32)> {
    let base = batch / k;
    let rem = batch % k;
    let mut out = Vec::with_capacity(k as usize);
    let mut start = 0u64;
    for i in 0..k {
        let n = base + u32::from(i < rem);
        out.push((start, n));
        start += n as u64;
    }
    out
}

/// Recomputes the whole run in one process (`reference_training`) from the protocol initial
/// weights and compares it with the checkpoint published by the server.
async fn collective_check(
    ctx: &Ctx,
    run: uuid::Uuid,
    inputs: &[String],
    abs: f64,
    rel: f64,
    out: Option<&Path>,
) -> anyhow::Result<()> {
    use gg_compute::grad::{CoordOptim, reference_training};
    use gg_compute::mlp::{MlpTask, Optim, initial_weights};
    let v = ctx.get(&format!("/researcher/collective/{run}")).await?;
    let cfg = &v["config"];
    let state = v["state"].as_str().unwrap_or_default();
    let step = v["step"].as_u64().unwrap_or(0);
    anyhow::ensure!(
        step > 0,
        "run {run} is {state} at step 0: nothing to check yet"
    );
    let project = v["project_id"].as_str().unwrap_or_default().to_owned();
    if !cfg["graph"].is_null() {
        return graph_check(ctx, run, &v, abs, rel, out).await;
    }
    let inputs = parse_inputs(inputs)?;
    for n in ["train_x", "train_y"] {
        anyhow::ensure!(inputs.contains_key(n), "--input {n}=FILE required");
    }
    let d_in = cfg["d_input"].as_u64().unwrap_or(0) as usize;
    let d_h = cfg["d_hidden"].as_u64().unwrap_or(0) as usize;
    let d_out = cfg["d_output"].as_u64().unwrap_or(0) as usize;
    let total = cfg["total_examples"].as_u64().unwrap_or(0);
    let seed = cfg["seed"].as_u64().unwrap_or(0);
    let batch = cfg["batch_per_round"].as_u64().unwrap_or(0);
    let lr = cfg["learning_rate"].as_f64().unwrap_or(0.0);
    let optim = CoordOptim::parse(cfg["optimizer"].as_str().unwrap_or_default())
        .ok_or_else(|| anyhow::anyhow!("unknown optimizer in run config"))?;
    let train_x = TensorF32::read_from_path(&inputs["train_x"], &Default::default())?;
    let train_y = TensorF32::read_from_path(&inputs["train_y"], &Default::default())?;
    let x_hash = Sha256Digest::of_bytes(&train_x.encode());
    let y_hash = Sha256Digest::of_bytes(&train_y.encode());
    anyhow::ensure!(
        cfg["train_x"]["hash"] == json!(x_hash) && cfg["train_y"]["hash"] == json!(y_hash),
        "local train_x/train_y differ from the run's dataset artifacts"
    );
    let shapes: Vec<(&str, Vec<u64>)> = vec![
        ("w1", vec![d_in as u64, d_h as u64]),
        ("b1", vec![d_h as u64]),
        ("w2", vec![d_h as u64, d_out as u64]),
        ("b2", vec![d_out as u64]),
    ];
    let published = download_weights(ctx, &project, &v["checkpoint"]["weights"], &shapes).await?;
    let init = initial_weights(&MlpTask {
        d_in,
        d_h,
        d_out,
        optimizer: Optim::Sgd,
        lr: 0.0,
        batch: 1,
        steps: 1,
        seed,
        n_train: total as usize,
        n_eval: 0,
    });
    let rounds: Vec<(u64, u64)> = (0..step).map(|s| (seed.wrapping_add(s), batch)).collect();
    let started = std::time::Instant::now();
    let reference = reference_training(&init, optim, lr, &train_x, &train_y, &rounds)?;
    let elapsed = started.elapsed();
    let tol = gg_protocol::messages::ToleranceV1 { abs, rel };
    let mut all_ok = true;
    let mut worst = 0.0f64;
    println!(
        "run {run}: state {state}, {step} rounds recomputed locally in {:.2}s (single process, full batch {batch})",
        elapsed.as_secs_f64()
    );
    for ((name, _), (cand, refw)) in shapes
        .iter()
        .zip(published.named().iter().zip(reference.named().iter()))
    {
        let c = gg_compute::compare::compare_f32(name, cand.1, refw.1, tol);
        let max_abs = c
            .checks
            .iter()
            .find(|k| k.name.ends_with(".max_abs_diff"))
            .and_then(|k| k.measured)
            .unwrap_or(f64::NAN);
        worst = worst.max(max_abs);
        all_ok &= c.passed;
        println!(
            "  {name:<3} max_abs_diff {max_abs:.3e}  {}",
            if c.passed {
                "within tolerance"
            } else {
                "OUT OF TOLERANCE"
            }
        );
    }
    if let Some(dir) = out {
        std::fs::create_dir_all(dir)?;
        for (name, t) in reference.named() {
            std::fs::write(dir.join(format!("{name}.ggt")), t.encode())?;
        }
        println!("  reference weights written to {}", dir.display());
    }
    println!(
        "collective-check: {} (worst max_abs_diff {worst:.3e}, tolerance abs {abs:e} rel {rel:e})",
        if all_ok { "EQUIVALENT" } else { "MISMATCH" }
    );
    anyhow::ensure!(
        all_ok,
        "published checkpoint differs from the single-process reference"
    );
    Ok(())
}

#[derive(clap::Args, Debug)]
struct PlanArgs {
    /// JSON file describing the graph.
    model: PathBuf,
    /// Examples to process in total (dataset size × epochs).
    #[arg(long)]
    examples: u64,
    /// Examples per micro-batch.
    #[arg(long, default_value_t = 8)]
    batch: u64,
    /// Contributors available at the same time.
    #[arg(long)]
    contributors: u64,
    /// Sustained executor throughput of one contributor, in GFLOP/s (measured: ~300 on this stack).
    #[arg(long, default_value_t = 300.0)]
    gflops: f64,
    /// Memory one contributor gives to the model, in GiB.
    #[arg(long, default_value_t = 12.0)]
    memory_gib: f64,
    /// Uplink of one contributor, in Mbit/s (home fiber: 100 to 500; ADSL: 1 to 10).
    #[arg(long, default_value_t = 300.0)]
    uplink_mbit: f64,
    /// Downlink of one contributor, in Mbit/s.
    #[arg(long, default_value_t = 1000.0)]
    downlink_mbit: f64,
    /// Micro-batches computed locally between two parameter synchronizations.
    #[arg(long, default_value_t = 1)]
    local_steps: u64,
    /// Share of micro-batches recomputed by a validator.
    #[arg(long, default_value_t = 0.25)]
    audit_rate: f64,
    /// Where activations travel between stages: `peer` (stage to stage) or `coordinator`
    /// (every packet returns to the source, which emits the next one).
    #[arg(long, default_value = "peer")]
    topology: String,
    /// Coordinator uplink in Mbit/s (topology `coordinator` only).
    #[arg(long, default_value_t = 300.0)]
    coordinator_uplink_mbit: f64,
    /// Coordinator downlink in Mbit/s (topology `coordinator` only).
    #[arg(long, default_value_t = 1000.0)]
    coordinator_downlink_mbit: f64,
    /// Recompute activations during the backward pass instead of keeping them all.
    #[arg(long)]
    recompute: bool,
    /// Fraction of parameters that get a gradient and optimizer state (1 = all, 0.01 ≈ adapters).
    #[arg(long, default_value_t = 1.0)]
    trainable_fraction: f64,
    /// Host memory a contributor offers for offloading parameters and optimizer state, in GiB.
    #[arg(long, default_value_t = 0.0)]
    host_offload_gib: f64,
    /// Host-to-GPU link, in GB/s (PCIe 4.0 ×16 sustains about 16).
    #[arg(long, default_value_t = 16.0)]
    host_link_gbs: f64,
    /// Print the plan as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args, Debug)]
struct TemplateArgs {
    /// Kind of skeleton. Only `transformer` exists.
    kind: String,
    #[arg(long, default_value_t = 12)]
    layers: u64,
    #[arg(long, default_value_t = 768)]
    dim: u64,
    #[arg(long, default_value_t = 12)]
    heads: u64,
    #[arg(long, default_value_t = 1024)]
    seq: u64,
    #[arg(long, default_value_t = 32000)]
    vocab: u64,
    /// Feed-forward width; defaults to 4 × dim.
    #[arg(long)]
    ffn: Option<u64>,
    /// Let every position attend to every other one (an encoder). By default the transformer is
    /// causal: a position only sees the ones before it, which next-token prediction requires.
    #[arg(long)]
    bidirectional: bool,
    /// Position encoding: `rotary` (RoPE, as in Llama), `learned` (one vector per position, as in
    /// GPT-2) or `none`.
    #[arg(long, default_value = "rotary")]
    positions: String,
    /// Frequency base of the rotary encoding (10000 when neither this nor `rope_theta` in
    /// `--rope-scaling` gives one).
    #[arg(long)]
    rope_base: Option<f64>,
    /// Rotary scaling, as the `rope_scaling` (transformers 4) or `rope_parameters` (transformers 5)
    /// object of a Hugging Face `config.json`: inline JSON, or `@file.json`. Variants: linear,
    /// dynamic, yarn, longrope, llama3.
    #[arg(long)]
    rope_scaling: Option<String>,
    /// The model's `max_position_embeddings`, which dynamic NTK, YaRN and LongRoPE consult;
    /// defaults to `--seq`.
    #[arg(long)]
    rope_max_positions: Option<u64>,
    /// Output file.
    #[arg(long)]
    out: PathBuf,
}

/// Offline distribution plan.
fn plan_model(a: &PlanArgs) -> anyhow::Result<()> {
    use anyhow::Context as _;
    use gg_compute::graph::plan::{
        self, Bound, Machine, MemoryPolicy, Topology, Workload, human_bytes, human_duration,
    };
    use gg_compute::graph::{self, ModelGraphV1};

    let text = std::fs::read_to_string(&a.model)
        .with_context(|| format!("reading {}", a.model.display()))?;
    let model: ModelGraphV1 = serde_json::from_str(&text).with_context(|| {
        format!(
            "{} is not a readable gg-graph/v1 description",
            a.model.display()
        )
    })?;
    // Inline functions exactly as the server does (ADR-0025).
    let model = gg_compute::graph::functions::inline(&model)
        .map_err(|e| anyhow::anyhow!("model rejected: {e}"))?;
    let report = match graph::validate(&model) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("model rejected: {e}");
            std::process::exit(2);
        }
    };
    let machine = Machine {
        flops_per_s: a.gflops * 1e9,
        memory_bytes: (a.memory_gib * 1024.0 * 1024.0 * 1024.0) as u64,
        uplink_bytes_per_s: a.uplink_mbit * 1e6 / 8.0,
        downlink_bytes_per_s: a.downlink_mbit * 1e6 / 8.0,
        host_link_bytes_per_s: a.host_link_gbs * 1e9,
    };
    let topology = match a.topology.as_str() {
        "peer" | "chain" => Topology::Peer,
        "coordinator" | "star" => Topology::Coordinator,
        other => anyhow::bail!("unknown topology '{other}'; expected `peer` or `coordinator`"),
    };
    let work = Workload {
        examples: a.examples,
        batch: a.batch,
        contributors: a.contributors,
        local_steps: a.local_steps,
        audit_rate: a.audit_rate,
        topology,
        memory: MemoryPolicy {
            trainable_fraction: a.trainable_fraction,
            recompute_activations: a.recompute,
            host_offload_bytes: (a.host_offload_gib * 1024.0 * 1024.0 * 1024.0) as u64,
        },
        coordinator_link: (topology == Topology::Coordinator).then(|| {
            (
                a.coordinator_uplink_mbit * 1e6 / 8.0,
                a.coordinator_downlink_mbit * 1e6 / 8.0,
            )
        }),
    };
    let p = match plan::plan(&report, &machine, &work) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("no plan: {e}");
            std::process::exit(3);
        }
    };
    if a.json {
        print(&serde_json::to_value(&p)?);
        return Ok(());
    }
    println!(
        "model \"{}\": {} parameters ({}), {} nodes",
        report.name,
        report.parameters,
        human_bytes(report.parameter_bytes),
        report.order.len()
    );
    println!(
        "machine: {:.0} GFLOP/s, {:.1} GiB, {:.0}/{:.0} Mbit/s up/down",
        a.gflops, a.memory_gib, a.uplink_mbit, a.downlink_mbit
    );
    println!(
        "workload: {} examples, micro-batch {}, {} contributors, {} local steps, audit {:.0}%",
        a.examples,
        a.batch,
        a.contributors,
        a.local_steps,
        a.audit_rate * 100.0
    );
    println!();
    println!("stages: {}", p.stages.len());
    for (i, s) in p.stages.iter().enumerate() {
        let host = if s.host_bytes > 0 {
            format!("  host {:>10}", human_bytes(s.host_bytes))
        } else {
            String::new()
        };
        println!(
            "  stage {:<3} nodes {:>4}..{:<4} parameters {:>10}  GPU memory {:>10}{host}  compute/micro-batch {:>8.2} s",
            i,
            s.first,
            s.last,
            human_bytes(s.parameter_bytes),
            human_bytes(s.memory_bytes),
            s.compute_s_per_batch
        );
    }
    for c in &p.cuts {
        println!(
            "  cut after '{}': {} per example ({} tensors), transfer/micro-batch {:.2} s, {:.0} FLOP/byte",
            c.after,
            human_bytes(c.bytes_per_example),
            c.tensors.len(),
            c.transfer_s_per_batch,
            c.intensity_flop_per_byte
        );
    }
    println!();
    if p.coordinator_bytes_per_micro_batch > 0 {
        println!(
            "coordinator: {} per micro-batch, {:.2} s ({:.0}/{:.0} Mbit/s) — global ceiling",
            human_bytes(p.coordinator_bytes_per_micro_batch),
            p.coordinator_s_per_micro_batch,
            a.coordinator_uplink_mbit,
            a.coordinator_downlink_mbit
        );
    }
    println!(
        "pace: {:.2} s per micro-batch per pipeline, set by {}",
        p.s_per_micro_batch,
        match p.bound {
            Bound::Compute => "the executor",
            Bound::Network => "the network link",
        }
    );
    println!(
        "pipelines: {} ({} contributors used), micro-batches including audit: {}",
        p.pipelines, p.contributors_used, p.micro_batches
    );
    println!(
        "synchronization per machine: {} in {}",
        human_bytes(p.sync_bytes_per_machine),
        human_duration(p.sync_s_per_machine)
    );
    println!(
        "total Internet traffic: {}",
        human_bytes(p.total_network_bytes)
    );
    println!("ESTIMATED DURATION: {}", human_duration(p.wall_s));
    let reference = plan::single_accelerator_s(&report, &work, 150e12);
    println!(
        "for comparison, a single datacenter card at 150 TFLOP/s: {}",
        human_duration(reference)
    );
    for n in &p.notes {
        println!("  - {n}");
    }
    Ok(())
}

/// Writes a generated skeleton.
fn write_template(a: &TemplateArgs) -> anyhow::Result<()> {
    use gg_compute::graph::templates::{TransformerDims, transformer};
    anyhow::ensure!(
        a.kind == "transformer",
        "unknown skeleton '{}'; only `transformer` exists",
        a.kind
    );
    let dims = TransformerDims {
        layers: a.layers,
        dim: a.dim,
        heads: a.heads,
        seq: a.seq,
        vocab: a.vocab,
        ffn: a.ffn.unwrap_or(4 * a.dim),
        causal: !a.bidirectional,
        positions: match a.positions.as_str() {
            "none" => gg_compute::graph::templates::Positions::None,
            "learned" => gg_compute::graph::templates::Positions::Learned,
            "rotary" | "rope" => rotary_positions(a)?,
            other => {
                anyhow::bail!(
                    "unknown position encoding '{other}': expected rotary, learned or none"
                )
            }
        },
    };
    if !matches!(
        dims.positions,
        gg_compute::graph::templates::Positions::Rotary { .. }
    ) {
        anyhow::ensure!(
            a.rope_base.is_none() && a.rope_scaling.is_none() && a.rope_max_positions.is_none(),
            "--rope-base, --rope-scaling and --rope-max-positions only apply with --positions rotary"
        );
    }
    let model = transformer(dims).map_err(anyhow::Error::msg)?;
    let report = gg_compute::graph::validate(&model)
        .map_err(|e| anyhow::anyhow!("the generated skeleton is invalid, this is a bug: {e}"))?;
    std::fs::write(&a.out, serde_json::to_string_pretty(&model)?)?;
    println!(
        "{} written: {} nodes, {} parameters",
        a.out.display(),
        report.order.len(),
        report.parameters
    );
    Ok(())
}

/// RoPE encoding of the skeleton: base, and scaling read as in a `config.json`.
fn rotary_positions(a: &TemplateArgs) -> anyhow::Result<gg_compute::graph::templates::Positions> {
    use anyhow::Context as _;
    use gg_compute::graph::rope::RopeScaling;
    let (scaling, theta) = match &a.rope_scaling {
        None => (RopeScaling::None, None),
        Some(text) => {
            let text = match text.strip_prefix('@') {
                Some(path) => {
                    std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?
                }
                None => text.clone(),
            };
            let value: serde_json::Value = serde_json::from_str(&text)
                .context("--rope-scaling is not a readable JSON object")?;
            RopeScaling::from_hugging_face(&value, a.rope_max_positions.unwrap_or(a.seq))
                .map_err(|e| anyhow::anyhow!("--rope-scaling rejected: {e}"))?
        }
    };
    let base = match (a.rope_base, theta) {
        (Some(b), Some(t)) if b != t => {
            anyhow::bail!("--rope-base {b} contradicts rope_theta {t} from --rope-scaling")
        }
        (Some(b), _) => b,
        (None, Some(t)) => t,
        (None, None) => 10000.0,
    };
    Ok(gg_compute::graph::templates::Positions::Rotary { base, scaling })
}

/// Offline forward pass on the reference profile.
fn eval_model(
    model: &Path,
    checkpoint: Option<&Path>,
    seed: u64,
    inputs: &[String],
    output: Option<&str>,
    out: &Path,
) -> anyhow::Result<()> {
    use anyhow::Context as _;
    use gg_compute::graph::exec::{self, Prepared};
    use gg_compute::graph::{self, ModelGraphV1};
    use gg_protocol::tensor_file::{BundleF32, TensorLimits};

    let text =
        std::fs::read_to_string(model).with_context(|| format!("reading {}", model.display()))?;
    let graph: ModelGraphV1 = serde_json::from_str(&text).with_context(|| {
        format!(
            "{} is not a readable gg-graph/v1 description",
            model.display()
        )
    })?;
    // Inline functions exactly as the server does (ADR-0025).
    let graph = gg_compute::graph::functions::inline(&graph)
        .map_err(|e| anyhow::anyhow!("model rejected: {e}"))?;
    let prepared = Prepared::new(&graph).map_err(|e| anyhow::anyhow!("model rejected: {e}"))?;
    let params = match checkpoint {
        Some(path) => {
            let bundle = BundleF32::read_from_path(
                path,
                &TensorLimits {
                    max_elements: prepared.report().parameters,
                },
            )?;
            graph::task::check_bundle_matches(&graph, &bundle, "checkpoint")?;
            bundle.entries
        }
        None => exec::init_parameters(&graph, seed),
    };
    let mut named = BTreeMap::new();
    for (name, path) in parse_inputs(inputs)? {
        let t = TensorF32::read_from_path(&path, &Default::default())
            .with_context(|| format!("input '{name}'"))?;
        named.insert(name, t);
    }
    let output = output.unwrap_or(graph.loss.prediction()).to_owned();
    let t = exec::forward_cpu(&prepared, &params, &named, seed, &output)
        .map_err(|e| anyhow::anyhow!("evaluation: {e}"))?;
    std::fs::write(out, t.encode())?;
    println!("{}: '{output}' with shape {:?}", out.display(), t.shape);
    Ok(())
}

/// Checks a model description offline, before any budget is reserved.
fn check_model(path: &Path, batch: u64, as_json: bool) -> anyhow::Result<()> {
    use anyhow::Context as _;
    use gg_compute::graph::{self, ModelGraphV1};

    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let model: ModelGraphV1 = serde_json::from_str(&text).with_context(|| {
        format!(
            "{} is not a readable gg-graph/v1 description",
            path.display()
        )
    })?;
    // Inline functions exactly as the server does (ADR-0025).
    let model = gg_compute::graph::functions::inline(&model)
        .map_err(|e| anyhow::anyhow!("model rejected: {e}"))?;

    let report = match graph::validate(&model) {
        Ok(report) => report,
        Err(error) => {
            eprintln!("model rejected: {error}");
            if let graph::GraphError::UnsupportedOp { suggestions, .. } = &error
                && suggestions.is_empty()
            {
                eprintln!("the full profile is available with `gg job ops` or at /v1/catalog/ops");
            }
            std::process::exit(2);
        }
    };

    if as_json {
        print(&serde_json::to_value(&report)?);
        return Ok(());
    }
    let mib = |bytes: u64| bytes as f64 / (1024.0 * 1024.0);
    let flop = |v: u64| match v {
        0..=999_999 => format!("{:.1} kFLOP", v as f64 / 1e3),
        1_000_000..=999_999_999 => format!("{:.1} MFLOP", v as f64 / 1e6),
        _ => format!("{:.3} GFLOP", v as f64 / 1e9),
    };
    println!(
        "model \"{}\" accepted by profile {}",
        report.name, report.profile
    );
    println!("  nodes                 : {}", report.order.len());
    println!("  operators             : {}", report.ops_used.join(", "));
    println!(
        "  parameters            : {} ({:.1} MiB), {} trainable ({:.1} MiB exchanged)",
        report.parameters,
        mib(report.parameter_bytes),
        report.trainable_parameters,
        mib(report.trainable_parameter_bytes)
    );
    println!(
        "  compute per example   : {} training ({} forward only)",
        flop(report.training_flops_per_example),
        flop(report.forward_flops_per_example)
    );
    println!(
        "  memory, batch {batch:<5}   : {:.1} MiB, excluding optimizer state",
        mib(report.memory_bytes(batch))
    );
    if !report.tolerance_ops.is_empty() {
        println!(
            "  reproducible within a tolerance, because of: {}",
            report.tolerance_ops.join(", ")
        );
    }
    Ok(())
}

/// Operator profile accepted by this server.
async fn ops(ctx: &Ctx, as_json: bool) -> anyhow::Result<()> {
    let v = ctx.get("/catalog/ops").await?;
    if as_json {
        print(&v);
        return Ok(());
    }
    println!(
        "profile {} — format {}",
        v["profile"].as_str().unwrap_or("?"),
        v["format"].as_str().unwrap_or("?")
    );
    for op in v["operators"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
        println!(
            "  {:<12} {:<28} {}",
            op["name"].as_str().unwrap_or("?"),
            op["onnx"].as_str().unwrap_or("-"),
            op["summary"].as_str().unwrap_or("")
        );
    }
    println!(
        "limits: {} nodes, {} parameters, rank {}",
        v["max_nodes"], v["max_parameters"], v["max_rank"]
    );
    Ok(())
}

async fn compare(ctx: &Ctx, job: JobId, out: Option<&Path>) -> anyhow::Result<()> {
    let st = ctx.get(&format!("/researcher/jobs/{job}")).await?;
    let project = st["project_id"].as_str().unwrap_or_default().to_owned();
    let mut rows = Vec::new();
    for t in st["tasks"].as_array().cloned().unwrap_or_default() {
        let task_id = t["task_id"].as_str().unwrap_or_default().to_owned();
        let d = ctx.get(&format!("/researcher/tasks/{task_id}")).await?;
        let params = d["spec"]["definition"]["params"].clone();
        let mut row = json!({ "task_id": task_id, "index": t["index"], "state": t["state"], "attempts": t["attempts_made"], "params": params, "eval_loss": Value::Null, "final_train_loss": Value::Null, "profile": Value::Null });
        if t["state"] == "accepted"
            && let Some(m) = d["accepted_outputs"]
                .as_array()
                .and_then(|a| a.iter().find(|o| o["name"] == "metrics"))
        {
            let r: ArtifactRef =
                serde_json::from_value(json!({ "hash": m["hash"], "size": m["size"] }))?;
            let tmp = std::env::temp_dir().join(format!("gg-metrics-{task_id}.json"));
            ctx.api
                .download(
                    &ctx.token,
                    &format!("/researcher/projects/{project}/artifacts/{}", r.hash),
                    &r,
                    1 << 20,
                    &tmp,
                )
                .await?;
            let metrics: Value = serde_json::from_slice(&std::fs::read(&tmp)?)?;
            let _ = std::fs::remove_file(&tmp);
            row["eval_loss"] = metrics["eval_loss"].clone();
            row["final_train_loss"] = metrics["final_train_loss"].clone();
            row["profile"] = metrics["numeric_profile"].clone();
        }
        rows.push(row);
    }
    rows.sort_by(|a, b| {
        let fa = a["eval_loss"].as_f64().unwrap_or(f64::INFINITY);
        let fb = b["eval_loss"].as_f64().unwrap_or(f64::INFINITY);
        fa.partial_cmp(&fb).unwrap_or(std::cmp::Ordering::Equal)
    });
    println!(
        "{:<5} {:<10} {:<8} {:<6} {:<8} {:<5} {:<12} {:<12} profile",
        "idx", "state", "hidden", "opt", "lr", "seed", "eval_loss", "train_loss"
    );
    for r in &rows {
        let p = &r["params"];
        println!(
            "{:<5} {:<10} {:<8} {:<6} {:<8} {:<5} {:<12} {:<12} {}",
            r["index"],
            r["state"].as_str().unwrap_or("?"),
            p["d_hidden"],
            p["optimizer"].as_str().unwrap_or("?"),
            p["learning_rate"],
            p["seed"],
            r["eval_loss"]
                .as_f64()
                .map(|v| format!("{v:.5}"))
                .unwrap_or("-".into()),
            r["final_train_loss"]
                .as_f64()
                .map(|v| format!("{v:.5}"))
                .unwrap_or("-".into()),
            r["profile"].as_str().unwrap_or("-")
        );
    }
    let report = json!({ "job_id": job, "state": st["state"], "rows": rows });
    if let Some(p) = out {
        std::fs::write(p, serde_json::to_string_pretty(&report)?)?;
        println!("wrote {}", p.display());
    }
    Ok(())
}

async fn submit(ctx: &Ctx, a: SubmitArgs) -> anyhow::Result<()> {
    let inputs = parse_inputs(&a.inputs)?;
    if a.kind == "mlp.train.v1" {
        let (d_input, d_output) = dataset_widths(&inputs)?;
        let refs = upload_named_inputs(ctx, a.project, &inputs, a.dry_run).await?;
        let def = gg_compute::catalog::mlp_definition(
            mlp_params(&a.mlp, d_input, d_output, a.mlp.seed),
            refs,
        )?;
        let body =
            json!({ "name": a.name, "priority": a.priority, "tasks": [def], "dry_run": a.dry_run });
        if a.idempotency_key.is_some() {
            anyhow::ensure!(!a.dry_run, "idempotency key is meaningless with --dry-run");
        }
        print(&post_job(ctx, a.project, body, a.idempotency_key.as_deref()).await?);
        return Ok(());
    }
    anyhow::ensure!(
        a.kind == "tensor.matmul.v1",
        "unknown compute kind {} (catalog: {:?})",
        a.kind,
        gg_compute::catalog::KINDS
    );
    let pa = inputs
        .get("a")
        .ok_or_else(|| anyhow::anyhow!("--input a=FILE required"))?;
    let pb = inputs
        .get("b")
        .ok_or_else(|| anyhow::anyhow!("--input b=FILE required"))?;
    let ta = TensorF32::read_from_path(pa, &Default::default())?;
    let tb = TensorF32::read_from_path(pb, &Default::default())?;
    ta.check_finite()?;
    tb.check_finite()?;
    anyhow::ensure!(
        ta.shape.len() == 2 && tb.shape.len() == 2 && ta.shape[1] == tb.shape[0],
        "shapes {:?} × {:?} are not compatible",
        ta.shape,
        tb.shape
    );
    let (m, k, n) = (ta.shape[0] as u32, ta.shape[1] as u32, tb.shape[1] as u32);
    let ra = if a.dry_run {
        ArtifactRef {
            hash: Sha256Digest::of_bytes(&ta.encode()),
            size: ta.encode().len() as u64,
        }
    } else {
        upload_file(ctx, a.project, pa, "ggt1-f32").await?
    };
    let rb = if a.dry_run {
        ArtifactRef {
            hash: Sha256Digest::of_bytes(&tb.encode()),
            size: tb.encode().len() as u64,
        }
    } else {
        upload_file(ctx, a.project, pb, "ggt1-f32").await?
    };
    let def = gg_compute::catalog::matmul_definition(m, k, n, ra, rb)?;
    let body =
        json!({ "name": a.name, "priority": a.priority, "tasks": [def], "dry_run": a.dry_run });
    let path = format!("/researcher/projects/{}/jobs", a.project);
    let v = if let Some(key) = &a.idempotency_key {
        anyhow::ensure!(!a.dry_run, "idempotency key is meaningless with --dry-run");
        let http = reqwest_client()?;
        let r = http
            .post(format!("{}/v1{path}", server_base(ctx)?))
            .bearer_auth(&ctx.token)
            .header("Idempotency-Key", key)
            .json(&body)
            .send()
            .await?;
        let status = r.status();
        let v: Value = r.json().await?;
        anyhow::ensure!(status.is_success(), "server returned {status}: {v}");
        v
    } else {
        ctx.post(&path, body).await?
    };
    print(&v);
    Ok(())
}

fn reqwest_client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()?)
}

/// Client for the control plane. Tokens travel on this connection, so plain `http://` is refused
/// to another machine unless `GG_ALLOW_INSECURE_HTTP=1` (a development grid on a private network).
fn api_for(server: &str) -> anyhow::Result<Api> {
    let insecure = std::env::var("GG_ALLOW_INSECURE_HTTP").is_ok_and(|v| v == "1");
    Ok(if insecure {
        Api::new_allowing_plain_http(server)?
    } else {
        Api::new(server)?
    })
}

fn server_base(_ctx: &Ctx) -> anyhow::Result<String> {
    let saved = load_saved();
    std::env::var("GG_SERVER_URL")
        .ok()
        .or(saved.server_url)
        .ok_or_else(|| anyhow::anyhow!("no server configured"))
}

async fn results(ctx: &Ctx, task: TaskId, out: &Path) -> anyhow::Result<()> {
    let d = ctx.get(&format!("/researcher/tasks/{task}")).await?;
    anyhow::ensure!(
        d["task"]["state"] == "accepted",
        "task is {} (results are available only once accepted)",
        d["task"]["state"]
    );
    let project = d["project_id"].as_str().unwrap_or_default().to_owned();
    std::fs::create_dir_all(out)?;
    for o in d["accepted_outputs"]
        .as_array()
        .cloned()
        .unwrap_or_default()
    {
        let name = o["name"].as_str().unwrap_or("out").to_owned();
        let r: ArtifactRef =
            serde_json::from_value(json!({ "hash": o["hash"], "size": o["size"] }))?;
        let dest = out.join(format!("{name}.ggt"));
        ctx.api
            .download(
                &ctx.token,
                &format!("/researcher/projects/{project}/artifacts/{}", r.hash),
                &r,
                16 << 30,
                &dest,
            )
            .await?;
        println!(
            "{name}: {} ({} bytes, sha256 {})",
            dest.display(),
            r.size,
            r.hash
        );
    }
    if let Some(receipt) = d["receipt_id"].as_str() {
        println!("receipt: {receipt}");
    }
    Ok(())
}

/// Sign a manifest built elsewhere. Nothing is read from the network and no file is checked here:
/// the offline machine attests the manifest it was handed, and the verification of the bytes happens
/// where the binaries are.
fn sign_release(manifest_path: &Path, key_path: &Path, out: &Path) -> anyhow::Result<()> {
    use gg_protocol::release::ReleaseManifestV1;

    let text = std::fs::read_to_string(manifest_path)?;
    let manifest: ReleaseManifestV1 = serde_json::from_str(&text)?;
    manifest
        .validate_structure()
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    use gg_protocol::release::{
        CHECKSUMS_FILE, CHECKSUMS_SIG_FILE, SSHSIG_NAMESPACE, allowed_signers_line, checksums_text,
        sshsig_sign,
    };

    anyhow::ensure!(
        key_path.is_file(),
        "{} does not exist: is the medium mounted? A new key is only created by `gg release keygen`",
        key_path.display()
    );
    let key = SecretKey::load_from_file(key_path)?;
    let network = manifest.network.clone();
    let signed = Signed::sign(&key, &network, Domain::Release, manifest)?;
    let json = serde_json::to_string_pretty(&signed)? + "\n";
    // The same key also signs a checksum list in the OpenSSH format, so that a contributor can check
    // the release with ssh-keygen and sha256sum before running anything that was downloaded.
    let manifest_name = out
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow::anyhow!("--out must name a file"))?;
    let sums = checksums_text(&signed.body, manifest_name, json.as_bytes())?;
    let sig = sshsig_sign(&key, SSHSIG_NAMESPACE, sums.as_bytes());
    let dir = out.parent().unwrap_or_else(|| Path::new(""));
    std::fs::write(out, &json)?;
    std::fs::write(dir.join(CHECKSUMS_FILE), &sums)?;
    std::fs::write(dir.join(CHECKSUMS_SIG_FILE), &sig)?;
    println!(
        "signed manifest: {}\nchecksums: {} and {}\npublic key (publish out of band): {}\nallowed_signers: {}\nversion {}, commit {}, {} files, network {}",
        out.display(),
        dir.join(CHECKSUMS_FILE).display(),
        dir.join(CHECKSUMS_SIG_FILE).display(),
        key.public_key(),
        allowed_signers_line(&key.public_key()),
        signed.body.version,
        signed.body.git_commit,
        signed.body.artifacts.len(),
        signed.body.network
    );
    Ok(())
}

/// Create the release key on the offline machine. Never overwrites: losing a release key by
/// accident is a revocation.
fn release_keygen(out: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(
        !out.exists(),
        "{} already exists: refusing to replace a release key",
        out.display()
    );
    let key = SecretKey::generate()?;
    key.save_to_file(out)?;
    let public = key.public_key();
    println!(
        "release key written to {} (mode 0600): keep it offline\npublic key (hex, for gg release verify): {public}\nallowed_signers (for ssh-keygen -Y verify): {}",
        out.display(),
        gg_protocol::release::allowed_signers_line(&public)
    );
    Ok(())
}

/// Add files that sit next to an unsigned manifest to it, keeping names sorted and unique.
fn release_add(manifest_path: &Path, files: &[String]) -> anyhow::Result<()> {
    use gg_protocol::release::{ReleaseArtifactV1, ReleaseManifestV1, hash_file};

    let text = std::fs::read_to_string(manifest_path)?;
    let mut manifest: ReleaseManifestV1 = serde_json::from_str(&text)?;
    let dir = manifest_path.parent().unwrap_or_else(|| Path::new(""));
    for name in files {
        anyhow::ensure!(
            !manifest.artifacts.iter().any(|a| &a.name == name),
            "{name} is already in the manifest"
        );
        let (size, sha256) = hash_file(&dir.join(name))?;
        println!("  {name}: {size} bytes, sha256 {sha256}");
        manifest.artifacts.push(ReleaseArtifactV1 {
            name: name.clone(),
            size,
            sha256,
        });
    }
    manifest.artifacts.sort_by(|a, b| a.name.cmp(&b.name));
    manifest
        .validate_structure()
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    std::fs::write(
        manifest_path,
        serde_json::to_string_pretty(&manifest)? + "\n",
    )?;
    println!(
        "{}: {} files, still unsigned",
        manifest_path.display(),
        manifest.artifacts.len()
    );
    Ok(())
}

/// Check a release before running it: signature from a key **you** trust, then the bytes on disk.
/// Nothing is installed or replaced here; trust is a decision, so the keys come from the command
/// line, never from the manifest.
fn verify_release(
    manifest_path: &Path,
    keys: &[String],
    revoked: &[String],
    network: &str,
    dir: Option<&Path>,
) -> anyhow::Result<()> {
    use gg_protocol::release::{ReleaseManifestV1, check_files, verify_manifest};

    let net = NetworkId::new(network)?;
    let trusted: Vec<PublicKey> = keys
        .iter()
        .map(|k| k.parse())
        .collect::<Result<_, _>>()
        .map_err(|e| anyhow::anyhow!("trusted key: {e}"))?;
    let revoked: Vec<PublicKey> = revoked
        .iter()
        .map(|k| k.parse())
        .collect::<Result<_, _>>()
        .map_err(|e| anyhow::anyhow!("revoked key: {e}"))?;
    let text = std::fs::read_to_string(manifest_path)?;
    let signed: Signed<ReleaseManifestV1> = serde_json::from_str(&text)?;
    verify_manifest(&signed, &net, &trusted, &revoked).map_err(|e| anyhow::anyhow!("{e}"))?;
    let dir = dir
        .map(Path::to_path_buf)
        .or_else(|| manifest_path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    check_files(&signed.body, &dir).map_err(|e| anyhow::anyhow!("{e}"))?;
    println!(
        "release {} ({} files) signed by {} and intact in {}",
        signed.body.version,
        signed.body.artifacts.len(),
        signed.signer,
        dir.display()
    );
    println!(
        "  commit {}  toolchain {}  network {}",
        signed.body.git_commit, signed.body.toolchain, signed.body.network
    );
    for a in &signed.body.artifacts {
        println!("  {:<12} {:>10} bytes  {}", a.name, a.size, a.sha256);
    }
    Ok(())
}

fn verify_receipts(file: &Path, server_key: &str, network: &str) -> anyhow::Result<()> {
    let key: PublicKey = server_key.parse()?;
    let net = NetworkId::new(network)?;
    let text = std::fs::read_to_string(file)?;
    let list: Vec<Value> = serde_json::from_str(&text)?;
    let mut ok = 0;
    for item in &list {
        let signed: Signed<ContributionReceiptV1> =
            serde_json::from_value(item["receipt"].clone())?;
        let canonical = hex::decode(item["canonical_hex"].as_str().unwrap_or_default())?;
        let from_bytes: Signed<ContributionReceiptV1> = Signed::from_canonical_bytes(&canonical)?;
        anyhow::ensure!(
            from_bytes == signed,
            "receipt {}: JSON and canonical bytes differ",
            item["receipt_id"]
        );
        anyhow::ensure!(
            signed.signer == key,
            "receipt {}: signed by {} not the server key",
            item["receipt_id"],
            signed.signer
        );
        signed.verify(&net, Domain::Receipt)?;
        let expected_hash = signed.body_hash(&net, Domain::Receipt)?;
        let stated: Sha256Digest = item["receipt_hash"].as_str().unwrap_or_default().parse()?;
        anyhow::ensure!(
            expected_hash == stated,
            "receipt {}: hash mismatch",
            item["receipt_id"]
        );
        ok += 1;
        println!(
            "ok {} attempt {} amount {} worker {}",
            signed.body.receipt_id,
            signed.body.attempt_id,
            signed.body.amount,
            signed.body.worker_public_key
        );
    }
    println!("{ok}/{} receipts verified", list.len());
    Ok(())
}

fn gen_matmul(m: u32, k: u32, n: u32, seed: u64, out_a: &Path, out_b: &Path) -> anyhow::Result<()> {
    let mut d = gg_protocol::drbg::Drbg::new("gg-cli/gen-matmul", seed);
    let a = TensorF32::new(vec![m as u64, k as u64], d.sym_f32_vec((m * k) as usize))?;
    let b = TensorF32::new(vec![k as u64, n as u64], d.sym_f32_vec((k * n) as usize))?;
    std::fs::write(out_a, a.encode())?;
    std::fs::write(out_b, b.encode())?;
    println!(
        "a: {} [{m}x{k}]\nb: {} [{k}x{n}]",
        out_a.display(),
        out_b.display()
    );
    Ok(())
}

async fn worker_session(server: &str, key: &Path) -> anyhow::Result<(Api, String)> {
    let api = api_for(server)?;
    let info = api.info().await?;
    let sk = SecretKey::load_from_file(key)?;
    let s = api.login(&sk, &info.network_id, "worker-login").await?;
    Ok((api, s.token))
}

#[tokio::main]
async fn main() {
    if let Err(e) = real_main().await {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

async fn real_main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Family::Auth { cmd } => match cmd {
            AuthCmd::Login { server, token } => save(&SavedConfig {
                server_url: Some(server),
                token: Some(token),
            }),
            AuthCmd::Whoami => {
                let ctx = Ctx::new(cli.server, cli.token)?;
                print(&ctx.get("/researcher/me").await?);
                Ok(())
            }
        },
        Family::Project { cmd } => {
            let ctx = Ctx::new(cli.server, cli.token)?;
            match cmd {
                ProjectCmd::List => print(&ctx.get("/researcher/projects").await?),
                ProjectCmd::Show { id } => {
                    print(&ctx.get(&format!("/researcher/projects/{id}")).await?)
                }
                ProjectCmd::Ledger { id } => print(
                    &ctx.get(&format!("/researcher/projects/{id}/ledger"))
                        .await?,
                ),
                ProjectCmd::Receipts { id, out } => {
                    let v = ctx
                        .get(&format!("/researcher/projects/{id}/receipts"))
                        .await?;
                    match out {
                        Some(p) => {
                            std::fs::write(&p, serde_json::to_string_pretty(&v)?)?;
                            println!(
                                "wrote {} receipts to {}",
                                v.as_array().map(|a| a.len()).unwrap_or(0),
                                p.display()
                            );
                        }
                        None => print(&v),
                    }
                }
            }
            Ok(())
        }
        Family::Job { cmd } => {
            match &cmd {
                JobCmd::EvalModel {
                    model,
                    checkpoint,
                    seed,
                    inputs,
                    output,
                    out,
                } => {
                    return eval_model(
                        model,
                        checkpoint.as_deref(),
                        *seed,
                        inputs,
                        output.as_deref(),
                        out,
                    );
                }
                JobCmd::CheckModel { model, batch, json } => {
                    return check_model(model, *batch, *json);
                }
                JobCmd::Plan(a) => return plan_model(a),
                JobCmd::Template(a) => return write_template(a),
                _ => {}
            }
            let ctx = Ctx::new(cli.server, cli.token)?;
            match cmd {
                JobCmd::CheckModel { .. }
                | JobCmd::EvalModel { .. }
                | JobCmd::Plan(_)
                | JobCmd::Template(_) => {
                    unreachable!("handled offline above")
                }
                JobCmd::Ops { json } => ops(&ctx, json).await?,
                JobCmd::Submit(a) => submit(&ctx, a).await?,
                JobCmd::Collective(a) => collective(&ctx, a).await?,
                JobCmd::CollectiveStatus { run } => {
                    print(&ctx.get(&format!("/researcher/collective/{run}")).await?)
                }
                JobCmd::CollectiveCheckpoint { run, out } => {
                    let view = ctx.get(&format!("/researcher/collective/{run}")).await?;
                    let project = view["project_id"]
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("run view lacks project_id"))?
                        .to_owned();
                    let parts = view["checkpoint"]["weights"]
                        .as_object()
                        .filter(|m| m.keys().all(|k| k.starts_with("checkpoint")))
                        .ok_or_else(|| {
                            anyhow::anyhow!("this run is not a graph run (no checkpoint bundle)")
                        })?;
                    let parameters = view["config"]["graph"]["parameters"].as_u64().unwrap_or(0);
                    // Shards downloaded one at a time, then reassembled into a single bundle.
                    let mut entries = BTreeMap::new();
                    for value in parts.values() {
                        let r: ArtifactRef = serde_json::from_value(value.clone())?;
                        let tmp = out.with_extension(format!("part-{}", r.hash));
                        ctx.api
                            .download(
                                &ctx.token,
                                &format!("/researcher/projects/{project}/artifacts/{}", r.hash),
                                &r,
                                gg_compute::graph::shards::MAX_SHARD_BYTES,
                                &tmp,
                            )
                            .await?;
                        let part = gg_protocol::tensor_file::BundleF32::read_from_path(
                            &tmp,
                            &gg_protocol::tensor_file::TensorLimits {
                                max_elements: parameters,
                            },
                        )?;
                        let _ = std::fs::remove_file(&tmp);
                        entries.extend(part.entries);
                    }
                    std::fs::write(
                        &out,
                        gg_protocol::tensor_file::BundleF32::new(entries)?.encode(),
                    )?;
                    let bundle = gg_protocol::tensor_file::BundleF32::read_from_path(
                        &out,
                        &gg_protocol::tensor_file::TensorLimits {
                            max_elements: parameters,
                        },
                    )?;
                    println!(
                        "{}: step {}, {} tensors, {} bytes",
                        out.display(),
                        view["step"],
                        bundle.entries.len(),
                        std::fs::metadata(&out)?.len()
                    );
                }
                JobCmd::CollectiveCancel { run, yes } => {
                    anyhow::ensure!(
                        yes,
                        "canceling stops the run and its open rounds; pass --yes to confirm"
                    );
                    ctx.post(&format!("/researcher/collective/{run}/cancel"), json!({}))
                        .await?;
                    println!("run {run} canceled");
                }
                JobCmd::CollectiveCheck {
                    run,
                    inputs,
                    abs,
                    rel,
                    out,
                } => collective_check(&ctx, run, &inputs, abs, rel, out.as_deref()).await?,
                JobCmd::Sweep(a) => sweep(&ctx, a).await?,
                JobCmd::Compare { job, out } => compare(&ctx, job, out.as_deref()).await?,
                JobCmd::Status { job } => {
                    print(&ctx.get(&format!("/researcher/jobs/{job}")).await?)
                }
                JobCmd::List { project } => {
                    let v = ctx.get(&format!("/researcher/projects/{project}")).await?;
                    print(&v);
                }
                JobCmd::Cancel { job, yes } => {
                    anyhow::ensure!(
                        yes,
                        "canceling releases the job's reservations and stops assignment; pass --yes to confirm"
                    );
                    print(
                        &ctx.post(&format!("/researcher/jobs/{job}/cancel"), json!({}))
                            .await?,
                    );
                }
                JobCmd::Task { task } => {
                    print(&ctx.get(&format!("/researcher/tasks/{task}")).await?)
                }
                JobCmd::Results { task, out } => results(&ctx, task, &out).await?,
            }
            Ok(())
        }
        Family::Release { cmd } => match cmd {
            ReleaseCmd::Keygen { out } => release_keygen(&out),
            ReleaseCmd::Sign { manifest, key, out } => sign_release(&manifest, &key, &out),
            ReleaseCmd::Add { manifest, files } => release_add(&manifest, &files),
            ReleaseCmd::SshKey { key } => {
                let key: PublicKey = key
                    .parse()
                    .map_err(|e| anyhow::anyhow!("public key: {e}"))?;
                println!("{}", gg_protocol::release::allowed_signers_line(&key));
                Ok(())
            }
            ReleaseCmd::Verify {
                manifest,
                keys,
                revoked,
                network,
                dir,
            } => verify_release(&manifest, &keys, &revoked, &network, dir.as_deref()),
        },
        Family::Worker { cmd } => match cmd {
            WorkerCmd::Keygen { out } => {
                let k = SecretKey::generate()?;
                k.save_to_file(&out)?;
                println!("{}", k.public_key());
                Ok(())
            }
            WorkerCmd::Register { key, name } => {
                let server = cli
                    .server
                    .or(load_saved().server_url)
                    .ok_or_else(|| anyhow::anyhow!("no server configured"))?;
                let api = api_for(&server)?;
                let sk = SecretKey::load_from_file(&key)?;
                let r = api.register_worker(&sk.public_key(), &name).await?;
                println!(
                    "worker_id: {}\nauthorized: {}\npublic_key: {}",
                    r.worker_id,
                    r.authorized,
                    sk.public_key()
                );
                Ok(())
            }
            WorkerCmd::Payout { key, address } => {
                let server = cli
                    .server
                    .clone()
                    .or(load_saved().server_url)
                    .ok_or_else(|| anyhow::anyhow!("no server configured"))?;
                let (api, token) = worker_session(&server, &key).await?;
                api.post_json::<Value>(&token, "/worker/payout", &json!({ "address": address }))
                    .await?;
                println!("payout address declared: {address}");
                Ok(())
            }
            WorkerCmd::Receipts { key, out } => {
                let server = cli
                    .server
                    .or(load_saved().server_url)
                    .ok_or_else(|| anyhow::anyhow!("no server configured"))?;
                let (api, token) = worker_session(&server, &key).await?;
                let v: Value = api.get_json(&token, "/worker/receipts").await?;
                match out {
                    Some(p) => {
                        std::fs::write(&p, serde_json::to_string_pretty(&v)?)?;
                        println!(
                            "wrote {} receipts to {}",
                            v.as_array().map(|a| a.len()).unwrap_or(0),
                            p.display()
                        );
                    }
                    None => print(&v),
                }
                Ok(())
            }
        },
        Family::Receipt { cmd } => match cmd {
            ReceiptCmd::Verify {
                file,
                server_key,
                network,
            } => verify_receipts(&file, &server_key, &network),
        },
        Family::Ledger { cmd } => {
            let ctx = Ctx::new(cli.server, cli.token)?;
            match cmd {
                LedgerCmd::Balance { project } => {
                    print(&ctx.get(&format!("/researcher/projects/{project}")).await?["budget"])
                }
            }
            Ok(())
        }
        Family::Admin { cmd } => {
            let ctx = Ctx::new(cli.server, cli.token)?;
            match cmd {
                AdminCmd::ResearcherCreate { name, approve } => {
                    let admission = if approve { "approved" } else { "pending" };
                    print(
                        &ctx.post(
                            "/admin/researchers",
                            json!({ "display_name": name, "admission": admission }),
                        )
                        .await?,
                    );
                }
                AdminCmd::ResearcherAdmission { id, state, note } => {
                    ctx.post(
                        &format!("/admin/researchers/{id}/admission"),
                        json!({ "state": state, "note": note }),
                    )
                    .await?;
                    println!("ok");
                }
                AdminCmd::ProjectCreate {
                    name,
                    kinds,
                    owner,
                    ack_data_policy,
                } => {
                    anyhow::ensure!(
                        ack_data_policy,
                        "this deployment publishes a data rule (GET /v1/info, champ data_policy_text); \
                         pass --ack-data-policy to state that the submitter accepts it"
                    );
                    print(&ctx.post("/admin/projects", json!({ "name": name, "allowed_kinds": kinds, "owner_researcher_id": owner, "data_policy_ack": true })).await?);
                }
                AdminCmd::ProjectGrant {
                    id,
                    amount,
                    reference,
                } => {
                    print(
                        &ctx.post(
                            &format!("/admin/projects/{id}/grant"),
                            json!({ "amount": amount, "reference": reference }),
                        )
                        .await?,
                    );
                }
                AdminCmd::ProjectMember {
                    id,
                    researcher,
                    role,
                } => {
                    ctx.post(
                        &format!("/admin/projects/{id}/members"),
                        json!({ "researcher_id": researcher, "role": role }),
                    )
                    .await?;
                    println!("ok");
                }
                AdminCmd::ProjectList => print(&ctx.get("/admin/projects").await?),
                AdminCmd::WorkerList => print(&ctx.get("/admin/workers").await?),
                AdminCmd::ResearcherList => print(&ctx.get("/admin/researchers").await?),
                AdminCmd::ContributorList => print(&ctx.get("/admin/contributors").await?),
                AdminCmd::ContributorSuspend {
                    id,
                    reason,
                    reinstate,
                    yes,
                } => {
                    if !reinstate {
                        anyhow::ensure!(
                            yes && !reason.trim().is_empty(),
                            "suspending stops every machine of the account; pass --reason \"...\" and --yes"
                        );
                    }
                    print(
                        &ctx.post(
                            &format!("/admin/contributors/{id}/suspend"),
                            json!({ "suspended": !reinstate, "reason": reason }),
                        )
                        .await?,
                    );
                }
                AdminCmd::WorkerAuthorize { id, revoke, yes } => {
                    if revoke {
                        anyhow::ensure!(
                            yes,
                            "revoking ends the worker's sessions; pass --yes to confirm"
                        );
                    }
                    ctx.post(
                        &format!("/admin/workers/{id}/authorize"),
                        json!({ "authorized": !revoke }),
                    )
                    .await?;
                    println!("ok");
                }
                AdminCmd::ValidatorAuthorize { id, revoke, yes } => {
                    if revoke {
                        anyhow::ensure!(
                            yes,
                            "withdrawing a validator ends its sessions and hands its verifications to another; pass --yes to confirm"
                        );
                    }
                    print(
                        &ctx.post(
                            &format!("/admin/validators/{id}/authorize"),
                            json!({ "authorized": !revoke }),
                        )
                        .await?,
                    );
                }
                AdminCmd::ArtifactWithdraw { hash, reason, yes } => {
                    anyhow::ensure!(
                        yes,
                        "withdrawing deletes the artifact and cancels the jobs and runs using it; pass --yes to confirm"
                    );
                    print(
                        &ctx.post(
                            &format!("/admin/artifacts/{hash}/withdraw"),
                            json!({ "reason": reason }),
                        )
                        .await?,
                    );
                }
                AdminCmd::ValidatorAdd {
                    key,
                    name,
                    operator,
                } => {
                    print(
                        &ctx.post(
                            "/admin/validators",
                            json!({ "public_key": key, "display_name": name, "operator": operator }),
                        )
                        .await?,
                    );
                }
                AdminCmd::OperatorCreate { name, contact } => {
                    print(
                        &ctx.post(
                            "/admin/operators",
                            json!({ "name": name, "contact": contact }),
                        )
                        .await?,
                    );
                }
                AdminCmd::OperatorList => print(&ctx.get("/admin/operators").await?),
                AdminCmd::DisputeList { all } => {
                    print(&ctx.get(&format!("/admin/disputes?open={}", !all)).await?)
                }
                AdminCmd::DisputeResolve {
                    dispute,
                    outcome,
                    reason,
                    yes,
                } => {
                    anyhow::ensure!(
                        matches!(outcome.as_str(), "accepted" | "rejected" | "inconclusive"),
                        "outcome must be accepted, rejected or inconclusive"
                    );
                    anyhow::ensure!(
                        yes,
                        "settling a dispute pays or refuses a contribution; pass --yes to confirm"
                    );
                    print(
                        &ctx.post(
                            &format!("/admin/disputes/{dispute}/resolve"),
                            json!({ "outcome": outcome, "reason": reason }),
                        )
                        .await?,
                    );
                }
                AdminCmd::BatchCreate {
                    batch_id,
                    chain_id,
                    contract,
                    asset,
                    max_leaves,
                    dry_run,
                } => {
                    print(
                        &ctx.post(
                            "/admin/reward-batches",
                            json!({ "batch_id": batch_id, "chain_id": chain_id, "contract": contract, "asset": asset, "max_leaves": max_leaves, "dry_run": dry_run }),
                        )
                        .await?,
                    );
                }
                AdminCmd::AuditRate { set, reason } => match set {
                    Some(rate) => {
                        anyhow::ensure!(
                            (0.0..=1.0).contains(&rate),
                            "the audit rate is a share between 0 and 1"
                        );
                        print(
                            &ctx.post(
                                "/admin/config/audit-rate",
                                json!({ "rate": rate, "reason": reason }),
                            )
                            .await?,
                        );
                    }
                    None => print(&ctx.get("/admin/config/audit-rate").await?),
                },
                AdminCmd::BatchList => print(&ctx.get("/admin/reward-batches").await?),
                AdminCmd::BatchExport { batch_id, out } => {
                    let v = ctx
                        .get(&format!("/admin/reward-batches/{batch_id}"))
                        .await?;
                    match out {
                        Some(path) => {
                            std::fs::write(&path, serde_json::to_string_pretty(&v)? + "\n")?;
                            println!(
                                "batch {batch_id}: {} leaves written to {}",
                                v["leaf_count"],
                                path.display()
                            );
                        }
                        None => print(&v),
                    }
                }
                AdminCmd::BatchSettle {
                    batch_id,
                    event,
                    tx_hash,
                    height,
                    observed_root,
                    note,
                } => {
                    anyhow::ensure!(
                        matches!(
                            event.as_str(),
                            "submitted" | "observed" | "finalized" | "reconcile"
                        ),
                        "event must be submitted, observed, finalized or reconcile"
                    );
                    print(
                        &ctx.post(
                            &format!("/admin/reward-batches/{batch_id}/settlement"),
                            json!({ "event": event, "tx_hash": tx_hash, "height": height, "observed_root": observed_root, "note": note }),
                        )
                        .await?,
                    );
                }
                AdminCmd::TasksSummary => print(&ctx.get("/admin/tasks/summary").await?),
                AdminCmd::Audit => print(&ctx.get("/admin/audit").await?),
            }
            Ok(())
        }
        Family::Data { cmd } => match cmd {
            DataCmd::GenMatmul {
                m,
                k,
                n,
                seed,
                out_a,
                out_b,
            } => gen_matmul(m, k, n, seed, &out_a, &out_b),
            DataCmd::Inspect {
                path,
                json: as_json,
            } => {
                use gg_protocol::tensor_file::{BundleF32, TensorF32};
                let bytes = std::fs::read(&path)?;
                let entries: Vec<(String, Vec<u64>)> = if bytes.starts_with(b"GGB1") {
                    let b = BundleF32::decode(&bytes, &Default::default())?;
                    b.entries
                        .iter()
                        .map(|(k, t)| (k.clone(), t.shape.clone()))
                        .collect()
                } else {
                    let t = TensorF32::decode(&bytes, &Default::default())?;
                    vec![(String::new(), t.shape)]
                };
                if as_json {
                    print(&json!(
                        entries
                            .iter()
                            .map(|(k, s)| json!({"name": k, "shape": s}))
                            .collect::<Vec<_>>()
                    ));
                } else {
                    for (k, shape) in &entries {
                        println!(
                            "{:<24} {:?}",
                            if k.is_empty() { "(tensor)" } else { k },
                            shape
                        );
                    }
                    println!("entries: {}, bytes: {}", entries.len(), bytes.len());
                }
                Ok(())
            }
            DataCmd::GenMlp {
                d_input,
                d_output,
                d_teacher,
                n_train,
                n_eval,
                seed,
                noise,
                out_dir,
            } => {
                let cfg = gg_compute::synthetic::SyntheticMlpConfig {
                    d_input,
                    d_output,
                    d_teacher,
                    n_train,
                    n_eval,
                    seed,
                    noise,
                };
                let ds = cfg.generate().map_err(|e| anyhow::anyhow!(e))?;
                std::fs::create_dir_all(&out_dir)?;
                for (name, t) in [
                    ("train_x", &ds.train_x),
                    ("train_y", &ds.train_y),
                    ("eval_x", &ds.eval_x),
                    ("eval_y", &ds.eval_y),
                ] {
                    let p = out_dir.join(format!("{name}.ggt"));
                    std::fs::write(&p, t.encode())?;
                    println!("{name}: {} {:?}", p.display(), t.shape);
                }
                Ok(())
            }
        },
    }
}
