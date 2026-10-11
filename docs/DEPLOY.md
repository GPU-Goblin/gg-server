# Deploying the private beta

From a blank dedicated server to the first authorized worker and the first validator. Every command
is meant to be typed as written, in order. The grid answers at `grid.gpugoblin.com` (see "Domain"). Day-to-day operations: [RUNBOOK.md](RUNBOOK.md). Releases:
[RELEASE.md](RELEASE.md). Contributors follow `deploy/worker/INSTALL.md`.

What runs on the server, and nothing else is exposed:

```
Internet ──443/80──> Caddy (HTTPS, Let's Encrypt) ──> 127.0.0.1:8080 gg-server ──Unix socket──> PostgreSQL 17
                                                                      ^
                                                   gg-verifier ───────┘ (127.0.0.1, its own user)
timers: gg-backup (daily), gg-healthcheck (every 5 minutes)
```

## Files in this repository and where they go

| Repository file | On the server | Owner, mode |
|---|---|---|
| `deploy/server/server.toml` | `/etc/gpu-goblin/server.toml` | root:gg 0640 |
| `deploy/server/verifier.toml` | `/etc/gpu-goblin/verifier.toml` | root:gg-verifier 0640 |
| `deploy/server/domain.env` | `/etc/gpu-goblin/domain.env` | root 0644 |
| `deploy/server/backup.conf` | `/etc/gpu-goblin/backup.conf` | root 0644 |
| `deploy/server/monitor.conf` | `/etc/gpu-goblin/monitor.conf` | root 0644 |
| `deploy/server/monitor.env.example` | `/etc/gpu-goblin/monitor.env` (with the token) | root 0600 |
| `deploy/server/gg-server.service`, `gg-verifier.service`, `gg-backup.{service,timer}`, `gg-healthcheck.{service,timer}` | `/etc/systemd/system/` | root 0644 |
| `deploy/server/Caddyfile` | `/etc/caddy/Caddyfile` | root 0644 |
| `deploy/server/caddy-gpu-goblin.conf` | `/etc/systemd/system/caddy.service.d/gpu-goblin.conf` | root 0644 |
| `deploy/server/nginx-gpu-goblin.conf` (instead of Caddy, on a server whose nginx already serves other sites) | `/etc/nginx/sites-available/gpu-goblin` | root 0644 |
| `deploy/server/gg-backup.sh`, `gg-restore.sh`, `gg-healthcheck.sh` | `/usr/local/lib/gpu-goblin/` | root 0755 |
| release binaries `gg`, `gg-server`, `gg-verifier` | `/usr/local/bin/` | root 0755 |

Data: `/var/lib/gpu-goblin/server` (objects, `server.key`, `audit.secret`; user gg),
`/var/lib/gg-verifier` (validator key and work; user gg-verifier), `/var/backups/gpu-goblin`
(user gg), `/srv/gpu-goblin/releases` (signed releases offered to contributors).

## Domain

The project's domain is `gpugoblin.com`:

| Name | Serves |
|---|---|
| `gpugoblin.com` (and `www.`) | the public site: presentation, accounts (sign-up, sign-in), downloads (repository gg-web) |
| `grid.gpugoblin.com` | the grid: API, signed releases (`/releases/`) — this server; its former `/portal` pages send to the site's account page |
| `noreply@gpugoblin.com` | sender of sign-in e-mails |

DNS: an `A` record (and `AAAA` if the server has IPv6) for `grid.gpugoblin.com` pointing to this
server, before Caddy starts, so that it can obtain the certificate.

The grid's host name appears in three kinds of places:

- **On the server, one file**: `/etc/gpu-goblin/domain.env` (`GG_DOMAIN`, `GG_PUBLIC_URL`, and the
  ACME contact `GG_ACME_EMAIL`). Caddy, the health check and gg-server read it.
- **In what contributors receive**: `DEFAULT_SERVER_URL` at the top of `deploy/worker/install.sh`
  (and hence every release).
- **In the documentation and examples.**

## Debian 12 or Ubuntu 24.04: what differs

| | Debian 12 (bookworm) | Ubuntu 24.04 (noble) |
|---|---|---|
| PostgreSQL in the distribution | 15: too old, use PGDG (below) | 16: too old, use PGDG (below) |
| Caddy | `apt install caddy` (2.6, main) | `apt install caddy` (2.6, universe) |
| Firewall | nftables present, `ufw` to install | `ufw` installed, inactive |
| glibc | 2.36 | 2.39 |
| unattended upgrades | `apt install unattended-upgrades` | installed |

