# Runbook

One page for day-to-day operation of the beta server. Installation: [DEPLOY.md](DEPLOY.md).
Incidents (compromised keys, disputes, settlement): [INCIDENTS.md](INCIDENTS.md). Admin commands run
from the laptop with `GG_SERVER_URL=https://grid.gpugoblin.com` and an admin `GG_TOKEN`.

## Start, stop, logs

```bash
sudo systemctl start|stop|restart gg-server gg-verifier     # stop = SIGTERM, graceful (60 s at most)
systemctl status gg-server ; journalctl -u gg-server -f      # also: gg-verifier, caddy, postgresql@17-main
sudo systemctl stop gg-verifier gg-server                    # maintenance: validator first, then server
```

While gg-server is stopped nothing is assigned and no receipt is issued; workers retry by themselves
and leases that expire meanwhile are reclaimed at restart (the task goes back to the queue, never the
late result). A restart takes a few seconds.

## Workers

```bash
gg admin worker-list                                   # worker_id, display_name, public_key, authorized
gg admin worker-authorize <worker_id>                  # after receiving its public key through a trusted channel
gg admin worker-authorize <worker_id> --revoke --yes   # immediate: sessions ended, no assignment, P2P announcements removed
gg admin contributor-list                              # accounts that signed up: address, machines, accepted tasks
gg admin contributor-suspend <id> --reason "..." --yes # every machine of the account stops at once (ADR-0030)
gg admin contributor-suspend <id> --reinstate          # its machines work again
```

Results already accepted were recomputed by a validator; see INCIDENTS.md for a malicious worker.

## Validators and operators

```bash
gg admin operator-create --name <operator> --contact <email>
gg admin validator-add --key <validator public key> --name <name> --operator <operator>
gg admin validator-authorize <validator_id> --revoke --yes   # withdraw; its verifications go back to the queue
gg admin operator-list ; gg admin dispute-list ; gg admin audit-rate
```

A validator on another machine uses `server_url = "https://grid.gpugoblin.com"` and `public = true` in
its `verifier.toml`, and its own `gg-verifier.service`. Audits start once two operators validate.

## Researchers and projects

```bash
gg admin researcher-create --name "<name>"                                  # pending, token printed once
gg admin researcher-admission <id> --state approved|suspended|rejected --note "<why>"
gg admin project-create --name <p> --owner <researcher_id> --ack-data-policy --kind <kind> ...
gg admin project-grant <project_id> --amount <credits> ; gg admin project-list
gg admin artifact-withdraw <sha256> --reason "<why>" --yes   # data published by mistake (then purge the backups, below)
```

## Disk and retention

A training run's intermediate artifacts go once their round is settled (ADR-0040): workers' outputs,
micro-batch rows, superseded optimizer moments, and the weights between kept checkpoints (step 0,
every `checkpoint_keep_every` steps, the current one). Researchers' uploads never go. A pass runs
every `retention_interval_secs` and logs `retention pass rounds=… artifacts=… bytes=…`
(`journalctl -u gg-server | grep "retention pass"`). A round waiting for a dispute or an unfinished
verification keeps everything until it is settled. `retention = false` in server.toml keeps
everything: then plan about 2 GB of disk per round of a SmolLM2-sized run.

## Backups and restore

Daily at 03:15 (`gg-backup.timer`): `pg_dump -Fc` of the database, then the new objects (copied,
re-hashed), `server.key`, rotation (14 dumps), optional off-site rsync.

```bash
sudo systemctl start gg-backup.service && journalctl -u gg-backup -n 20
sudo -u gg /usr/local/lib/gpu-goblin/gg-backup.sh status     # last success, dumps, objects
sudo -u gg /usr/local/lib/gpu-goblin/gg-backup.sh verify     # re-hash every backed-up object (monthly)
sudo -u gg /usr/local/lib/gpu-goblin/gg-backup.sh purge <sha256>   # after artifact-withdraw; off-site copy by hand
```

**Restore** (same machine, or a new one prepared with DEPLOY.md steps 1 to 6, with
`/var/backups/gpu-goblin` copied back and an empty `gpu_goblin` database):

