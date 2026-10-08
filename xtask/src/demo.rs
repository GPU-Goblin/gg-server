//! Reference demonstration: real server, PostgreSQL, N CPU workers (separate processes with
//! separate executors), one validator; either a grid of 12 independent MLP trainings (default) or a
//! batch of matmul jobs; receipts exported and verified offline; ledger report. `--faults`
//! interrupts a worker holding a lease, replays accepted submissions and submits corrupted results
//! from a dishonest worker. All waits are bounded.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use clap::Args;
use serde_json::{Value, json};

#[derive(Args)]
pub struct DemoArgs {
    /// Inject faults: interrupted worker, replayed submission, corrupted results.
    #[arg(long)]
    pub faults: bool,
    /// Use the WGPU/Vulkan engine for workers (fails clearly if unavailable).
    #[arg(long)]
    pub gpu: bool,
    /// Compute kind: `mlp` (grid of 12 trainings, default), `matmul` or `collective`
    /// (one collective training of `--rounds` rounds × 4 `mlp.grad.v1` tasks, ADR-0011).
    #[arg(long, default_value = "mlp")]
    pub kind: String,
    /// Rounds of the collective or graph run.
    #[arg(long, default_value_t = 10)]
    pub rounds: u32,
    /// Federated validation (P6, ADR-0014): two validators under distinct operators, every
    /// submission audited by the other operator. With `--faults`, a third validator vouches for
    /// everything without recomputing, so a disagreement opens a dispute an operator must settle.
    #[arg(long)]
    pub federated: bool,
    /// Distribute artifacts between workers (P5, ADR-0012): chunks verified against the
    /// server-signed manifest, control plane as fallback. With `--faults`, one worker serves
    /// corrupted chunks and must be caught.
    #[arg(long)]
    pub p2p: bool,
    /// Lab node (ADR-0041; `--kind matmul` or `graph`, implies `--p2p`): a node on the
    /// researcher's side holds the inputs, pulls the outputs from the workers, and validators read
    /// both through peers; for `graph`, it also draws the weights and applies every optimizer
    /// step. The control plane stores none of them.
    #[arg(long)]
    pub node: bool,
    /// Verification by other contributors (ADR-0042): every task computed again by another
    /// account (each demo worker is its own), compared; with the default probation, every task
    /// gets a third computation too. Validators then have nothing to recompute.
    #[arg(long)]
    pub replicas: bool,
    /// Number of honest CPU workers.
    #[arg(long, default_value_t = 3)]
    pub workers: u32,
    /// Number of matmul jobs (matmul kind only; the mlp grid always has 12 tasks).
    #[arg(long, default_value_t = 12)]
    pub jobs: u32,
    /// Matrix dimension for the matmul kind (square matrices).
    #[arg(long, default_value_t = 256)]
    pub dim: u32,
    /// Output directory for the report.
    #[arg(long, default_value = "var/demo")]
    pub out: PathBuf,
    /// Overall timeout in seconds (starts after the build).
    #[arg(long, default_value_t = 300)]
    pub timeout_secs: u64,
    /// Wait for this many workers started elsewhere (a virtual machine reaching this one through
    /// a relay to the loopback port) to register themselves, and authorize them; they take tasks
    /// like the local ones. The demo prints the port and network they need.
    #[arg(long, default_value_t = 0)]
    pub external_workers: u32,
}

struct Procs {
    children: Vec<(String, Child)>,
}

impl Procs {
    fn spawn(&mut self, name: &str, mut cmd: Command, log: &Path) -> anyhow::Result<()> {
        let out = std::fs::File::create(log)?;
        let err = out.try_clone()?;
        cmd.stdout(Stdio::from(out))
            .stderr(Stdio::from(err))
            .stdin(Stdio::null());
        let child = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("spawn {name}: {e}"))?;
        println!("  started {name} (pid {})", child.id());
        self.children.push((name.to_owned(), child));
        Ok(())
    }
    fn kill(&mut self, name: &str) {
        if let Some(pos) = self.children.iter().position(|(n, _)| n == name) {
            let (_, mut c) = self.children.remove(pos);
            let _ = c.kill();
            let _ = c.wait();
            println!("  killed {name}");
        }
    }
}

impl Drop for Procs {
    fn drop(&mut self) {
        for (name, c) in self.children.iter_mut().rev() {
            let _ = c.kill();
            let _ = c.wait();
            println!("  stopped {name}");
        }
    }
}

fn bin(name: &str) -> PathBuf {
    let target = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("target"));
    target.join("debug").join(name)
}

