#!/usr/bin/env bash
# Restore a GPU Goblin server from a gg-backup.sh directory. Procedure: docs/RUNBOOK.md, "Restore".
#
#   gg-restore.sh [--dump <file>] [--replace-key] --yes
#
# Same configuration file as gg-backup.sh (/etc/gpu-goblin/backup.conf). Run as the gg user, with
# gg-server STOPPED. The database named in PG_CONNINFO must exist (empty, or the one to overwrite):
# its content is replaced by the dump. Objects are copied back without deleting anything newer.
set -euo pipefail
umask 077

CONF="${GG_BACKUP_CONF:-/etc/gpu-goblin/backup.conf}"
if [[ -r "$CONF" ]]; then
	# shellcheck source=backup.conf
	source "$CONF"
fi
BACKUP_DIR="${BACKUP_DIR:-/var/backups/gpu-goblin}"
GG_DATA_DIR="${GG_DATA_DIR:-/var/lib/gpu-goblin/server}"
PG_CONNINFO="${PG_CONNINFO:-host=/var/run/postgresql dbname=gpu_goblin}"
PG_RESTORE="${PG_RESTORE:-pg_restore}"
read -r -a pg_restore_cmd <<<"$PG_RESTORE"

log() { printf '%s gg-restore: %s\n' "$(date -u +%FT%TZ)" "$*"; }
die() {
	log "FAILED: $*"
	exit 1
}

dump="" yes=no replace_key=no
while [[ $# -gt 0 ]]; do
	case "$1" in
	--dump)
		dump="${2:?--dump needs a file}"
		shift 2
		;;
	--yes)
		yes=yes
		shift
		;;
	--replace-key)
		replace_key=yes
		shift
		;;
	*) die "unknown argument $1 (usage: gg-restore.sh [--dump <file>] [--replace-key] --yes)" ;;
	esac
done

[[ "$yes" == yes ]] || die "restoring replaces the database content; pass --yes to confirm"
if command -v systemctl >/dev/null && systemctl is-active --quiet gg-server.service; then
	die "gg-server.service is running: systemctl stop gg-server gg-verifier first"
fi
command -v rsync >/dev/null || die "rsync is required (apt install rsync)"

if [[ -z "$dump" ]]; then
	dump="$(find "$BACKUP_DIR/db" -maxdepth 1 -name 'gpu_goblin-*.dump' | sort | tail -n 1)"
	[[ -n "$dump" ]] || die "no dump in $BACKUP_DIR/db"
fi
[[ -f "$dump" ]] || die "$dump does not exist"
log "dump: $dump"
"${pg_restore_cmd[@]}" --list <"$dump" >/dev/null || die "the dump cannot be read"

# 1. The server key first: restoring data under a different key would make the grid sign new
# receipts with a key nobody knows. An existing different key is only replaced on request.
if [[ -f "$BACKUP_DIR/keys/server.key" ]]; then
	if [[ ! -f "$GG_DATA_DIR/server.key" ]]; then
		mkdir -p "$GG_DATA_DIR"
		cp -p "$BACKUP_DIR/keys/server.key" "$GG_DATA_DIR/server.key"
		log "server.key restored"
	elif ! cmp -s "$BACKUP_DIR/keys/server.key" "$GG_DATA_DIR/server.key"; then
		[[ "$replace_key" == yes ]] ||
			die "$GG_DATA_DIR/server.key differs from the backed-up key; pass --replace-key to restore the old one"
		mv "$GG_DATA_DIR/server.key" "$GG_DATA_DIR/server.key.replaced-$(date -u +%Y%m%dT%H%M%SZ)"
		cp -p "$BACKUP_DIR/keys/server.key" "$GG_DATA_DIR/server.key"
		log "server.key replaced by the backed-up key (the previous one is kept next to it)"
	fi
else
	log "WARNING: no server.key in the backup; restore it from your offline copy before starting gg-server"
fi

# 2. Database, in one transaction: either the whole dump is in, or nothing changed.
log "restoring the database"
"${pg_restore_cmd[@]}" --clean --if-exists --no-owner --no-privileges --exit-on-error \
	--single-transaction --dbname="$PG_CONNINFO" <"$dump" || die "pg_restore failed (database unchanged)"

# 3. Objects: additions only. Objects newer than the dump are harmless (orphans). The backup copies
# objects after dumping, so every object the dump references is in the backup.
log "restoring objects"
mkdir -p "$GG_DATA_DIR/objects"
rsync -a --ignore-existing "$BACKUP_DIR/objects/" "$GG_DATA_DIR/objects/" || die "rsync of the objects failed"
log "restore complete: start gg-server, then check (docs/RUNBOOK.md): gg admin tasks-summary, a receipt export"
