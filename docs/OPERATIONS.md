# Operations (P1 status)

## Database

- `cargo xtask db up|down|reset` drives a `gpu-goblin-postgres` container (postgres:17-alpine,
  127.0.0.1:5434, no host privileges). Migrations are embedded in `gg-storage` and applied when the
  server starts (`sqlx::migrate!`).
- Backup: `cargo xtask db backup --out backups/<date> [--data-dir var/server]` — `pg_dump -Fc` (via
  the host's `pg_dump` or `docker exec gpu-goblin-postgres`), copy of `objects/` and of `server.key`
  (the key signs receipts: protect it like a secret).
- Restore: `cargo xtask db restore --from backups/<date> --yes` (`pg_restore --clean`), objects
  copied back. Automated test: `gg-control/tests/restore.rs` (dump → restore into a fresh database
  → receipts verifiable, ledger balanced and still append-only).
- Write ordering and reconciliation: the database references objects that already exist on disk;
  after restoring a database older than the store, orphaned objects may remain (harmless); after
  restoring an older store, an `artifacts` row may point to a missing object: the download answers
  404 and `gg admin` must replay the upload.

## Server

- Startup: `gg-server --config deploy/server.example.toml` or `GG_*` variables.
- On restart: expired leases are reclaimed by the reaper; `assigned` tasks whose lease has run out
  go back to `queued` (new generation) or `failed`.
- Server signing key: `GG_DATA_DIR/server.key` (created at first startup, mode 0600).
- Objects: `GG_DATA_DIR/objects/objects/sha256/aa/bb/<hex>`; temporary files under `GG_DATA_DIR/objects`.
- Collective coordinator (P4): internal loop every 2 s over open rounds (`FOR UPDATE SKIP
  LOCKED`, `(run, step)` key on checkpoints); several instances can run without double
  aggregation. Checkpoints are project artifacts (`GG_DATA_DIR/objects`), never deleted.
- Quotas: `quota_requests_per_minute`/`quota_burst` per actor; worker/validator sessions get
  `quota_compute_multiplier` × that budget. A worker or validator request covered by one of its
  live leases (its active attempt's inputs, heartbeats, outputs and result, or the artifacts of a
  verification it holds) consumes nothing: the lease already bounds it. Claims, registrations and
  requests outside a lease are charged, and nothing is admitted once the bucket is empty.
- Peer-to-peer (ADR-0012): `p2p_enabled` turns on the tracker (signed manifests, announcements,
  tickets); `p2p_chunk_bytes` sets the verification granularity; `p2p_peer_ttl_secs` the expiry of
  announcements (cleaned up by the reaper); `p2p_allow_private_addrs` must stay `false` in public
  mode, otherwise a worker could be steered to a private address. Turning off `p2p_enabled` changes
  nothing for HTTP downloads: the control plane remains the durable source.

## Researcher sign-in

- ORCID (`[orcid]`, ADR-0010) and/or a single-use e-mail link (`[email_login]`, ADR-0028). Either way
  a new researcher is `pending` until an operator admits them: `gg admin researcher-list` shows the
  ORCID iD or the verified address.
- `public_url` is the address researchers use (behind the HTTPS proxy); links and the CLI command
  shown with a new token are built from it. It must be https when `public = true`.
- E-mail sign-in needs an SMTP relay: `smtp_host`, `smtp_port`, `smtp_tls` (`starttls` or `tls`;
  `none` only to a relay on this machine), `smtp_username`, and the password in the environment
  variable named by `smtp_password_env` (`GG_SMTP_PASSWORD` by default). `allowed_domains` restricts
  addresses (subdomains included); `max_links_per_hour` bounds the links sent to one address.

## Data rule and audit rate

- `data_policy` (default `public-synthetic-only`; `research-any-data` for a research deployment
  that accepts any data with a caution about confidentiality, docs/DECISIONS.md D4) is published
  in plain text by `GET /v1/info` and must be explicitly accepted when a project is created
  (`gg admin project-create --ack-data-policy`). Without acceptance, no project exists.
- `project_initial_grant` (default 0): credits granted to every new project. A deployment that
  does not bill researchers sets a large amount; tasks still reserve their budget before they
  start, and the ledger records who computed what.
- `audit_rate` is 0.25 at startup and can be changed **without redeploying**:
  `gg admin audit-rate --set 0.5 --reason "..."`. The change is recorded in the audit log, and
  `gg admin audit-rate` shows the value in effect and the startup value.

## Releases and incidents

Signed release of executables and revocation policy: docs/RELEASE.md. The key does not live on the
build machine: `cargo xtask release` produces an **unsigned** manifest, and `gg release sign` signs
it on an offline machine (decision D3). Incident procedures (compromised key, complacent validator,
restore, settlement to reconcile): docs/INCIDENTS.md. Dependency and license inventory:
`cargo xtask licenses`.

## Federated validation (P6)

- `gg admin operator-create --name op-b` then `gg admin validator-add --key <pubkey> --operator op-b`.
  Without `--operator`, a validator is attached to the `unassigned` operator.
- `audit_rate` (0 to 1, 0.25 by default) sets the share of submissions re-verified by another
  operator. No audit is opened as long as only one operator can validate: a deployment with a
  single validator behaves exactly as before.
- `gg admin dispute-list` shows pending disagreements, with both signed opinions and their
  operators; `gg admin dispute-resolve <id> --outcome rejected --reason "..." --yes` settles them.
  Settlement is the only thing that moves credits after a disagreement, and it is recorded in the
  audit log. A `disputed` task keeps its budget reservation until it is settled.
- The audit draw secret lives in `GG_DATA_DIR/audit.secret` (0600): backing it up with the database
  is not necessary (losing it only changes future draws), but disclosing it would make audits
  predictable.

## Demonstration

`cargo xtask demo [--kind mlp|matmul|collective] [--faults] [--gpu] [--jobs N] [--workers N] [--dim D]
[--rounds R]` builds the binaries, creates a `gg_demo_<timestamp>` database in the container,
starts the server/validator/workers in `var/demo/run-<timestamp>/` (logs, keys, bundles), submits
the jobs, waits (with a bound), exports `receipts.json`, verifies the receipts offline
(`gg receipt verify`) and writes `report.json` with the checks. The processes are stopped at the
end; the data remains. Cleaning up demo databases:
`docker exec gpu-goblin-postgres psql -U gg -d gpu_goblin -c "DROP DATABASE gg_demo_<ts>"`.

## Worker

- Artifact cache: `work_dir/cache/<sha256>` (`cache_max_bytes`, 0 to disable; LRU). Every hit is
  re-hashed before use; a corrupted entry is deleted; the entries of an attempt in progress are
  pinned and never evicted. The validator has the same cache for its inputs.
- Encryption: exchanges between peers use TLS 1.3 with a pinned raw public key (ADR-0013). Nothing
  to configure: the transport key is generated at startup and its fingerprint is announced. A
  restart changes the fingerprint; announcements expire and are made again.
- Behind a NAT: `gg-worker nat-probe [--map <port>]` reports what the gateway offers (NAT-PMP,
  UPnP-IGD, external address) and tests an ephemeral port mapping that it removes immediately. The
  worker tries NAT-PMP (`nat_pmp`) then UPnP-IGD (`upnp`), checks the mapping in the router's
  table, renews it at half-lease and **removes it when it stops sharing**; otherwise it announces
  the address the control plane observes. The server dials the address back (`p2p_dial_back`)
  before advertising it: if the log says "announced address is not reachable", open a port and set
  `advertise_addr`, or leave the worker as a reader only.
- Peer-to-peer sharing: `[p2p]` section (disabled by default). `enabled` allows fetching from
  peers, `share` allows serving chunks, bounded by `max_upload_conns` and `upload_budget_bytes`;
  `listen_addr`/`advertise_addr` decide what is announced. Enabling sharing makes the contributor's
  `ip:port` visible to the workers authorized on the same artifact. The logs report
  `peer_chunks` / `http_chunks` per attempt and a summary at shutdown.
- Downloads: retry on `429`/`5xx`/disconnection (6 attempts, backoff doubled up to 10 s).
  Worker/validator sessions get `quota_compute_multiplier` × the base quota on the server side,
  and requests covered by their leases are not counted.
- Updates: after a release that fixes a correctness or security problem, raise
  `min_worker_version` (`GG_MIN_WORKER_VERSION`). A worker below it gets no task (`426`); it stops
  and logs that it must be updated, instead of retrying.
- Run `gg-worker doctor` before any start: it measures the sandbox (namespaces, Landlock, seccomp,
  cgroups, GPU under restriction); `public = true` requires everything to be OK.
- Local resume: if the executor dies leaving `outputs/checkpoint` behind, the agent relaunches it
  once on the same bundle as long as the lease is valid (the log shows `resumed_from_step`).

## Monitoring

- `GET /v1/admin/metrics` (admin token): Prometheus text format, bounded labels (route, method,
  status class, task state, decision outcome).
- Quotas: `quota_requests_per_minute` / `quota_burst` per actor; exceeding them → `429` +
  `Retry-After`.

`tracing` logs (JSON available). Task states at a glance: `GET /v1/admin/tasks/summary`; the health
check (`deploy/server/gg-healthcheck.sh`) alerts on tasks that stop moving.

## Documented unmet prerequisites

- ORCID Sandbox: no credentials → provider flow "not tested".
- GPU CI runner: none; GPU trials are run manually on the hardware identified in docs/STATUS.md.