fn gg(server: &str, token: &str, args: &[&str]) -> anyhow::Result<Value> {
    let out = Command::new(bin("gg"))
        .args(args)
        .env("GG_SERVER_URL", server)
        .env("GG_TOKEN", token)
        .output()?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    anyhow::ensure!(
        out.status.success(),
        "gg {} failed: {}{}",
        args.join(" "),
        stdout,
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(serde_json::from_str(stdout.trim()).unwrap_or(Value::String(stdout.trim().to_owned())))
}

fn gg_text(server: &str, token: &str, args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new(bin("gg"))
        .args(args)
        .env("GG_SERVER_URL", server)
        .env("GG_TOKEN", token)
        .output()?;
    anyhow::ensure!(
        out.status.success(),
        "gg {} failed: {}{}",
        args.join(" "),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn free_port() -> anyhow::Result<u16> {
    let l = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(l.local_addr()?.port())
}

fn wait_health(server: &str, timeout: Duration) -> anyhow::Result<()> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()?;
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Ok(r) = client.get(format!("{server}/v1/health")).send()
            && r.status().is_success()
        {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    anyhow::bail!("server did not become healthy within {timeout:?}")
}

#[allow(clippy::too_many_arguments)]
fn write_worker_config(
    path: &Path,
    dir: &Path,
    server: &str,
    name: &str,
    engine: &str,
    kind: &str,
    faults: Option<&str>,
    p2p: bool,
) -> anyhow::Result<()> {
    let mut text = format!(
        "server_url = \"{server}\"\nkey_path = \"{}\"\nwork_dir = \"{}\"\ndisplay_name = \"{name}\"\nengine = \"{engine}\"\nkinds = [\"{kind}\"]\nclaim_wait_secs = 5\nheartbeat_secs = 5\nsandbox = \"bwrap\"\nkeep_failed_bundles = true\n",
        dir.join("worker.key").display(),
        dir.join("work").display()
    );
    if p2p {
        // Single host: peers are loopback, so the local policy must allow private addresses.
        text.push_str(
            "\n[p2p]\nenabled = true\nshare = true\nlisten_addr = \"127.0.0.1:0\"\nallow_private_peers = true\nmax_peers = 8\nmax_upload_conns = 8\nnat_pmp = false\nupnp = false\n# The demo artifacts are smaller than 1 MiB: without this lowered threshold, a worker would\n# fetch them straight from the control plane, and peer-to-peer would not be exercised.\nmin_bytes = 0\n",
        );
    }
    if let Some(f) = faults {
        text.push_str(&format!("\n[dev_faults]\n{f}\n"));
    }
    std::fs::write(path, text)?;
    Ok(())
}

/// The researcher's lab node, run in this process (ADR-0041).
struct Lab {
    rt: tokio::runtime::Runtime,
    node: std::sync::Arc<gg_node::Node>,
    stop: tokio::sync::watch::Sender<bool>,
}

fn start_lab(
    server: &str,
    researcher_token: &str,
    project: &str,
    dir: &Path,
    log: &Path,
) -> anyhow::Result<Lab> {
    // The node runs in this process: its log goes to its own file, like the other processes'.
    let file = std::fs::File::create(log)?;
    let _ = tracing_subscriber::fmt()
        .with_env_filter("gg_node=debug,gg_client=info")
        .with_writer(std::sync::Mutex::new(file))
        .with_ansi(false)
        .try_init();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let key = gg_protocol::keys::SecretKey::generate()?;
    let r = reqwest::blocking::Client::new()
        .post(format!("{server}/v1/researcher/nodes"))
        .bearer_auth(researcher_token)
        .json(&json!({ "public_key": key.public_key().to_hex(), "name": "demo-lab" }))
        .send()?;
    anyhow::ensure!(
        r.status().is_success(),
        "register the lab node: {}",
        r.text()?
    );
    // The node works for the projects attached to it (ADR-0041, A7).
    let node: Value = r.json()?;
    let a = reqwest::blocking::Client::new()
        .post(format!("{server}/v1/researcher/projects/{project}/nodes"))
        .bearer_auth(researcher_token)
        .json(&json!({ "node_id": node["node_id"] }))
        .send()?;
    anyhow::ensure!(
        a.status().is_success(),
        "attach the lab node: {}",
        a.text()?
    );
    let node = rt.block_on(gg_node::Node::start(
        gg_node::NodeConfig {
            server_url: server.to_owned(),
            allow_plain_http: true,
            state_dir: dir.to_path_buf(),
            listen_addr: "127.0.0.1:0".into(),
            reach: Default::default(),
            max_conns: 16,
            allow_private_peers: true,
            // A producer that serves wrong bytes (`--faults`) has its task sent again soon.
            give_up_after: Duration::from_secs(10),
            expect: None,
        },
        key,
    ))?;
    let (stop, rx) = tokio::sync::watch::channel(false);
    rt.spawn(
        node.clone()
            .keep_pulling(Duration::from_millis(20), rx.clone()),
    );
    rt.spawn(
        node.clone()
            .keep_stepping(Duration::from_secs(1), rx.clone()),
    );
    rt.spawn(
        node.clone()
            .keep_relaying(Duration::from_secs(1), rx.clone()),
    );
    // Comparisons of computations, when the server replicates them (ADR-0042).
    rt.spawn(
        node.clone()
            .keep_comparing(Duration::from_secs(1), rx.clone()),
    );
    rt.spawn(node.clone().keep_announcing(Duration::from_secs(60), rx));
    Ok(Lab { rt, node, stop })
}

pub fn run(mut args: DemoArgs) -> anyhow::Result<()> {
    anyhow::ensure!(
        !args.node || args.kind == "matmul" || args.kind == "graph",
        "--node runs with --kind matmul or --kind graph"
    );
    // The node's artifacts travel between peers only.
    args.p2p |= args.node;
    anyhow::ensure!(
        args.kind == "mlp"
            || args.kind == "matmul"
            || args.kind == "collective"
            || args.kind == "graph",
        "--kind must be mlp, matmul, collective or graph"
    );
    anyhow::ensure!(
        !(args.federated && (args.kind == "collective" || args.kind == "graph")),
        "--federated runs on independent tasks (--kind matmul or mlp): a disputed round would hold the whole collective run until an operator settles it"
    );
    anyhow::ensure!(
        !(args.gpu && args.kind == "mlp"),
        "GPU-trained mlp.train.v1 results are not verifiable by the CPU replay validator (profile not admitted, see docs/VALIDATION.md); use --kind matmul with --gpu"
    );
    println!(
        "== GPU Goblin demo (kind: {}, faults: {}, gpu: {}) ==",
        args.kind, args.faults, args.gpu
    );
    println!("-- building binaries");
    super::run("cargo", &["build", "--workspace", "--bins"])?;
    if args.gpu {
        super::worker::build(&["--features", "gg-worker/gpu"])?;
    } else {
        super::worker::build(&[])?;
    }
    let worker_bin = super::worker::target("debug")?.join("gg-worker");
    println!("-- database");
    super::db::up()?;
    // The bounded wait starts after the build (a GPU build alone can take minutes).
    let started = Instant::now();
    let deadline = started + Duration::from_secs(args.timeout_secs);
    let check_deadline = |what: &str| -> anyhow::Result<()> {
        anyhow::ensure!(Instant::now() < deadline, "demo timeout while {what}");
        Ok(())
    };

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let root = args.out.join(format!("run-{stamp}"));
    std::fs::create_dir_all(&root)?;
    let logs = root.join("logs");
    std::fs::create_dir_all(&logs)?;
    let port = free_port()?;
    let server = format!("http://127.0.0.1:{port}");
    let network = "gg-demo";
    let db_name = format!("gg_demo_{stamp}");
    // The development database from `DATABASE_URL` (see `.env.example`): same user and host, a
    // fresh database for this run.
    let (db_user, db_password, db_host, db_base) = super::db::url_parts();
    let create = Command::new("docker")
        .args([
            "exec",
            &super::db::container(),
            "psql",
            "-U",
            &db_user,
            "-d",
            &db_base,
            "-c",
            &format!("CREATE DATABASE {db_name}"),
        ])
        .output()?;
    anyhow::ensure!(
        create.status.success(),
        "create demo database: {}",
        String::from_utf8_lossy(&create.stderr)
    );
    let db_url = format!("postgres://{db_user}:{db_password}@{db_host}/{db_name}");
    let server_dir = root.join("server");
    std::fs::create_dir_all(&server_dir)?;

    let mut procs = Procs {
        children: Vec::new(),
    };
    println!("-- control plane on {server} (network {network}, db {db_name})");
    let lease_secs = if args.faults { "8" } else { "120" };
    let mut cmd = Command::new(bin("gg-server"));
    cmd.arg("run")
        .env("GG_DATABASE_URL", &db_url)
        .env("GG_DATA_DIR", &server_dir)
        .env("GG_BIND_ADDR", format!("127.0.0.1:{port}"))
        .env("GG_NETWORK_ID", network)
        .env("GG_LEASE_SECS", lease_secs)
        .env("GG_REAPER_INTERVAL_SECS", "1")
        .env(
            "GG_P2P_ALLOW_PRIVATE_ADDRS",
            if args.p2p { "1" } else { "0" },
        )
        // Small chunks so the demo exercises multi-chunk transfers on modest artifacts.
        .env("GG_P2P_CHUNK_BYTES", "65536")
        // With a lab node, every output goes to it: the control plane relays none.
        .env(
            "GG_NODE_RELAY_MAX_BYTES",
            if args.node { "0" } else { "67108864" },
        )
        // Federated validation: audit every submission so the demonstration is deterministic.
        .env("GG_AUDIT_RATE", if args.federated { "1.0" } else { "0" })
        .env(
            "GG_REPLICA_VERIFICATION",
            if args.replicas { "1" } else { "0" },
        )
        .env("RUST_LOG", "info,sqlx=warn");
    procs.spawn("gg-server", cmd, &logs.join("server.log"))?;
    wait_health(&server, Duration::from_secs(30))?;
    let server_key = {
        let out = Command::new(bin("gg-server"))
            .arg("identity")
            .env("GG_DATA_DIR", &server_dir)
            .env("GG_DATABASE_URL", &db_url)
            .output()?;
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    };
    let admin = {
        let out = Command::new(bin("gg-server"))
            .args(["bootstrap-admin", "--label", "demo"])
            .env("GG_DATA_DIR", &server_dir)
            .env("GG_DATABASE_URL", &db_url)
            .output()?;
        anyhow::ensure!(
            out.status.success(),
            "bootstrap-admin: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    };

    println!("-- researcher, project, budget");
    let r = gg(
        &server,
        &admin,
        &[
            "admin",
            "researcher-create",
            "--name",
            "Demo Researcher",
            "--approve",
        ],
    )?;
    let researcher_token = r["token"].as_str().unwrap().to_owned();
    let researcher_id = r["researcher_id"].as_str().unwrap().to_owned();
    let p = gg(
        &server,
        &admin,
        &[
            "admin",
            "project-create",
            "--name",
            "demo",
            "--kind",
            "tensor.matmul.v1",
            "--kind",
            "mlp.train.v1",
            "--kind",
            "mlp.grad.v1",
            "--kind",
            "graph.grad.v1",
            "--owner",
            &researcher_id,
            // The demo uses only synthetic data: the policy is accepted explicitly, as for any
            // project (docs/DECISIONS.md, D4).
            "--ack-data-policy",
        ],
    )?;
    let project = p["project_id"].as_str().unwrap().to_owned();
    let grant = if args.kind == "collective" || args.kind == "graph" {
        // 4 tasks per round, 2 TC per accepted task, room for reissued rounds.
        (args.rounds as u64) * 4 * 2 * 3 + 200
    } else {
        (args.jobs as u64) * 10 + 200
    };
    let b = gg(
        &server,
        &admin,
        &[
            "admin",
            "project-grant",
            &project,
            "--amount",
            &grant.to_string(),
            "--reference",
            "demo",
        ],
    )?;
    println!("  project {project} budget {}", b["available"]);

    println!("-- validator");
    let start_validator = |procs: &mut Procs,
                           name: &str,
                           operator: &str,
                           always_accept: bool|
     -> anyhow::Result<()> {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir)?;
        let vkey = dir.join("verifier.key");
        let out = Command::new(bin("gg-verifier"))
            .arg("init")
            .env("GG_VERIFIER_KEY", &vkey)
            .env("GG_SERVER_URL", &server)
            .output()?;
        anyhow::ensure!(
            out.status.success(),
            "verifier init: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let vpub = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        gg(
            &server,
            &admin,
            &[
                "admin",
                "validator-add",
                "--key",
                &vpub,
                "--name",
                name,
                "--operator",
                operator,
            ],
        )?;
        let cfg = dir.join("verifier.toml");
        let mut text = format!(
            "server_url = \"{server}\"\nkey_path = \"{}\"\nwork_dir = \"{}\"\nclaim_wait_secs = 5\n",
            vkey.display(),
            dir.join("work").display()
        );
        if always_accept {
            text.push_str("\n[dev_faults]\nalways_accept = true\n");
        }
        std::fs::write(&cfg, text)?;
        let mut cmd = Command::new(bin("gg-verifier"));
        cmd.args(["--config", cfg.to_str().unwrap(), "run"])
            .env("RUST_LOG", "info")
            // What a lab node holds is read through peers, here on this machine.
            .env(
                "GG_VERIFIER_ALLOW_PRIVATE_PEERS",
                if args.p2p { "1" } else { "0" },
            );
        procs.spawn(name, cmd, &logs.join(format!("{name}.log")))?;
        println!(
            "  {name}: operator {operator}{}",
            if always_accept {
                " (DEV FAULT: vouches without recomputing)"
            } else {
                ""
            }
        );
        Ok(())
    };
    if args.federated {
        start_validator(&mut procs, "gg-verifier", "op-a", false)?;
        start_validator(&mut procs, "verifier-b", "op-b", false)?;
        if args.faults {
            start_validator(&mut procs, "verifier-vouching", "op-c", true)?;
        }
    } else {
        start_validator(&mut procs, "gg-verifier", "unassigned", false)?;
    }

    let data = root.join("data");
    std::fs::create_dir_all(&data)?;
    let lab = if args.node {
        println!("-- lab node (holds the inputs, pulls the outputs)");
        let lab = start_lab(
            &server,
            &researcher_token,
            &project,
            &root.join("node"),
            &logs.join("node.log"),
        )?;
        println!("  node {}", lab.node.id());
        Some(lab)
    } else {
        None
    };
    let mut node_inputs: Vec<gg_core::Sha256Digest> = Vec::new();
    let mut jobs: Vec<String> = Vec::new();
    let mut expected_tasks = 0usize;
    let kind_id: &str;
    let mut run_id: Option<String> = None;
    let collective_inputs: Vec<String> = ["train_x", "train_y"]
        .iter()
        .map(|n| format!("{n}={}", data.join(format!("{n}.ggt")).display()))
        .collect();
    if args.kind == "graph" {
        // The researcher describes the model as a graph (ADR-0016); the coordinator distributes
        // micro-batch gradients exactly as for a catalog MLP (ADR-0011/0017).
        kind_id = "graph.grad.v1";
        println!(
            "-- synthetic dataset and one training of a described graph ({} rounds × 4 micro-batch tasks, adam)",
            args.rounds
        );
        gg_text(
            &server,
            &researcher_token,
            &[
                "data",
                "gen-mlp",
                "--d-input",
                "16",
                "--d-output",
                "4",
                "--d-teacher",
                "32",
                "--n-train",
                "4096",
                "--n-eval",
                "64",
                "--seed",
                "7",
                "--noise",
                "0.05",
                "--out-dir",
                data.to_str().unwrap(),
            ],
        )?;
        // A perceptron with two hidden layers and GELU: not in the original catalog, described here.
        let graph_path = data.join("model.json");
        std::fs::write(
            &graph_path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "format": "gg-graph/v1",
                "name": "demo-graphe",
                "inputs": [{"name": "x", "shape": [0, 16]}],
                "parameters": [
                    {"name": "w1", "shape": [16, 48], "init": "xavier_uniform"},
                    {"name": "b1", "shape": [48], "init": "zeros"},
                    {"name": "w2", "shape": [48, 24], "init": "xavier_uniform"},
                    {"name": "b2", "shape": [24], "init": "zeros"},
                    {"name": "w3", "shape": [24, 4], "init": "xavier_uniform"},
                    {"name": "b3", "shape": [4], "init": "zeros"}
                ],
                "nodes": [
                    {"op": "MatMul", "inputs": ["x", "w1"], "output": "h1"},
                    {"op": "Add", "inputs": ["h1", "b1"], "output": "h1b"},
                    {"op": "Gelu", "inputs": ["h1b"], "output": "a1"},
                    {"op": "MatMul", "inputs": ["a1", "w2"], "output": "h2"},
                    {"op": "Add", "inputs": ["h2", "b2"], "output": "h2b"},
                    {"op": "Relu", "inputs": ["h2b"], "output": "a2"},
                    {"op": "MatMul", "inputs": ["a2", "w3"], "output": "h3"},
                    {"op": "Add", "inputs": ["h3", "b3"], "output": "pred"}
                ],
                "loss": {"mean_squared_error": {"prediction": "pred",
                    "target": {"name": "y", "shape": [0, 4]}}}
            }))?,
        )?;
        // A refusal comes before anything is sent: that is the promise of ADR-0016, checked here.
        let check = gg_text(
            &server,
            &researcher_token,
            &["job", "check-model", graph_path.to_str().unwrap()],
        )?;
        print!("{check}");
        anyhow::ensure!(
            check.contains("accepted by profile"),
            "check-model refused the demo graph"
        );
        let rounds = args.rounds.to_string();
        let x = format!("x={}", data.join("train_x.ggt").display());
        let y = format!("y={}", data.join("train_y.ggt").display());
        let r = if let Some(lab) = &lab {
            // The description is small and stays on the server; the datasets stay on the node,
            // which draws the weights and steps the run.
            let graph_bytes = std::fs::read(&graph_path)?;
            let graph_hash = gg_core::Sha256Digest::of_bytes(&graph_bytes);
            let http = reqwest::blocking::Client::new();
            let up = http
                .put(format!(
                    "{server}/v1/researcher/projects/{project}/artifacts/{graph_hash}?size={}&schema=gg-graph-v1",
                    graph_bytes.len()
                ))
                .bearer_auth(&researcher_token)
                .body(graph_bytes.clone())
                .send()?;
            anyhow::ensure!(
                up.status().is_success(),
                "upload the description: {}",
                up.text()?
            );
            let project_id = gg_core::ProjectId(project.parse()?);
            let hold = |f: &str| {
                lab.rt
                    .block_on(lab.node.hold(project_id, &data.join(f), "ggt1-f32"))
            };
            let (rx, ry) = (hold("train_x.ggt")?, hold("train_y.ggt")?);
            node_inputs.extend([rx.hash, ry.hash]);
            let r = http
                .post(format!(
                    "{server}/v1/researcher/projects/{project}/collective"
                ))
                .bearer_auth(&researcher_token)
                .json(&json!({
                    "name": "demo-graphe",
                    "graph": { "hash": graph_hash, "size": graph_bytes.len() },
                    "datasets": { "x": rx, "y": ry },
                    "optimizer": "adam", "learning_rate": 0.02, "batch_per_round": 256,
                    "tasks_per_round": 4, "rounds": args.rounds, "seed": 1,
                    "max_round_retries": 2, "holder_node": lab.node.id(),
                }))
                .send()?;
            anyhow::ensure!(r.status().is_success(), "create the run: {}", r.text()?);
            r.json::<Value>()?
        } else {
            gg(
                &server,
                &researcher_token,
                &[
                    "job",
                    "collective",
                    "--project",
                    &project,
                    "--name",
                    "demo-graphe",
                    "--graph",
                    graph_path.to_str().unwrap(),
                    "--dataset",
                    &x,
                    "--dataset",
                    &y,
                    "--optimizer",
                    "adam",
                    "--lr",
                    "0.02",
                    "--batch",
                    "256",
                    "--tasks",
                    "4",
                    "--rounds",
                    &rounds,
                    "--seed",
                    "1",
                    "--retries",
                    "2",
                ],
            )?
        };
        let id = r["run_id"].as_str().unwrap().to_owned();
        println!(
            "  run {id}: {} parameters, state {} step {}/{}",
            r["config"]["graph"]["parameters"], r["state"], r["step"], r["rounds_total"]
        );
        run_id = Some(id);
    } else if args.kind == "collective" {
        kind_id = "mlp.grad.v1";
        println!(
            "-- synthetic dataset and one collective training ({} rounds × 4 micro-batch tasks of 64 examples, adam, hidden 32)",
            args.rounds
        );
        gg_text(
            &server,
            &researcher_token,
            &[
                "data",
                "gen-mlp",
                "--d-input",
                "16",
                "--d-output",
                "4",
                "--d-teacher",
                "32",
                "--n-train",
                "4096",
                "--n-eval",
                "64",
                "--seed",
                "7",
                "--noise",
                "0.05",
                "--out-dir",
                data.to_str().unwrap(),
            ],
        )?;
        let rounds = args.rounds.to_string();
        let mut argv: Vec<&str> = vec![
            "job",
            "collective",
            "--project",
            &project,
            "--name",
            "demo-collective",
            "--hidden",
            "32",
            "--optimizer",
            "adam",
            "--lr",
            "0.02",
            "--batch",
            "256",
            "--tasks",
            "4",
            "--rounds",
            &rounds,
            "--seed",
            "1",
            "--retries",
            "2",
        ];
        for i in &collective_inputs {
            argv.push("--input");
            argv.push(i);
        }
        let r = gg(&server, &researcher_token, &argv)?;
        let id = r["run_id"].as_str().unwrap().to_owned();
        println!(
            "  run {id}: state {} step {}/{} (round 0 job {})",
            r["state"], r["step"], r["rounds_total"], r["rounds"][0]["job_id"]
        );
        run_id = Some(id);
    } else if args.kind == "mlp" {
        kind_id = "mlp.train.v1";
        println!(
            "-- synthetic dataset and a sweep of 12 independent trainings (hidden 32,64 × lr 0.02,0.05 × 3 seeds)"
        );
        gg_text(
            &server,
            &researcher_token,
            &[
                "data",
                "gen-mlp",
                "--d-input",
                "16",
                "--d-output",
                "4",
                "--d-teacher",
                "32",
                "--n-train",
                "4096",
                "--n-eval",
                "1024",
                "--seed",
                "7",
                "--noise",
                "0.05",
                "--out-dir",
                data.to_str().unwrap(),
            ],
        )?;
        let inputs: Vec<String> = ["train_x", "train_y", "eval_x", "eval_y"]
            .iter()
            .map(|n| format!("{n}={}", data.join(format!("{n}.ggt")).display()))
            .collect();
        let key = format!("demo-sweep-{stamp}");
        let mut argv: Vec<&str> = vec![
            "job",
            "sweep",
            "--project",
            &project,
            "--name",
            "mlp-grid",
            "--hidden",
            "32,64",
            "--lr",
            "0.02,0.05",
            "--optimizer",
            "adam",
            "--batch",
            "64",
            "--steps",
            "300",
            "--seeds",
            "3",
            "--idempotency-key",
            &key,
        ];
        for i in &inputs {
            argv.push("--input");
            argv.push(i);
        }
        let j = gg(&server, &researcher_token, &argv)?;
        let job_id = j["job_id"].as_str().unwrap().to_owned();
        let j2 = gg(&server, &researcher_token, &argv)?;
        anyhow::ensure!(
            j2["job_id"] == j["job_id"],
            "idempotent retry created another job"
        );
        expected_tasks = j["tasks"].as_array().map(|a| a.len()).unwrap_or(0);
        println!(
            "  submitted sweep {job_id} with {expected_tasks} tasks (reservation {} TC per attempt)",
            j["tasks"][0]["reservation_per_attempt"]
        );
        jobs.push(job_id);
    } else {
        kind_id = "tensor.matmul.v1";
        println!(
            "-- inputs and {} matmul jobs ({}x{}x{})",
            args.jobs, args.dim, args.dim, args.dim
        );
        for i in 0..args.jobs {
            let a = data.join(format!("a{i}.ggt"));
            let mut b = data.join(format!("b{i}.ggt"));
            let d = args.dim.to_string();
            gg_text(
                &server,
                &researcher_token,
                &[
                    "data",
                    "gen-matmul",
                    "--m",
                    &d,
                    "--k",
                    &d,
                    "--n",
                    &d,
                    "--seed",
                    &(1000 + i as u64).to_string(),
                    "--out-a",
                    a.to_str().unwrap(),
                    "--out-b",
                    b.to_str().unwrap(),
                ],
            )?;
            // With peers, every job multiplies by the same `b`: workers then fetch the same
            // artifact, from each other, and the lying peer (`--faults`) is really asked for it.
            if args.p2p && !args.node && i > 0 {
                b = data.join("b0.ggt");
            }
            if let Some(lab) = &lab {
                // The node holds the inputs and pulls the outputs; the job names it.
                let project_id = gg_core::ProjectId(project.parse()?);
                let ra = lab.rt.block_on(lab.node.hold(project_id, &a, "ggt1-f32"))?;
                let rb = lab.rt.block_on(lab.node.hold(project_id, &b, "ggt1-f32"))?;
                node_inputs.extend([ra.hash, rb.hash]);
                let def =
                    gg_compute::catalog::matmul_definition(args.dim, args.dim, args.dim, ra, rb)?;
                let r = reqwest::blocking::Client::new()
                    .post(format!("{server}/v1/researcher/projects/{project}/jobs"))
                    .bearer_auth(&researcher_token)
                    .json(&json!({ "name": format!("matmul-{i}"), "tasks": [def], "output_holder": lab.node.id() }))
                    .send()?;
                anyhow::ensure!(r.status().is_success(), "submit: {}", r.text()?);
                let j: Value = r.json()?;
                jobs.push(j["job_id"].as_str().unwrap().to_owned());
                expected_tasks += 1;
                continue;
            }
            let key = format!("demo-{stamp}-{i}");
            let argv = [
                "job",
                "submit",
                "--project",
                &project,
                "--name",
                &format!("matmul-{i}"),
                "--input",
                &format!("a={}", a.display()),
                "--input",
                &format!("b={}", b.display()),
                "--idempotency-key",
                &key,
            ];
            let j = gg(&server, &researcher_token, &argv)?;
            let job_id = j["job_id"].as_str().unwrap().to_owned();
            let j2 = gg(&server, &researcher_token, &argv)?;
            anyhow::ensure!(
                j2["job_id"] == j["job_id"],
                "idempotent retry created another job"
            );
            jobs.push(job_id);
            expected_tasks += 1;
        }
        println!("  submitted {} jobs", jobs.len());
    }
    if args.kind == "matmul" && lab.is_none() {
        let dry = gg(
            &server,
            &researcher_token,
            &[
                "job",
                "submit",
                "--project",
                &project,
                "--dry-run",
                "--input",
                &format!("a={}", data.join("a0.ggt").display()),
                "--input",
                &format!("b={}", data.join("b0.ggt").display()),
            ],
        )?;
        println!(
            "  dry-run estimate: reservation {} TC, est_cpu_secs {}",
            dry["tasks"][0]["reservation_per_attempt"], dry["tasks"][0]["estimate"]["est_cpu_secs"]
        );
    }

    println!("-- workers");
    let engine = if args.gpu { "gpu" } else { "cpu" };
    let mut worker_names = Vec::new();
    let start_worker =
        |procs: &mut Procs, name: &str, faults: Option<&str>| -> anyhow::Result<String> {
            let dir = root.join(name);
            std::fs::create_dir_all(&dir)?;
            let cfg = dir.join("worker.toml");
            // With replicas, workers also compare two computations when a lab node asks for it.
            let kinds = if args.replicas {
                format!("{kind_id}\", \"replica.compare.v1")
            } else {
                kind_id.to_owned()
            };
            write_worker_config(&cfg, &dir, &server, name, engine, &kinds, faults, args.p2p)?;
            let out = Command::new(&worker_bin)
                .args(["--config", cfg.to_str().unwrap(), "init"])
                .output()?;
            anyhow::ensure!(
                out.status.success(),
                "worker init: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            let pubkey = String::from_utf8_lossy(&out.stdout).trim().to_owned();
            let doctor = Command::new(&worker_bin)
                .args(["--config", cfg.to_str().unwrap(), "doctor"])
                .output()?;
            std::fs::write(logs.join(format!("{name}-doctor.txt")), &doctor.stdout)?;
            anyhow::ensure!(
                doctor.status.success(),
                "worker doctor refused {name}:\n{}",
                String::from_utf8_lossy(&doctor.stdout)
            );
            let reg = gg_text(
                &server,
                "",
                &[
                    "worker",
                    "register",
                    "--key",
                    dir.join("worker.key").to_str().unwrap(),
                    "--name",
                    name,
                ],
            )?;
            let worker_id = reg
                .lines()
                .find_map(|l| l.strip_prefix("worker_id: "))
                .unwrap_or_default()
                .to_owned();
            gg(&server, &admin, &["admin", "worker-authorize", &worker_id])?;
            let mut cmd = Command::new(&worker_bin);
            cmd.args(["--config", cfg.to_str().unwrap(), "run"])
                .env("RUST_LOG", "info");
            procs.spawn(name, cmd, &logs.join(format!("{name}.log")))?;
            println!("  {name}: id {worker_id} key {pubkey}");
            Ok(worker_id)
        };
    // The fragile worker starts alone and its faults are staged before any other worker runs:
    // nothing else can advance the work meanwhile, so each fault happens whatever the speed of the
    // machine (rounds of a small model take a fraction of a second).
    if args.faults {
        start_worker(
            &mut procs,
            "worker-fragile",
            Some("hold_before_submit_secs = 30"),
        )?;
        worker_names.push("worker-fragile".to_owned());
    }
    let fragile_holds = || -> usize {
        std::fs::read_to_string(logs.join("worker-fragile.log"))
            .unwrap_or_default()
            .matches("DEV FAULT: holding attempt")
            .count()
    };
    let mut cancelled_job: Option<String> = None;
    if args.faults && run_id.is_none() {
        println!("-- fault: interrupting worker-fragile while it holds a lease");
        loop {
            check_deadline("waiting for the fragile worker to hold a lease")?;
            if fragile_holds() >= 1 {
                std::thread::sleep(Duration::from_secs(2));
                procs.kill("worker-fragile");
                break;
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    }
    if args.faults
        && let Some(run) = &run_id
    {
        // 1. The fragile worker holds a task of the open round for 30 s: cancel that round's job
        //    meanwhile. Its late submission must be refused (never integrated), the round reissued.
        println!(
            "-- fault: cancelling the open round's job while worker-fragile holds one of its tasks"
        );
        loop {
            check_deadline("waiting for the fragile worker to hold a lease")?;
            if fragile_holds() >= 1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(300));
        }
        let st = gg(
            &server,
            &researcher_token,
            &["job", "collective-status", run],
        )?;
        let open = st["rounds"]
            .as_array()
            .and_then(|a| a.iter().find(|r| r["state"] == "open"))
            .cloned()
            .ok_or_else(|| {
                anyhow::anyhow!("no open round while the fragile worker holds a task")
            })?;
        let job = open["job_id"].as_str().unwrap().to_owned();
        let c = gg(
            &server,
            &researcher_token,
            &["job", "cancel", &job, "--yes"],
        )?;
        println!(
            "  cancelled job {job} of round {} attempt {}: {c}",
            open["step"], open["attempt"]
        );
        cancelled_job = Some(job);
        // The worker heartbeats while holding: the server answers 409 (attempt superseded) and the
        // worker drops the held result without ever submitting it.
        println!(
            "-- fault: waiting for the fragile worker to drop the held result of the cancelled round"
        );
        loop {
            check_deadline("waiting for the held attempt to be dropped")?;
            let log = std::fs::read_to_string(logs.join("worker-fragile.log")).unwrap_or_default();
            if log.contains("lease lost while holding") {
                break;
            }
            std::thread::sleep(Duration::from_millis(300));
        }
        // 2. It then holds a task of the reissued round: interrupt it so that its lease expires.
        println!("-- fault: interrupting worker-fragile while it holds a lease");
        loop {
            check_deadline("waiting for the fragile worker to hold a second lease")?;
            if fragile_holds() >= 2 {
                std::thread::sleep(Duration::from_secs(2));
                procs.kill("worker-fragile");
                break;
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    let mut dishonest_id = None;
    if args.faults {
        dishonest_id = Some(start_worker(
            &mut procs,
            "worker-dishonest",
            Some("corrupt_outputs = true\nreplay_submission = true"),
        )?);
        // crashy worker: its executor is killed after the first checkpoint and must resume locally
        start_worker(
            &mut procs,
            "worker-crashy",
            Some("crash_executor_once = true\nreplay_submission = true"),
        )?;
        worker_names.push("worker-crashy".to_owned());
        if args.p2p {
            // A peer that answers chunk requests with wrong bytes: readers must catch it on the
            // signed manifest, drop it and finish from another source.
            start_worker(
                &mut procs,
                "worker-liar",
                Some("serve_corrupt_chunks = true"),
            )?;
            worker_names.push("worker-liar".to_owned());
            // A liar can only lie about what it holds. In a swarm, the other workers may finish
            // their downloads before it has anything, so it is allowed to fetch its inputs first,
            // so that the check really covers "a holder that lies".
            let log = logs.join("worker-liar.log");
            loop {
                check_deadline("waiting for the lying peer to hold its inputs")?;
                if std::fs::read_to_string(&log)
                    .unwrap_or_default()
                    .contains("inputs ready")
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
    for i in 0..args.workers {
        let name = format!("worker-{i}");
        start_worker(&mut procs, &name, Some("replay_submission = true"))?;
        worker_names.push(name);
    }
    if args.external_workers > 0 {
        println!(
            "-- waiting for {} external worker(s): port {port}, network {network}",
            args.external_workers
        );
        let mut external = std::collections::BTreeSet::new();
        while external.len() < args.external_workers as usize {
            check_deadline("waiting for external workers")?;
            let list = gg(&server, &admin, &["admin", "worker-list"])?;
            for w in list.as_array().cloned().unwrap_or_default() {
                if w["authorized"] == false {
                    let id = w["worker_id"].as_str().unwrap_or_default().to_owned();
                    gg(&server, &admin, &["admin", "worker-authorize", &id])?;
                    println!("  external {}: id {id} authorized", w["display_name"]);
                    external.insert(id);
                }
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    let mut run_view = Value::Null;
    if let Some(run) = &run_id {
        println!("-- waiting for the collective run to finish (bounded)");
        loop {
            check_deadline("waiting for the collective run")?;
            run_view = gg(
                &server,
                &researcher_token,
                &["job", "collective-status", run],
            )?;
            let st = run_view["state"].as_str().unwrap_or("?");
            if matches!(st, "completed" | "failed" | "cancelled") {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        println!(
            "  run {run}: state {} step {}/{} failure {:?}",
            run_view["state"], run_view["step"], run_view["rounds_total"], run_view["failure"]
        );
        for r in run_view["rounds"].as_array().cloned().unwrap_or_default() {
            jobs.push(r["job_id"].as_str().unwrap().to_owned());
        }
    }

    // The operator's role in a dispute: settle on an outcome a validator actually signed. The demo
    // plays that role explicitly; nothing settles itself.
    let mut settled: Vec<Value> = Vec::new();
    let settle_disputes = |settled: &mut Vec<Value>| -> anyhow::Result<()> {
        if !args.federated {
            return Ok(());
        }
        let open = gg(&server, &admin, &["admin", "dispute-list"])?;
        for d in open.as_array().cloned().unwrap_or_default() {
            let id = d["dispute_id"].as_str().unwrap_or_default().to_owned();
            // Settle on the outcome the recomputing validator signed.
            let outcome = d["opinions"]
                .as_array()
                .and_then(|o| {
                    o.iter()
                        .find(|x| x["outcome"] == "rejected")
                        .or_else(|| o.iter().find(|x| x["outcome"] == "accepted"))
                })
                .and_then(|x| x["outcome"].as_str())
                .unwrap_or("rejected")
                .to_owned();
            let r = gg(
                &server,
                &admin,
                &[
                    "admin",
                    "dispute-resolve",
                    &id,
                    "--outcome",
                    &outcome,
                    "--reason",
                    "operator reviewed the signed recomputation",
                    "--yes",
                ],
            )?;
            println!("  dispute {id} settled as {outcome} by the operator");
            settled.push(r);
        }
        Ok(())
    };

    println!("-- waiting for all jobs to finish (bounded)");
    let mut summary;
    let mut task_rows: Vec<Value> = Vec::new();
    loop {
        check_deadline("waiting for jobs")?;
        let mut done = 0;
        let mut states = Vec::new();
        task_rows.clear();
        for job in &jobs {
            let st = gg(&server, &researcher_token, &["job", "status", job])?;
            let s = st["state"].as_str().unwrap_or("?").to_owned();
            if matches!(s.as_str(), "succeeded" | "failed" | "cancelled") {
                done += 1;
            }
            for t in st["tasks"].as_array().cloned().unwrap_or_default() {
                task_rows.push(json!({ "job_id": job, "task_id": t["task_id"], "state": t["state"], "attempts_made": t["attempts_made"] }));
            }
            states.push(json!({ "job_id": job, "state": s }));
        }
        summary = json!({ "jobs": states, "tasks": task_rows });
        if done == jobs.len() {
            break;
        }
        settle_disputes(&mut settled)?;
        std::thread::sleep(Duration::from_secs(1));
    }
    let succeeded = summary["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|j| j["state"] == "succeeded")
        .count();
    let accepted_tasks = task_rows
        .iter()
        .filter(|t| t["state"] == "accepted")
        .count();
    let cancelled_tasks = task_rows
        .iter()
        .filter(|t| t["state"] == "cancelled")
        .count();
    if run_id.is_some() {
        expected_tasks = task_rows.len();
    }
    println!(
        "  {succeeded}/{} jobs succeeded, {accepted_tasks}/{expected_tasks} tasks accepted in {:.1}s",
        jobs.len(),
        started.elapsed().as_secs_f64()
    );
    let mut comparison = Value::Null;
    if args.kind == "mlp" {
        println!("-- comparison of the grid (eval loss per configuration, best first)");
        let cmp_file = root.join("comparison.json");
        let table = gg_text(
            &server,
            &researcher_token,
            &[
                "job",
                "compare",
                &jobs[0],
                "--out",
                cmp_file.to_str().unwrap(),
            ],
        )?;
        for line in table.lines().take(14) {
            println!("  {line}");
        }
        comparison = serde_json::from_str(&std::fs::read_to_string(&cmp_file)?)?;
    }

    println!("-- receipts and ledger");
    let receipts_file = root.join("receipts.json");
    gg_text(
        &server,
        &researcher_token,
        &[
            "project",
            "receipts",
            &project,
            "--out",
            receipts_file.to_str().unwrap(),
        ],
    )?;
    let verify = gg_text(
        &server,
        "",
        &[
            "receipt",
            "verify",
            receipts_file.to_str().unwrap(),
            "--server-key",
            &server_key,
            "--network",
            network,
        ],
    )?;
    println!("  {}", verify.lines().last().unwrap_or(""));
    let receipts: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(&receipts_file)?)?;
    let budget = gg(
        &server,
        &researcher_token,
        &["ledger", "balance", "--project", &project],
    )?;
    let ledger = gg(&server, &researcher_token, &["project", "ledger", &project])?;
    let workers = gg(&server, &admin, &["admin", "worker-list"])?;
    let tasks_summary = gg(&server, &admin, &["admin", "tasks-summary"])?;
    println!(
        "  budget: available {} reserved {} spent {}",
        budget["available"], budget["reserved"], budget["spent"]
    );
    println!("  receipts: {}", receipts.len());

    let mut checks = Vec::new();
    let mut ok = true;
    let mut push = |name: &str, pass: bool, detail: String| {
        println!(
            "  [{}] {name}: {detail}",
            if pass { "PASS" } else { "FAIL" }
        );
        checks.push(json!({ "name": name, "pass": pass, "detail": detail }));
        ok &= pass;
    };
    let cancelled_jobs = usize::from(cancelled_job.is_some());
    push(
        "all_jobs_succeeded",
        succeeded + cancelled_jobs == jobs.len(),
        format!(
            "{succeeded}/{} (+{cancelled_jobs} cancelled on purpose)",
            jobs.len()
        ),
    );
    push(
        "all_tasks_accepted",
        accepted_tasks + cancelled_tasks == expected_tasks
            && (cancelled_tasks == 0 || cancelled_job.is_some()),
        format!("{accepted_tasks}/{expected_tasks} (+{cancelled_tasks} cancelled with the job)"),
    );
    // With replicas, one receipt per accepted computation: two or three per task (ADR-0042).
    let per_task = if args.replicas { 2..=3 } else { 1..=1 };
    push(
        "one_receipt_per_computation",
        receipts.len() >= accepted_tasks * per_task.start()
            && receipts.len() <= accepted_tasks * per_task.end(),
        format!(
            "{} receipts for {accepted_tasks} accepted tasks, {per_task:?} computations each (replays created none)",
            receipts.len()
        ),
    );
    let mut equivalence = String::new();
    if let Some(run) = &run_id {
        let rounds_total = run_view["rounds_total"].as_u64().unwrap_or(0);
        push(
            "collective_run_completed",
            run_view["state"] == "completed" && run_view["step"].as_u64() == Some(rounds_total),
            format!(
                "state {} step {}/{rounds_total}",
                run_view["state"], run_view["step"]
            ),
        );
        let steps: Vec<u64> = run_view["checkpoint_steps"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_u64()).collect())
            .unwrap_or_default();
        push(
            "one_checkpoint_per_step",
            steps == (0..=rounds_total).collect::<Vec<u64>>(),
            format!("published checkpoints for steps {steps:?}"),
        );
        println!(
            "-- recomputing the run in one process and comparing with the published checkpoint"
        );
        let mut argv: Vec<&str> = vec!["job", "collective-check", run];
        for i in &collective_inputs {
            argv.push("--input");
            argv.push(i);
        }
        // A run the lab node stepped: its checkpoint and datasets are read from the node's store.
        let store = root.join("node").join("store");
        let store = store.to_string_lossy().into_owned();
        if lab.is_some() {
            argv.extend(["--store", &store]);
        }
        equivalence = match gg_text(&server, &researcher_token, &argv) {
            Ok(t) => t,
            Err(e) => format!("collective-check failed: {e}"),
        };
        for line in equivalence.lines() {
            println!("  {line}");
        }
        push(
            "collective_equals_single_process_reference",
            equivalence.contains("collective-check: EQUIVALENT"),
            "distributed checkpoint equals the single-process reference within tolerance".into(),
        );
        if let Some(job0) = &cancelled_job {
            let rounds = run_view["rounds"].as_array().cloned().unwrap_or_default();
            let failed = rounds
                .iter()
                .find(|r| r["state"] == "failed" && r["job_id"] == json!(job0))
                .cloned()
                .unwrap_or(Value::Null);
            let step = failed["step"].clone();
            let next_attempt = failed["attempt"].as_i64().unwrap_or(0) + 1;
            let reissued = rounds.iter().any(|r| {
                r["step"] == step && r["attempt"] == next_attempt && r["state"] == "aggregated"
            });
            push(
                "cancelled_round_reissued",
                !failed.is_null() && reissued,
                format!(
                    "round {step}: attempt {} failed (job cancelled), attempt {next_attempt} aggregated; {} rounds in total",
                    failed["attempt"],
                    rounds.len()
                ),
            );
            // The other tasks of the cancelled round may have been verified and paid before the
            // cancellation (honest work, rounds are fast); the held result must never be.
            let flog = std::fs::read_to_string(logs.join("worker-fragile.log")).unwrap_or_default();
            let held: Vec<&str> = flog
                .lines()
                .filter(|l| l.contains("lease lost while holding"))
                .filter_map(|l| l.split("attempt_id=").nth(1))
                .filter_map(|rest| rest.split(['}', ' ', ':']).next())
                .collect();
            let held_receipts = receipts
                .iter()
                .filter(|r| r["attempt_id"].as_str().is_some_and(|a| held.contains(&a)))
                .count();
            let receipts_carry_attempt = receipts.iter().all(|r| r["attempt_id"].is_string());
            push(
                "held_result_of_cancelled_round_never_paid",
                !held.is_empty()
                    && flog.contains("attempt is superseded")
                    && receipts_carry_attempt
                    && held_receipts == 0,
                format!(
                    "fragile worker dropped {} held result(s) after the server's 409 (attempt superseded); {held_receipts} receipts reference them",
                    held.len()
                ),
            );
        }
    }
    let spent: u64 = budget["spent"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let expected_spent = receipts
        .iter()
        .map(|r| {
            r["amount"]
                .as_str()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0)
        })
        .sum::<u64>()
        // A validator's recomputation is charged like the computation; with replicas, the other
        // computations are the verification, each paid its own receipt (ADR-0042).
        * if args.replicas { 1 } else { 2 };
    push(
        "ledger_spent_matches_receipts",
        spent == expected_spent,
        format!("spent {spent} TC = rewards + verification {expected_spent} TC"),
    );
    push(
        "nothing_left_reserved",
        budget["reserved"] == "0",
        format!("reserved {}", budget["reserved"]),
    );
    if args.kind == "mlp" {
        let rows = comparison["rows"].as_array().cloned().unwrap_or_default();
        let with_loss = rows.iter().filter(|r| r["eval_loss"].is_number()).count();
        push(
            "comparison_collected",
            with_loss == expected_tasks,
            format!("{with_loss}/{expected_tasks} accepted trainings with recomputable eval loss"),
        );
    }
    if args.faults {
        let attempts_total: u64 = task_rows
            .iter()
            .map(|t| t["attempts_made"].as_u64().unwrap_or(0))
            .sum();
        push(
            "reassignment_happened",
            attempts_total > expected_tasks as u64,
            format!("{attempts_total} attempts for {expected_tasks} tasks"),
        );
        let dishonest = dishonest_id.as_deref().unwrap_or("");
        let dishonest_receipts = receipts
            .iter()
            .filter(|r| r["worker_id"] == dishonest)
            .count();
        push(
            "corrupted_results_never_paid",
            dishonest_receipts == 0,
            format!("dishonest worker receipts: {dishonest_receipts}"),
        );
        // Every validator writes its own log (federated mode runs several).
        let vlogs: String = ["gg-verifier", "verifier-b", "verifier-vouching"]
            .iter()
            .filter_map(|n| std::fs::read_to_string(logs.join(format!("{n}.log"))).ok())
            .collect();
        let rejected = vlogs.matches("outcome=rejected").count();
        push(
            "corrupted_results_rejected",
            rejected > 0,
            format!("{rejected} rejected decisions in the validator logs"),
        );
        let slog = std::fs::read_to_string(logs.join("server.log")).unwrap_or_default();
        push(
            "expired_lease_reaped",
            slog.contains("lease expired"),
            "server log shows the reaper re-queueing the interrupted attempt".into(),
        );
        if args.kind == "mlp" {
            let clog = std::fs::read_to_string(logs.join("worker-crashy.log")).unwrap_or_default();
            push(
                "executor_crash_resumed_locally",
                clog.contains("resuming from local checkpoint")
                    && clog.contains("result submitted"),
                "crashy worker log shows a local resume followed by a submission".into(),
            );
        }
    }
    let wlogs: String = worker_names
        .iter()
        .filter_map(|n| std::fs::read_to_string(logs.join(format!("{n}.log"))).ok())
        .collect();
    if args.p2p {
        let all_logs: String = worker_names
            .iter()
            .chain(std::iter::once(&"worker-dishonest".to_owned()))
            .filter_map(|n| std::fs::read_to_string(logs.join(format!("{n}.log"))).ok())
            .collect();
        let peer_chunks: u64 = all_logs
            .split("peer_chunks=")
            .skip(1)
            .filter_map(|s| s.split_whitespace().next())
            .filter_map(|s| s.parse::<u64>().ok())
            .sum();
        let http_chunks: u64 = all_logs
            .split("http_chunks=")
            .skip(1)
            .filter_map(|s| s.split_whitespace().next())
            .filter_map(|s| s.parse::<u64>().ok())
            .sum();
        push(
            "artifact_chunks_served_by_peers",
            peer_chunks > 0,
            format!("{peer_chunks} chunks from peers, {http_chunks} from the control plane"),
        );
        let served = all_logs.matches("peer sharing enabled").count();
        push(
            "sharing_is_explicit",
            served == worker_names.len() + usize::from(args.faults),
            format!("{served} workers announced a chunk server"),
        );
        if args.faults {
            let detections = all_logs.matches("do not match the signed manifest").count();
            push(
                "lying_peer_detected_and_dropped",
                detections > 0,
                format!("{detections} peer chunks rejected against the signed manifest"),
            );
        }
    }
    if args.federated {
        let operators = gg(&server, &admin, &["admin", "operator-list"])?;
        let named = operators
            .as_array()
            .map(|a| a.iter().filter(|o| o["name"] != "unassigned").count())
            .unwrap_or(0);
        push(
            "validators_belong_to_distinct_operators",
            named >= 2,
            format!("{named} named operators registered"),
        );
        let two_operator_payments = receipts
            .iter()
            .filter(|r| {
                r["receipt"]["body"]["decision_hashes"]
                    .as_array()
                    .map(|h| h.len() >= 2)
                    .unwrap_or(false)
            })
            .count();
        push(
            "every_payment_backed_by_two_signed_decisions",
            !receipts.is_empty() && two_operator_payments == receipts.len(),
            format!(
                "{two_operator_payments}/{} receipts reference two signed decisions",
                receipts.len()
            ),
        );
        if args.faults {
            let all_disputes = gg(&server, &admin, &["admin", "dispute-list", "--all"])?;
            let total = all_disputes.as_array().map(|a| a.len()).unwrap_or(0);
            push(
                "disagreement_opened_a_dispute",
                total > 0,
                format!("{total} disputes recorded between distinct operators"),
            );
            push(
                "disputes_settled_by_an_operator",
                !settled.is_empty() && settled.iter().all(|r| r["receipt_id"].is_null()),
                format!(
                    "{} disputes settled by an operator, none of them paid a corrupted result",
                    settled.len()
                ),
            );
        }
    }
    if let Some(lab) = &lab {
        let outputs: Vec<gg_core::Sha256Digest> = receipts
            .iter()
            .flat_map(|r| {
                r["receipt"]["body"]["output_hashes"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
            })
            .filter_map(|h| h.as_str().and_then(|s| s.parse().ok()))
            .collect();
        let all: Vec<gg_core::Sha256Digest> =
            node_inputs.iter().chain(outputs.iter()).copied().collect();
        let held = all
            .iter()
            .filter(|h| lab.rt.block_on(lab.node.holds(h)))
            .count();
        push(
            "node_holds_inputs_and_outputs",
            !outputs.is_empty() && held == all.len(),
            format!(
                "{held}/{} artifacts ({} inputs, {} outputs) held by the lab node",
                all.len(),
                node_inputs.len(),
                outputs.len()
            ),
        );
        let mut stored = std::collections::HashSet::new();
        let mut stack = vec![server_dir.clone()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d)?.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if let Some(n) = p.file_name().and_then(|n| n.to_str()) {
                    stored.insert(n.to_owned());
                }
            }
        }
        let on_server = all.iter().filter(|h| stored.contains(&h.to_hex())).count();
        push(
            "server_stores_no_node_artifact",
            on_server == 0,
            format!("{on_server} of them in the control plane's object store"),
        );
        let vlog = std::fs::read_to_string(logs.join("gg-verifier.log")).unwrap_or_default();
        push(
            "validator_read_through_peers",
            vlog.contains("read through peers") || vlog.contains("input prefetched"),
            "the validator recomputed from artifacts only peers hold".into(),
        );
    }
    // Only the demo's own workers replay their submissions (external workers run as configured).
    if args.workers > 0 {
        push(
            "replayed_submissions_not_recorded_twice",
            wlogs.contains("recorded=false same_hash=true"),
            "worker logs show idempotent replay answers".into(),
        );
    }

    let report = json!({
        "started_unix": stamp,
        "elapsed_secs": started.elapsed().as_secs_f64(),
        "server": server,
        "network": network,
        "server_public_key": server_key,
        "database": db_name,
        "kind": args.kind,
        "faults": args.faults,
        "gpu": args.gpu,
        "jobs": summary["jobs"],
        "tasks": summary["tasks"],
        "comparison": comparison,
        "collective_run": run_view,
        "collective_check": equivalence,
        "budget": budget,
        "ledger_entries": ledger["entries"].as_array().map(|a| a.len()).unwrap_or(0),
        "receipts": receipts.len(),
        "workers": workers,
        "tasks_summary": tasks_summary,
        "checks": checks,
        "ok": ok,
    });
    std::fs::write(
        root.join("report.json"),
        serde_json::to_string_pretty(&report)?,
    )?;
    println!("-- report: {}", root.join("report.json").display());
    drop(procs);
    if let Some(lab) = lab {
        let _ = lab.stop.send(true);
        lab.rt.block_on(lab.node.stop());
    }
    anyhow::ensure!(ok, "demo checks failed (see report)");
    println!("== demo OK ({:.1}s) ==", started.elapsed().as_secs_f64());
    Ok(())
}
