# Incident procedures

Each procedure only uses commands that exist today. When an action is not tooled, this is stated
as such rather than implied.

Audit log shared by all procedures: `gg admin audit` (who did what, and when).

## Compromised worker key or malicious worker

1. Revoke the authorization: `gg admin worker-authorize <worker_id> --authorized false`.
   Immediate effect: sessions revoked, no further assignments, and **this worker's peer-to-peer
   announcements are removed** (the tracker stops listing it as a source).
2. Check what it produced: `gg admin tasks-summary`, then `gg job task <task_id>` for the tasks
   concerned. Results already accepted were recomputed by a validator; those in progress become
   assignable again.
3. Credits already paid out are not taken back: the ledger is append-only. A correction goes
   through a compensating entry decided by the operator, not through deletion.

## Compromised validator key, or complacent validator

1. Investigate through the signed decisions: `gg job task <task_id>` exposes `verifications`
   (validator, operator, outcome, signed decision).
2. Increase the audit share (`audit_rate` in the server configuration, then restart) so that every
   submission is re-verified by **another operator** (ADR-0014).
3. Handle the disagreements: `gg admin dispute-list`, then
   `gg admin dispute-resolve <id> --outcome rejected --reason "..." --yes`.
4. Withdraw the validator: `gg admin validator-authorize <validator_id> --revoke --yes`. Its
   sessions end at once, the verifications it holds go back to the queue for another validator,
   and a new session it opens can neither claim nor decide. Its past decisions stay recorded and
   attributed. `gg admin validator-authorize <validator_id>` authorizes it again.

## Compromised server key

The server key (`GG_DATA_DIR/server.key`) signs assignments, receipts, chunk manifests and read
tickets. Its compromise is the most serious incident.

1. Stop the server. While it is stopped, no task is assigned and no receipt is issued.
2. Replace the key, then restart: receipts already issued remain verifiable with the old public
   key, which must therefore be kept and published as "old key, period …".
3. Workers and validators discover the new key through `/v1/info` at their next session.
4. **Not tooled**: there is no online rotation and no signature chain between the old and new key.
   This is a limitation to address before any public release.

## Suspicious or corrupted artifact

1. An artifact is addressed by its SHA-256: corrupted content cannot impersonate a hash. A forged
   chunk from a peer is rejected against the signed manifest, and the peer is dropped from the
   transfer.
2. If an artifact must be withdrawn (harmful content, data published by mistake):
   `gg admin artifact-withdraw <hash> --reason "..." --yes`. In one transaction, the collective
   runs and jobs that still need it are cancelled, their reservations released, and no worker is
   advertised as a holder any more; then its bytes are deleted. Its hash is never served nor
   accepted again, for every project that shared it. The command is idempotent: run it again if it
   was interrupted before the bytes were deleted.

## Database restore

1. Backup: `cargo xtask db backup --out <dir>` (`pg_dump -Fc` dump + copy of the object store).
2. Restore: `cargo xtask db restore --from <dir> --data-dir <dir> --yes`.
3. Post-restore check, tested by `gg-control/tests/restore.rs`: receipts remain verifiable offline,
   and the ledger remains balanced and append-only.

## Shutting down peer-to-peer sharing

- Server side: `p2p_enabled = false` shuts down the tracker (manifests, announcements, peers). HTTP
  downloads continue unchanged.
- Contributor side: `[p2p] enabled = false` or `share = false`. When sharing stops, the port
  mapping obtained through UPnP is removed.

## Settlement to reconcile

1. `gg admin batch-list` shows the state of each batch.
2. A batch whose root observed on chain differs from the committed root automatically moves to
   `to_reconcile` with the corresponding note.
3. After investigation: `gg admin batch-settle <batch_id> --event submitted --tx-hash <hash>` to
   resume, or leave it in reconciliation. A `finalized` batch is never reopened.

## Compromised release

See docs/RELEASE.md: revoke the release key, publish the revocation through the out-of-band
channel, and re-release with a new key. Contributors add `--revoked <key>` to their verification.
