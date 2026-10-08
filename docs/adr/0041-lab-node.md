# ADR-0041 — The lab node: the scientist's machine holds, serves and aggregates

Status: **proposed** (2026-09-25). Decided with the owner: the server only brings together those
who lend GPUs and those who need computation, and the costs go to the scientist's machine. Peers
keep relaying to one another. A server relay is allowed for small artifacts only.

## Context

The control plane stores and serves every artifact:

- the datasets and checkpoints (540 MB per SmolLM2 checkpoint, sent to every worker every round);
- the workers' gradients;
- the optimizer state.

It also runs the optimizer step (ADR-0036: 3 s, 4 GB of memory and 2 GB written per SmolLM2 round).
On a server shared with other services, that is what loads it. None of it needs to be on the
server: the scientist is the one who needs the computation, and can pay for it with their own
machine.

## Decision

**A lab node** runs on the scientist's machine, under a key tied to the researcher's account.

- **Invisible to the scientist.** It is the Rust binary `gg-node`, shipped inside the Python package
  (a `maturin` wheel with the binary in it; Linux x86_64 for the beta). `trainer.fit()` starts it
  when needed, and there is nothing else to install or run.
- **It keeps running in the background** until the scientist's runs have ended, even after the
  notebook or the Python process has exited (owner's choice). It has its state directory under
  `~/.local/share/gpugoblin/node`, a log there, a lock file, and it stops by itself when it has no run
  left.
- **One implementation.** The Python code does not reimplement the node: the optimizer steps stay
  bit for bit those of the tested Rust code, and the peer protocol keeps its checks.

For the runs of its projects, the node:

1. **Holds the artifacts**: datasets, initial and later checkpoints, the gradients and outputs it
   pulls from workers. It computes the SHA-256 of each chunk and of the whole, and declares them to
   the server.
2. **Serves them to workers** through the peer-to-peer protocol that already exists (ADR-0012):
   - the server signs the chunk manifest from the node's declared digests, as ADR-0026 already does
     for a producer's activations;
   - a reader checks every chunk against it and the whole object against its hash;
   - **the swarm stays:** workers re-serve what they downloaded, with rendezvous hashing, so the
     node's uplink sends about one copy per round, not one per worker.
3. **Pulls outputs** instead of receiving uploads. A worker keeps its outputs, declares their
   digests, and serves them. The node pulls them, checks them, and records them as held. A worker
   is released once the node holds its outputs; if the node never gets them in time, the task is
   reissued.
4. **Aggregates and steps.** It runs the same code as the server today (`aggregate_named`,
   `apply_step_named`, the ADR-0036 memory, the ADR-0035 bf16 gradients) on the gradients the
   verification accepted. It stores the new checkpoint, declares it, and reports its references to
   the server in a message signed by the node's key. The server opens the next round with them.
5. **Applies the retention policy** (ADR-0040) to its own disk.

**The server keeps** what is small:

- accounts and admission;
- scheduling, leases and signed assignments;
- chunk manifests and peer tickets;
- verification bookkeeping (ADR-0042);
- the ledger, receipts and disputes;
- the rendezvous of peers.

It stores hashes, sizes and small JSON, never a checkpoint.

**Reachability** (decision 2 of the owner). The server helps the lab and the contributors reach each
other through firewalls without carrying their data, cheapest way first:

1. **Direct.** The node opens a port on the router (UPnP, NAT-PMP) and is checked by dial-back, as
   a worker is.
2. **Reversed.** When a contributor is reachable and the node is not, the node connects out to it.
   The server only says which way.
3. **Hole punching.** The server exchanges the addresses each side is seen at, and both connect at
   once. It needs the peer protocol over QUIC (UDP; ADR-0013's open decision) and comes last.
4. **Relay, as a last resort.**
   - Through a reachable contributor, paid in credits for it, so the server stays light.
   - Through the server itself, up to `node_relay_max_bytes` (64 MiB by default).

   Relayed chunks are still checked against the signed manifest: a relay cannot alter anything.
   When nothing works for a larger artifact, the run waits and says why.

**Availability.** The run needs the node online.

- When it is away, rounds wait. Nothing is lost: the node's disk holds the state, and ADR-0036's
  replay rebuilds the moments.
- Only the scientist's own run is delayed.

## What changes in the invariants (to be written into CLAUDE.md)

- **"An artifact is referenced only after durable storage and a SHA-256 check"** — *exception*.
  - The durable copy of a lab-node run's artifacts is **the node's**: declared chunk digests, the
    whole-object hash checked by every reader, and a server-signed manifest.
  - The server holds no copy. If the node loses an artifact, only its own run stops.
  - Nothing is verified, aggregated or paid until the holder has it and it matches: the ADR-0026 rule,
    generalized.
- **"Nothing is paid while a verification is missing or has not accepted"**: unchanged.

## Consequences

- The server's disk, memory, CPU and bandwidth stop depending on the size of the models.
- The scientist's machine needs:
  - the disk for its run (checkpoints kept, a few rounds of intermediates);
  - the memory of the optimizer step;
  - an uplink that serves about one copy of each checkpoint per round, the swarm doing the rest.
- New failure modes, all confined to the scientist's own run:
  - the node offline;
  - the node's disk lost (its own backup).
- The Python package ships and starts the node. The rule of CLAUDE.md that it "only builds
  descriptions" becomes what it always meant: no researcher code goes over the network, only
  descriptions, data and results.

## Plan

Each milestone leaves the grid working end to end, and is tested against a single-process replay,
bit for bit.

- **A1. Shared peer code.** The chunk server and the swarm download move from gg-worker into
  gg-client, where the node and the validator use them too.
- **A2. The node's identity.** A `node` principal tied to a researcher: registered with the
  researcher's token, with sessions by challenge like a worker's.
- **A3. Artifacts held by a node.** The server records the hash, size and chunk digests the node
  declares, signs their manifest, and lets workers and validators with a reason to read them get
  tickets. It stores no copy. The node serves them.
  *Done for workers (2026-09-25)*: `POST /node/artifacts/{hash}` and `.../announce`, the crate
  `gg-node`, `server_holds` in the peer list (a reader keeps asking peers, for up to its request
  timeout, instead of falling back to the server); tests in `gg-node/tests/hold.rs`. Validators
  follow with A5.
- **A4. Outputs pulled by the node.** A worker keeps its outputs and declares their digests; the
  node pulls and checks them.
  *Done (2026-09-25)*: `output_holder` on a job, `serves_peers` at claim time, `GET /node/pulls`,
  node manifests, peer lists and tickets (`PeerTicketV1.reader_id` is now a `ReaderId`, same
  encoding), `POST /node/artifacts/{hash}/pulled`, uploads above `node_relay_max_bytes` refused,
  unpulled outputs past `pending_output_secs` send the task again; tests in `gg-node/tests/pull.rs`.
- **A5. Validators read through the swarm**, until ADR-0042 replaces them.
  *Done (2026-09-25)*: `GET /validator/artifacts/{hash}/manifest` and `…/peers`, the validator reads
  through peers on `409`, node-held inputs are prefetched; an output peers cannot serve releases the
  verification instead of judging the worker. Found on the way: a refused download lost its status
  (`409`), so a worker never turned to peers for a small input only a peer held.
  Demonstrated with real binaries (`cargo xtask demo --kind matmul --node [--faults]`); the node
  pulls each output on its own and gives up on one it cannot get, so the task is sent again at once.
- **A6. The optimizer step on the node.** It pulls the accepted gradients, steps, declares the new
  checkpoint, and reports its references signed. The server's coordinator only schedules.
  *Done (2026-09-25) for data-parallel graph runs over whole datasets*: `holder_node` at run
  creation; the node draws the weights from the seed (step 0), then applies each round with
  `aggregate_named` / `apply_step_named` and reports the checkpoint (`GET /node/steps`,
  `POST /node/runs/{id}/steps`); the server checks it against the shard plan (every shard, sizes,
  held by the node, untrained shards unchanged, optimizer step count) and opens the next round. The
  description stays on the server (small); a dataset's shape comes from the header the node
  declares. The node's requests wait on a server signal instead of polling. Tests:
  `gg-node/tests/train.rs` (bit-exact against a single-process replay, real `gg-verifier`);
  `cargo xtask demo --kind graph --node [--faults]` with real binaries.
  *Not yet*: staged runs, tiling, a supplied starting point, per-task rows (since done), MLP runs (the server
  steps those); the report is authenticated by the node's session and audited, not signed; the
  node keeps no state in memory between rounds (ADR-0036) and applies no retention to its disk.
- **A7. An installed node** (owner's decision of 2026-09-25, replacing "inside the Python package").
  `gg-node` is a Rust program installed on its own, like the worker, and runs as a service. The
  Python library is only told the node's address. The node finds the available control planes at a
  fixed URL on gpugoblin.com (several servers become easy), and reports on itself: where the
  computations are, how many tasks wait. One node can serve several researchers. Decided with the
  owner the same day:
  - **A laboratory's node, attached project by project.** A node is registered once (by a
    researcher responsible for the laboratory, or an operator). Each researcher then attaches it
    to their projects from their own account and can detach it. The node acts only for the
    projects that attached it.
  - **The Python library authenticates to the node with the researcher's GPU Goblin token.** The
    node checks it with the control plane and acts for that researcher only; no second set of
    identities.
  - **The list of servers is signed** offline with the release key that already signs the worker's
    releases; a node refuses a list that is not.
  - **The node's local API listens on its own machine by default**, and can be configured to
    listen on the laboratory's network (then over TLS).
  - Attaching needs the node owner's consent: the researcher who registered the node grants other
    researchers the use of it; a granted researcher attaches it to projects they are a member of.
    Otherwise anyone who learned a node's identifier could make another laboratory's machine work
    for them.
  *Done (2026-09-25)*.
  - Grants, and attachment per project (migration 0025).
  - `gg-node` (`init`, `identity`, `servers`, `run`) with its TOML configuration.
  - The signed list of servers (`gg-servers/v1`, SSHSIG namespace `gpugoblin-servers`, rollback
    refused), and a pinned server key.
  - The local API (`/v1/status`, datasets, runs, progress, checkpoint parts).
  - The installer (`deploy/node/install.sh`, `gg-node.service`, `node.toml.template`).
  - `gg node …` in the CLI, and `node=` in the Python client.
  - Also: a node run may start from a supplied bundle held by the node (fine-tuning), and the node
    reads small gradients the server relayed.
  - Tests: `gg-node/tests/{share,train}.rs`, `gg-python/tests/test_node.py`, and the live Python tests
    through a real node.
- **A8. Reachability**, in the order above (hole punching last).
  *Steps 1, 2 and 3 done (2026-09-25)*.
  - The node finds its address as a worker does. The code is shared in `gg_client::reach`, and the
    order is: configured address, IPv6, NAT-PMP, UPnP, bound address, the address the server observes
    (`/node/me` gives it). Each is checked by the server's dial-back. The node reports the address it
    is reachable at, and removes its UPnP mapping when it stops.
  - Owner's decision the same day: **the server carries no data by default**
    (`node_relay_max_bytes = 0`). The relay below stays as an option for a private deployment. For
    everyone else, an unreachable side is reached by the reversed connection (step 3), where the
    server only puts the two sides in touch.
  - A node nobody can reach still holds its artifacts. When allowed, the server relays the small ones
    (≤ `node_relay_max_bytes`) chunk by chunk (`GET /node/relay`, `PUT /node/relay/{id}`), and
    stores nothing. The reader checks every chunk against the signed manifest. A node that does not
    answer gives the reader a 503, and relayed chunks do not count against the node's quota. Tests:
    `gg-client` unit tests of `reach`, `gg-node/tests/relay.rs`.
  - **The reversed connection (step 3).** A reader that cannot reach the holder, but can itself be
    reached, asks the server (`…/artifacts/{hash}/reverse`). The server dials the reader back, signs
    a push ticket naming the reader's transport key (`PushTicketV1`), and hands the request to the
    holders, which wait for it (`GET /node/pushes`, `GET /worker/pushes`). The holder connects to
    the reader over TLS pinned to that key and pushes the chunks. The reader takes each one only
    under a ticket naming it, for an artifact it waits for, matching the signed manifest. The server
    carries no byte. It serves the three cases: a worker reading an unreachable node's input, a node
    pulling an unreachable worker's output, and a validator reading either (when it is given
    `push_listen`). Tests: `gg-node/tests/reverse.rs` (the ticket shown to another reader is
    refused, a chunk that is not the manifest's too, an address that cannot be dialled back is
    skipped), `gg-node/tests/pull.rs` (an output pushed by a worker nobody can reach).
  - *Not yet*: step 4, hole punching over QUIC, when **both** sides are unreachable (after the
    beta). Until then such a pair cannot exchange an artifact, unless a private deployment turns
    the relay on. A validator without `push_listen` cannot read from an unreachable holder.
- **Decision of 2026-09-25 (owner): the cutting is not done by the server.** The node cuts the model
  into stages, the matrices into tiles, and each round's micro-batch rows. The server only schedules
  what the node has cut. This is the next step after A8, and it lifts the A6 limits (staged runs,
  tiling, datasets above 256 MiB).
  - *Micro-batch rows: done (2026-09-25).* For a run with per-task rows (`batch_inputs`, datasets in
    parts or above 256 MiB), the node cuts the rows of each task, `round_indices(seed, total, start,
    count)` of every dataset held on it, holds them and declares them
    (`GET /node/rows`, `POST /node/runs/{id}/rows`). They are due for the open round and the next
    one, so the node cuts them while the round computes. The server checks the number of tasks, the
    datasets, each size against its slice, and that the node holds them for the project; it reads no
    row. A round replayed at the same step reuses them. Test: `gg-node/tests/train.rs`
    (`a_lab_node_cuts_the_rows_of_each_task`, one dataset in two parts, bit-exact against the
    replay; wrong task count, wrong size, step not due refused).
  - *Stages: done (2026-09-25).* The node's local API turns `stage_memory_bytes` into stage bounds
    (`partition` over the description, as the server read it), and the server is sent the bounds;
    it refuses `stage_memory_bytes` for a node's run. Every job of a staged round names the node as
    output holder: the node pulls and holds the activations, the gradients of activations and the
    partial parameter gradients; the workers pass activations on before that, as before (ADR-0026).
    When every wave of every micro-batch is recorded and accepted, `GET /node/steps` gives each
    micro-batch's partial gradients in production order (`partials`); the node sums each shard's
    in that order, adds a zero gradient for a trained parameter no stage used, and steps with the
    stage shard plan. Tests: `gg-node/tests/train.rs` (`a_lab_node_steps_a_model_cut_into_stages`,
    a transformer in 4 stages, bit-exact against a single-process replay of the same stages), and
    a unit test of the bounds the node computes.
    Found on the way: the peer list held a database connection while it took a second one to
    decide on the relay; as many simultaneous requests as the pool has connections deadlocked it
    for 10 s (`gg-node/tests/hold.rs`, `many_readers_at_once_do_not_exhaust_the_database_pool`,
    fails without the fix). Also, two attempts may declare the same bytes (an activation that does
    not depend on the data): announcing one now checks this worker's declaration, not the first.
  - *Tiles: done (2026-09-25).* The node's local API tiles every matrix above
    `tile_max_param_bytes` (`auto_tile` over the description, functions inlined), keeps the tiled
    description on the control plane with the researcher's token (it is small), and sends the
    server `graph` (tiled), `source_graph` and `tiling`. The server refuses `tile_max_param_bytes`
    for a node's run, and checks that `tiling::apply(source, tiling)` gives the description
    exactly. Stages are then cut over the tiled description. The node draws the tiles from the
    seed, or cuts the weights supplied for the source into tiles at step 0 after checking them
    against the source. Tests: `gg-node/tests/train.rs`
    (`a_lab_node_tiles_a_matrix_and_the_weights_supplied`, bit-exact against the replay of the
    tiled description; a tiling that does not give the description refused), and the installed
    node creating a run through its local API with both `tile_max_param_bytes` and
    `stage_memory_bytes`.
  - With the three, the A6 limits are lifted, except MLP runs, which the server still steps.
- **B. Verification by other contributors** (ADR-0042).