The release binaries are built on Ubuntu 22.04 (glibc 2.35) and run on both. Nothing else in this
guide differs. (Ubuntu 24.04 also restricts unprivileged user namespaces through AppArmor; that
concerns bubblewrap on contributors' machines, not this server, which runs no sandbox.)

## Sizing the server

| | Minimum for the beta | Why |
|---|---|---|
| Memory | 16 GB | PostgreSQL, the validator, and the optimizer state kept between rounds (`step_cache_bytes`, 4 GiB by default: SmolLM2's weights and Adam moments take 1.6 GB, ADR-0036) |
| Disk | 200 GB, SSD | Researchers' datasets and checkpoints, the kept checkpoints (step 0, every 50 steps, the current one), about ten minutes of intermediate artifacts per run (2 GB per SmolLM2 round until retention removes them, ADR-0040: 20 to 100 GB for one such run), backups |
| CPU | 8 cores | The CPU validator recomputes a sample of GPU-validated model tasks (engine checks) and every task no GPU validator takes |

The server has no GPU. A GPU validator, if any, runs on another machine of a validation operator
(ADR-0031).

## 1. DNS and base system

1. At the registrar: an `A` record (and `AAAA` if the server has IPv6) `grid.gpugoblin.com` → the
   server's address. Check from your laptop: `dig +short grid.gpugoblin.com`.
2. On the server, as root, once:
   ```bash
   apt update && apt full-upgrade -y
   apt install -y curl jq rsync openssh-client ca-certificates unattended-upgrades
   adduser <you> && usermod -aG sudo <you>        # an admin account; copy your SSH key to it
   ```
   In `/etc/ssh/sshd_config`: `PasswordAuthentication no`, `PermitRootLogin no`, then
   `systemctl reload ssh`, **after** checking that your key logs you in.

From here on, commands run as your admin account with `sudo`.

## 2. Firewall: only 80 and 443 public, 22 for administration

```bash
sudo apt install -y ufw                    # already there on Ubuntu
sudo ufw default deny incoming
sudo ufw default allow outgoing            # gg-server dials back workers' announced addresses
sudo ufw allow 22/tcp                      # better: sudo ufw allow from <your IP or range> to any port 22 proto tcp
sudo ufw allow 80/tcp                      # ACME challenge and redirect to HTTPS
sudo ufw allow 443/tcp
sudo ufw allow 443/udp                     # HTTP/3 (optional)
sudo ufw enable
sudo ufw status verbose
```

PostgreSQL (5432) and gg-server (8080) listen on loopback only; check it later with
`sudo ss -ltnp` (nothing but 22, 80, 443 on `0.0.0.0`/`[::]`).

## 3. PostgreSQL 17 (PGDG repository)

```bash
sudo apt install -y postgresql-common
sudo /usr/share/postgresql-common/pgdg/apt.postgresql.org.sh -y   # adds apt.postgresql.org, signed
sudo apt install -y postgresql-17
pg_lsclusters                                                     # 17 main 5432 online
sudo -u postgres createuser gg                                    # role gg, no password
sudo -u postgres createdb -O gg gpu_goblin
```

Authentication is **peer** over the Unix socket (the default line `local all all peer` of
`pg_hba.conf`): the operating-system user `gg` is the database role `gg`, and no password exists
anywhere. `listen_addresses` stays at its default (`localhost`). The server connects with
`database_url = "postgres://gg@localhost/gpu_goblin?host=/var/run/postgresql"` (tested: sqlx takes
the socket directory from `host=`).

## 4. Users and directories

```bash
sudo adduser --system --group --home /var/lib/gpu-goblin gg
sudo adduser --system --group --home /var/lib/gg-verifier gg-verifier
sudo install -d -m 0755 /etc/gpu-goblin /usr/local/lib/gpu-goblin /srv/gpu-goblin/releases
sudo install -d -o gg -g gg -m 0700 /var/backups/gpu-goblin
sudo install -d -o gg -g gg -m 0750 /var/lib/gpu-goblin
sudo install -d -o gg-verifier -g gg-verifier -m 0700 /var/lib/gg-verifier
```

## 5. Binaries, verified before use

Take the release from the GitHub release page of gg-server (signed, see RELEASE.md) into a temporary
directory, with the release public key written into `/etc/gpu-goblin/allowed_signers`:

```bash
echo 'release@gpu-goblin namespaces="gg-release" ssh-ed25519 AAAA...' | sudo tee /etc/gpu-goblin/allowed_signers
d=$(mktemp -d) && cd "$d"
for f in SHA256SUMS SHA256SUMS.sshsig gg gg-server gg-verifier; do
  curl -fsSLO "https://github.com/GPU-Goblin/gg-server/releases/download/vX.Y.Z/$f"; done
ssh-keygen -Y verify -f /etc/gpu-goblin/allowed_signers -I release@gpu-goblin -n gg-release -s SHA256SUMS.sshsig < SHA256SUMS
grep -E '  (gg|gg-server|gg-verifier)$' SHA256SUMS | sha256sum --strict -c
sudo install -m 0755 gg gg-server gg-verifier /usr/local/bin/
```

## 6. Configuration

From a checkout of this repository copied to the server (or `scp deploy/server/* server:`):

```bash
cd deploy/server
sudo install -m 0640 -g gg server.toml /etc/gpu-goblin/server.toml
sudo install -m 0640 -g gg-verifier verifier.toml /etc/gpu-goblin/verifier.toml
sudo install -m 0644 domain.env backup.conf monitor.conf /etc/gpu-goblin/
sudo install -m 0755 gg-backup.sh gg-restore.sh gg-healthcheck.sh /usr/local/lib/gpu-goblin/
sudo install -m 0644 gg-server.service gg-verifier.service gg-backup.service gg-backup.timer \
  gg-healthcheck.service gg-healthcheck.timer /etc/systemd/system/
sudo systemctl daemon-reload
sudoedit /etc/gpu-goblin/domain.env        # GG_DOMAIN and GG_ACME_EMAIL
```

Read `server.toml` once: every value is explained in it. The ones to decide are `network_id` (chosen
once, bound into every signature: `gg-beta-1` unless you prefer another; releases must be built with
the same) and `data_policy`.

## 7. Start the control plane

```bash
sudo systemctl enable --now gg-server
systemctl status gg-server                     # active (running)
journalctl -u gg-server -n 20                  # "control plane identity ... server_key=..." then "listening"
curl -s http://127.0.0.1:8080/v1/health        # {"status":"ok","database":true,...}
sudo -u gg gg-server --config /etc/gpu-goblin/server.toml identity    # the server public key
```

Keep the server public key: receipts are verified against it (`gg receipt verify --server-key`).
**Copy `/var/lib/gpu-goblin/server/server.key` to offline storage now** (a USB stick kept apart): it
signs receipts, it never changes, and losing it is an incident (INCIDENTS.md). The daily backup
keeps a copy too, but not off-site by default.

## 8. HTTPS with Caddy

```bash
sudo apt install -y caddy
sudo install -m 0644 Caddyfile /etc/caddy/Caddyfile
sudo install -d /etc/systemd/system/caddy.service.d
sudo install -m 0644 caddy-gpu-goblin.conf /etc/systemd/system/caddy.service.d/gpu-goblin.conf
sudo systemctl daemon-reload && sudo systemctl restart caddy
journalctl -u caddy -n 30                      # "certificate obtained successfully" for your domain
curl -s https://grid.gpugoblin.com/v1/health     # from your laptop
```

If the certificate is not obtained: DNS not pointing here yet, or port 80 closed. Caddy retries by
itself. The upload limit of the Caddyfile (1025 MiB) matches `max_upload_bytes` (1 GiB) + 1 KiB;
change both together.

### Behind an existing nginx (a shared server)

On a server whose nginx already holds ports 80 and 443 for other sites, Caddy cannot bind them:
nginx proxies to gg-server instead, with a certificate from certbot. Skip the Caddy commands above.

```bash
sudo apt install -y certbot
sudo install -d /var/www/letsencrypt
sudo install -m 0644 nginx-gpu-goblin.conf /etc/nginx/sites-available/gpu-goblin
# First only the port-80 part is valid (the certificate does not exist yet): obtain it through it.
sudo sed -n '1,/^}/p' /etc/nginx/sites-available/gpu-goblin | sudo tee /etc/nginx/sites-available/gpu-goblin-acme >/dev/null
sudo ln -s ../sites-available/gpu-goblin-acme /etc/nginx/sites-enabled/gpu-goblin
sudo nginx -t && sudo systemctl reload nginx
sudo certbot certonly --webroot -w /var/www/letsencrypt -d grid.gpugoblin.com \
  --email admin@gpugoblin.com --agree-tos --no-eff-email \
  --deploy-hook "systemctl reload nginx"
# Then the whole site.
sudo ln -sfn ../sites-available/gpu-goblin /etc/nginx/sites-enabled/gpu-goblin
sudo rm /etc/nginx/sites-available/gpu-goblin-acme
sudo nginx -t && sudo systemctl reload nginx
curl -s https://grid.gpugoblin.com/v1/health          # from your laptop
```

certbot renews by itself (its systemd timer) and reloads nginx. The health check's certificate alert
still applies; where it says "Caddy", read "certbot" (`journalctl -u certbot`).

On a shared server, also:

- **Ports.** gg-server listens on `127.0.0.1:8080` (`bind_addr`): check it is free (`sudo ss -ltnp`),
  and if not, change `bind_addr` and `proxy_pass` together.
- **PostgreSQL.** If a PostgreSQL already runs, PGDG's 17 becomes a second cluster on port 5433: add
  `port=5433` to `database_url` (`?host=/var/run/postgresql&port=5433`) and to `PG_CONNINFO` in
  backup.conf.
- **Firewall.** Section 2 closes everything but 22, 80 and 443: do not apply it as written if other
  services need other ports; add the rules for 80 and 443 only.
- **Memory and disk.** The sizing above is for GPU Goblin alone: count what the other services use.
- **Isolation.** gg-server, gg-verifier and PostgreSQL run as their own users, and `server.key`
  (it signs receipts) is readable by `gg` only; a compromise of another service running as root
  would still reach it. Acceptable for a private beta; a public release wants a machine of its own.

## 9. The first admin token

```bash
sudo -u gg gg-server --config /etc/gpu-goblin/server.toml bootstrap-admin --label owner
```

The token is printed **once**; put it in your password manager. On your laptop (with the `gg`
binary of the release):

```bash
read -rs GG_TOKEN && export GG_TOKEN GG_SERVER_URL=https://grid.gpugoblin.com   # paste, Enter
gg admin audit                                   # the bootstrap event
```

(`gg auth login --server https://grid.gpugoblin.com --token <token>` saves them instead, mode 0600.)

A second admin token, only for the health check, stays on the server:

```bash
t=$(sudo -u gg gg-server --config /etc/gpu-goblin/server.toml bootstrap-admin --label monitor)
printf 'GG_MONITOR_TOKEN=%s\n' "$t" | sudo install -m 0600 /dev/stdin /etc/gpu-goblin/monitor.env; unset t
```

## 10. Operator and first validator

An operator is the administrative entity that runs validators; audits (`audit_rate = 0.25`) only
start once a second, distinct operator validates.

```bash
gg admin operator-create --name <your lab> --contact <email>
# on the server: create the validator key (printed) as its own user
sudo -u gg-verifier gg-verifier --config /etc/gpu-goblin/verifier.toml init
# on the laptop: authorize that key for the operator
gg admin validator-add --key <printed key> --name server-validator --operator <your lab>
# on the server
sudo systemctl enable --now gg-verifier
journalctl -u gg-verifier -n 5                   # "validator identity key=..."
```

Copy `/var/lib/gg-verifier/verifier.key` to the same offline storage as `server.key`.

## 11. Backups and monitoring

```bash
sudo systemctl enable --now gg-backup.timer gg-healthcheck.timer
sudo systemctl start gg-backup.service && journalctl -u gg-backup -n 20      # "backup complete"
sudo -u gg /usr/local/lib/gpu-goblin/gg-backup.sh status
sudo systemctl start gg-healthcheck.service && journalctl -u gg-healthcheck -n 30
```

The health check lists `ok ...` lines; anything at error priority (`journalctl -u gg-healthcheck
-p err`) is an alert, explained in RUNBOOK.md. To receive alerts on a phone, set `WEBHOOK_URL` in
`/etc/gpu-goblin/monitor.conf` (an ntfy topic, for example). The off-site copy of backups is off
until `OFFSITE_RSYNC_TARGET` is set in `/etc/gpu-goblin/backup.conf`.

## 12. Releases for contributors

Put the signed release where install.sh looks for it (RELEASE.md, step 6):
`/srv/gpu-goblin/releases/vX.Y.Z/` and the `latest` link. Check from the laptop:
`curl -fsSI https://grid.gpugoblin.com/releases/latest/SHA256SUMS`.

## 13. Researchers: manual admission

Before admitting anyone, send them the data rule of this deployment, in these words or better:

> Any research data may be submitted, but it is **not confidential**: the volunteer machine that
> computes a task sees its inputs and outputs, and so does whoever controls that machine. Do not
> submit personal, medical, or otherwise protected data unless it is anonymized to the standard your
> ethics board requires for public release, nor data under a confidentiality agreement.

```bash
gg admin researcher-create --name "Ada Lovelace"                       # pending; prints id and token
gg admin researcher-admission <researcher_id> --state approved --note "checked: <how>"
gg admin project-create --name <project> --owner <researcher_id> --ack-data-policy \
  --kind tensor.matmul.v1 --kind mlp.train.v1 --kind mlp.grad.v1 --kind graph.grad.v1 --kind graph.stage.v1
gg admin project-grant <project_id> --amount 100
```

Hand the token over a private channel (never by plain e-mail); it is shown once. `--ack-data-policy`
records that the rule above was accepted. Refusal or suspension: `researcher-admission --state
rejected|suspended`. Researchers who signed in themselves (ORCID or e-mail) appear in
`gg admin researcher-list` as `pending` until you admit them.

## 14. The first worker

The contributor follows `deploy/worker/INSTALL.md` and sends you the worker public key printed at
the end. Then, on the laptop:

```bash
gg admin worker-list                               # find the row whose public_key is the one received
gg admin worker-authorize <worker_id>
gg admin worker-list                               # authorized: true
```

Authorize only keys that reached you through a channel you trust: an authorized worker receives
data. The contributor's log shows it claiming tasks within a few seconds.

## 15. Smoke test, end to end

It needs a worker that runs matrix products: a CPU or Vulkan worker (`install.sh` without
`--cuda`). A worker on the native CUDA engine claims model tasks only (`graph.grad.v1`,
`graph.stage.v1`); for it, train a model instead (`gg-python/examples/train_classifier.py`).

With the researcher's token (in a separate shell):

```bash
export GG_SERVER_URL=https://grid.gpugoblin.com; read -rs GG_TOKEN && export GG_TOKEN
gg data gen-matmul --m 64 --k 64 --n 64 --out-a a.ggt --out-b b.ggt
gg job submit --project <project_id> --input a=a.ggt --input b=b.ggt      # prints job_id
gg job status <job_id>                                                     # state: accepted, within seconds
gg project receipts <project_id> --out receipts.json
gg receipt verify receipts.json --server-key <server public key> --network gg-beta-1
```

`accepted` means: the worker computed it in its sandbox, the validator recomputed it and agreed,
and a signed receipt was issued. This exact sequence was rehearsed on 2026-09-24 with the release
binaries, the production `server.toml` values (`public = true`), the validator under the hardening
of `gg-verifier.service` and a worker installed by `install.sh`.

It was rehearsed again on 2026-09-25, end to end, on one machine (`gg-tools/rehearse_beta.sh`).

- **Release.** Built from the public repositories as gg-server's workflow builds it, CUDA executor
  included, signed offline with a throwaway key, verified with `gg release verify` and with
  `ssh-keygen` + `sha256sum`.
- **Server and validator.** Run with the production configuration files and `domain.env`.
- **Workers.** A CUDA worker and a CPU worker installed by `install.sh` and authorized from the keys
  they printed.
- **Researcher.** This smoke test, then twice `train_classifier.py` from the Python package: loss
  1.098 → 0.021, every result accepted, the rounds shared by both workers, retention acting.
- **Backup and restore.** `gg-backup.sh` pruned the expired objects, and `gg-restore.sh` restored into
  a new database. The restored server kept the same key, verified every receipt and served every
  run's current checkpoint.

**What the rehearsal changed.**

- A CUDA worker failed the matrix product it claimed; workers now claim only what their engine
  runs.
- The backup's pruning looked in the wrong directory; fixed.

**Not rehearsed:** Caddy and HTTPS (loopback HTTP instead), PostgreSQL through its Unix socket (TCP in
Docker instead), the systemd hardening.

## Health checklist

```bash
systemctl is-active gg-server gg-verifier caddy postgresql@17-main   # four "active"
curl -s https://grid.gpugoblin.com/v1/health                          # status ok, database true
curl -s https://grid.gpugoblin.com/v1/info | jq '{network_id, data_policy, dev_mode}'   # dev_mode false
gg admin tasks-summary ; gg admin worker-list ; gg admin operator-list
sudo -u gg /usr/local/lib/gpu-goblin/gg-backup.sh status          # last success < 30 h
journalctl -u gg-healthcheck -p err --since today                  # empty when all is well
sudo ss -ltnp                                                      # public: 22, 80, 443 only
```

## Known limits of this version

- **Client address behind Caddy.** gg-server reads the client address from `X-Forwarded-For`
  only for connections from `trusted_proxies` (Caddy on this machine: `127.0.0.1`, `::1`). Each
  anonymous client has its own quota, and a worker is told the address it is really seen at. Do not
  add other addresses to `trusted_proxies`.
- **Upload duration.** An artifact upload may take `upload_timeout_secs` (3600 s: 1 GiB accepts an
  uplink down to about 2.4 Mbit/s); other requests `request_timeout_secs` (300 s). Downloads stream
  and are not cut by these bounds.
- **Stop.** The binaries stop gracefully on SIGTERM (what systemd sends) as on Ctrl-C.