```bash
sudo systemctl stop gg-healthcheck.timer gg-backup.timer gg-verifier gg-server
sudo -u gg /usr/local/lib/gpu-goblin/gg-restore.sh --yes          # newest dump; or --dump <file>
sudo systemctl start gg-server gg-verifier gg-backup.timer gg-healthcheck.timer
gg admin tasks-summary                                            # the tasks are there
gg project receipts <project_id> --out r.json && gg receipt verify r.json --server-key <key> --network gg-beta-1
```

The restore runs in one transaction (all or nothing), puts back `server.key` if it is missing (it
refuses to replace a different one without `--replace-key`) and copies objects without deleting
newer ones. Anything done after the dump is lost: results submitted since then are recomputed by
workers, since their tasks come back to the queue. Rehearsed on 2026-09-24: dump of a live server,
restore into a fresh database and data directory, same server key, same row counts, the receipt
verified offline, the result downloadable.

## Publishing a release

Summary of RELEASE.md: tag gg-worker then gg-server `vX.Y.Z` → the draft appears with
`release.unsigned.json` → sign it offline (`gg release sign release.unsigned.json --key
/media/usb/release.key --out release.json`) → attach `release.json`, `SHA256SUMS`,
`SHA256SUMS.sshsig` → publish → copy to `/srv/gpu-goblin/releases/vX.Y.Z`, verify there, move the
`latest` link → tell contributors (they follow the Update section of INSTALL.md). The server's own
binaries: verify the same way, `sudo install -m 0755 gg-server gg-verifier gg /usr/local/bin/`,
then `sudo systemctl restart gg-server gg-verifier` (migrations run at start).

## Alerts

`journalctl -u gg-healthcheck -p err` (and the webhook, if configured). Each line is
`ALERT <name>: <detail>`.

| Alert | Meaning | First action |
|---|---|---|
| `service_down` | a unit is not active | `systemctl status <unit>`, `journalctl -u <unit> -n 50`; a unit that failed 10 times in 5 minutes stays down: fix, then `systemctl reset-failed <unit>` and start |
| `health` | gg-server does not answer on 127.0.0.1:8080, or has lost its database | `journalctl -u gg-server`; `pg_lsclusters`; `systemctl status postgresql@17-main` |
| `public_health` | the public URL fails while the local one works | Caddy (`journalctl -u caddy`), certificate, DNS, `ufw status` |
| `certificate` | the TLS certificate expires in less than 14 days | Caddy renews 30 days ahead: its log says why it cannot (port 80 closed, DNS moved) |
| `metrics` | the admin endpoints cannot be read | server down, or the monitor token revoked: recreate it (DEPLOY.md step 9) |
| `monitor_config` | no token for the health check | create `/etc/gpu-goblin/monitor.env` |
| `quota` | more than 200 requests refused with 429 in 5 minutes | `journalctl -u gg-server | grep 429`: a client in a loop, or a load the quotas do not fit (`quota_*` in server.toml, restart) |
| `validation_stalled` | results wait and no task was decided for 30 minutes | `systemctl status gg-verifier`; its journal; is the validator still authorized (`gg admin operator-list`)? |
| `tasks_stuck` | tasks queued, none assigned, for 30 minutes | no authorized worker online, or none accepts that kind / has the memory: `gg admin worker-list`, ask contributors |
| `disputes` | two validators disagree on a result | human decision: `gg admin dispute-list`, then `dispute-resolve` (INCIDENTS.md) |
| `outbox` | more than 1000 outbox events pending | `journalctl -u gg-server` for the outbox loop errors |
| `disk` | a filesystem (data, backups, PostgreSQL) is 85% full | objects are never deleted: grow the disk, move backups off-site, `gg-backup.sh status` |
| `backup` | no successful backup for 30 hours | `journalctl -u gg-backup`; run it by hand; `gg-backup.sh status` |
| `webhook` | the notification URL cannot be reached | check `WEBHOOK_URL` in monitor.conf |
