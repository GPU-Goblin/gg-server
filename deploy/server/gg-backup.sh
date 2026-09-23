#!/usr/bin/env bash
# GPU Goblin server backup: PostgreSQL dump (custom format) + content-addressed object store.
#
#   gg-backup.sh backup          daily run (gg-backup.timer): dump, objects, key, rotation, off-site
#   gg-backup.sh verify          re-hash every object of the backup against its name
#   gg-backup.sh purge <sha256>  remove one object from the backup (withdrawn artifact)
#   gg-backup.sh status          last success, dumps kept, object count
#
# Configuration: /etc/gpu-goblin/backup.conf (or GG_BACKUP_CONF), shell variables, see
# deploy/server/backup.conf. Runs as the gg user (peer authentication to PostgreSQL, read access to
# the data directory). Restore: gg-restore.sh, procedure in docs/RUNBOOK.md.
#
# Order matters: the database is dumped BEFORE the objects are copied. The server writes an object
# before the row that references it, so every object a dump references already exists when the copy
# starts; the reverse order could back up rows whose object is missing.
set -euo pipefail
umask 077

CONF="${GG_BACKUP_CONF:-/etc/gpu-goblin/backup.conf}"
if [[ -r "$CONF" ]]; then
	# shellcheck source=backup.conf
	source "$CONF"
fi

BACKUP_DIR="${BACKUP_DIR:-/var/backups/gpu-goblin}"
GG_DATA_DIR="${GG_DATA_DIR:-/var/lib/gpu-goblin/server}"
# libpq connection string or URI. Default: Unix socket, peer authentication, no password.
PG_CONNINFO="${PG_CONNINFO:-host=/var/run/postgresql dbname=gpu_goblin}"
# Client commands (a prefix such as "docker exec -i <container>" is allowed for rehearsals).
PG_DUMP="${PG_DUMP:-pg_dump}"
PG_RESTORE="${PG_RESTORE:-pg_restore}"
PSQL="${PSQL:-psql}"
KEEP_DUMPS="${KEEP_DUMPS:-14}"
# Off-machine copy, disabled while empty. Example: backup@backup.example.org:/srv/gpu-goblin
OFFSITE_RSYNC_TARGET="${OFFSITE_RSYNC_TARGET:-}"
OFFSITE_SSH_KEY="${OFFSITE_SSH_KEY:-}"
# server.key signs receipts: it goes off-site only if you say so (keep an offline copy instead).
OFFSITE_INCLUDE_KEY="${OFFSITE_INCLUDE_KEY:-no}"

log() { printf '%s gg-backup: %s\n' "$(date -u +%FT%TZ)" "$*"; }
die() {
	log "FAILED: $*"
	exit 1
}

read -r -a pg_dump_cmd <<<"$PG_DUMP"
read -r -a pg_restore_cmd <<<"$PG_RESTORE"
read -r -a psql_cmd <<<"$PSQL"

objects_src() {
	# data_dir/objects is the store root; objects/tmp holds uploads in progress and is never copied.
	printf '%s/objects/' "$GG_DATA_DIR"
}

cmd_backup() {
	command -v rsync >/dev/null || die "rsync is required (apt install rsync)"
	[[ -d "$GG_DATA_DIR/objects" ]] || die "$GG_DATA_DIR/objects does not exist (GG_DATA_DIR?)"
	mkdir -p "$BACKUP_DIR/db" "$BACKUP_DIR/objects" "$BACKUP_DIR/keys"
	exec 9>"$BACKUP_DIR/.lock"
	flock -n 9 || die "another backup is running"

	local stamp dump tmp bytes entries
	stamp="$(date -u +%Y%m%dT%H%M%SZ)"
	dump="$BACKUP_DIR/db/gpu_goblin-$stamp.dump"
	tmp="$dump.partial"

	# 1. Database, custom format, then read back its table of contents before keeping it.
	log "dumping the database"
	"${pg_dump_cmd[@]}" --format=custom --no-owner --no-privileges --dbname="$PG_CONNINFO" >"$tmp" ||
		{ rm -f "$tmp"; die "pg_dump failed"; }
	entries="$("${pg_restore_cmd[@]}" --list <"$tmp" | grep -cv '^;')" ||
		{ rm -f "$tmp"; die "the dump cannot be read back (pg_restore --list)"; }
	[[ "$entries" -gt 0 ]] || { rm -f "$tmp"; die "the dump is empty"; }
	mv "$tmp" "$dump"
	bytes="$(stat -c %s "$dump")"
	log "database: $dump ($bytes bytes, $entries entries)"

	# 2. Objects: immutable and named by their SHA-256, so the copy is incremental and never
	# overwrites (a corrupted source cannot damage a good copy). New files are re-hashed.
	log "copying new objects"
	local newlist
	newlist="$(mktemp)"
	trap 'rm -f "$newlist"' RETURN
	rsync -a --ignore-existing --exclude='/tmp/' --out-format='%n' \
		"$(objects_src)" "$BACKUP_DIR/objects/" >"$newlist" || die "rsync of the objects failed"
	local new=0 bad=0 f
	while IFS= read -r f; do
		[[ -f "$BACKUP_DIR/objects/$f" ]] || continue
		new=$((new + 1))
		if ! check_object "$BACKUP_DIR/objects/$f"; then
			bad=$((bad + 1))
			log "object $f does not match its name: removed from the backup"
			rm -f "$BACKUP_DIR/objects/$f"
		fi
	done <"$newlist"
	[[ "$bad" -eq 0 ]] || die "$bad corrupted object(s) in the source store: investigate $GG_DATA_DIR"
	log "objects: $new new, $(count_objects) in the backup"

	# 3. The server key (it never changes; a changed key is logged loudly).
	if [[ -f "$GG_DATA_DIR/server.key" ]]; then
		if [[ -f "$BACKUP_DIR/keys/server.key" ]] && ! cmp -s "$GG_DATA_DIR/server.key" "$BACKUP_DIR/keys/server.key"; then
			log "WARNING: server.key changed since the last backup; the previous one is kept as server.key.$stamp"
			mv "$BACKUP_DIR/keys/server.key" "$BACKUP_DIR/keys/server.key.$stamp"
		fi
		[[ -f "$BACKUP_DIR/keys/server.key" ]] || cp -p "$GG_DATA_DIR/server.key" "$BACKUP_DIR/keys/server.key"
	fi

	# 4. Rotation: keep the newest KEEP_DUMPS dumps. Objects are never rotated: an old dump may
	# reference any of them.
	local old
	while IFS= read -r old; do
		log "rotating out $old"
		rm -f "$old"
	done < <(find "$BACKUP_DIR/db" -maxdepth 1 -name 'gpu_goblin-*.dump' -printf '%f\n' |
		sort -r | tail -n +"$((KEEP_DUMPS + 1))" | sed "s#^#$BACKUP_DIR/db/#")

	# 4b. Objects the retention policy removed from the server (ADR-0040) leave the backup once every
	# dump kept was started after they expired: no dump kept can name them as live any more. Without
	# this the backup would keep every intermediate artifact of every training run.
	local oldest oldest_iso hex path gone=0
	oldest="$(find "$BACKUP_DIR/db" -maxdepth 1 -name 'gpu_goblin-*.dump' -printf '%f\n' | sort | head -n 1)"
	oldest="${oldest#gpu_goblin-}"
	oldest="${oldest%.dump}"
	if [[ "$oldest" =~ ^([0-9]{4})([0-9]{2})([0-9]{2})T([0-9]{2})([0-9]{2})([0-9]{2})Z$ ]]; then
		oldest_iso="${BASH_REMATCH[1]}-${BASH_REMATCH[2]}-${BASH_REMATCH[3]}T${BASH_REMATCH[4]}:${BASH_REMATCH[5]}:${BASH_REMATCH[6]}Z"
		while IFS= read -r hex; do
			[[ "$hex" =~ ^[0-9a-f]{64}$ ]] || continue
			# The store's root is data_dir/objects, whose objects live in objects/sha256/aa/bb.
			path="$BACKUP_DIR/objects/objects/sha256/${hex:0:2}/${hex:2:2}/$hex"
			if [[ -f "$path" ]]; then
				rm -f -- "$path"
				gone=$((gone + 1))
			fi
		done < <("${psql_cmd[@]}" --no-psqlrc -At --dbname="$PG_CONNINFO" \
			-c "SELECT encode(hash, 'hex') FROM artifacts WHERE expired_at < '$oldest_iso'::timestamptz" ||
			die "cannot list the expired artifacts")
		log "objects expired before the oldest dump kept ($oldest_iso): $gone removed from the backup"
	fi

	# 5. Off-machine copy (disabled by default). Additions only: the off-site copy keeps what 4b
	# removed here (propagating removals would let an emptied local backup empty it too).
	if [[ -n "$OFFSITE_RSYNC_TARGET" ]]; then
		offsite
	else
		log "off-site copy disabled (OFFSITE_RSYNC_TARGET is empty)"
	fi

	date -u +%s >"$BACKUP_DIR/LAST_SUCCESS"
	log "backup complete"
}

offsite() {
	local ssh=(ssh -o BatchMode=yes)
	[[ -n "$OFFSITE_SSH_KEY" ]] && ssh+=(-i "$OFFSITE_SSH_KEY" -o IdentitiesOnly=yes)
	log "off-site copy to $OFFSITE_RSYNC_TARGET"
	# Objects: additions only. Dumps: mirror, so the rotation applies there too.
	rsync -a --ignore-existing -e "${ssh[*]}" "$BACKUP_DIR/objects/" "$OFFSITE_RSYNC_TARGET/objects/" ||
		die "off-site copy of the objects failed"
	rsync -a --delete -e "${ssh[*]}" "$BACKUP_DIR/db/" "$OFFSITE_RSYNC_TARGET/db/" ||
		die "off-site copy of the dumps failed"
	if [[ "$OFFSITE_INCLUDE_KEY" == "yes" ]]; then
		rsync -a -e "${ssh[*]}" "$BACKUP_DIR/keys/" "$OFFSITE_RSYNC_TARGET/keys/" ||
			die "off-site copy of the key failed"
	fi
}

# An object's file name is its SHA-256 (objects/sha256/aa/bb/<hex>).
check_object() {
	local path="$1" name sum
	name="$(basename "$path")"
	[[ "$name" =~ ^[0-9a-f]{64}$ ]] || return 0
	sum="$(sha256sum "$path" | cut -d' ' -f1)"
	[[ "$sum" == "$name" ]]
}

count_objects() {
	find "$BACKUP_DIR/objects" -type f -regextype posix-extended -regex '.*/[0-9a-f]{64}' | wc -l
}

cmd_verify() {
	local total=0 bad=0 f
	while IFS= read -r -d '' f; do
		total=$((total + 1))
		if ! check_object "$f"; then
			bad=$((bad + 1))
			log "CORRUPTED: $f"
		fi
	done < <(find "$BACKUP_DIR/objects" -type f -print0)
	log "verified $total objects, $bad corrupted"
	[[ "$bad" -eq 0 ]]
}

cmd_purge() {
	local hash="${1:-}"
	[[ "$hash" =~ ^[0-9a-f]{64}$ ]] || die "usage: gg-backup.sh purge <sha256 hex>"
	local found
	found="$(find "$BACKUP_DIR/objects" -type f -name "$hash")"
	[[ -n "$found" ]] || die "object $hash is not in $BACKUP_DIR"
	rm -f -- "$found"
	log "removed $found from the local backup"
	[[ -z "$OFFSITE_RSYNC_TARGET" ]] || log "remove it from $OFFSITE_RSYNC_TARGET as well: it is not done here"
}

cmd_status() {
	if [[ -f "$BACKUP_DIR/LAST_SUCCESS" ]]; then
		local last
		last="$(cat "$BACKUP_DIR/LAST_SUCCESS")"
		echo "last success: $(date -u -d "@$last" +%FT%TZ) ($(($(date +%s) - last)) s ago)"
	else
		echo "last success: never"
	fi
	echo "dumps kept:"
	find "$BACKUP_DIR/db" -maxdepth 1 -name 'gpu_goblin-*.dump' -printf '  %f  %s bytes\n' 2>/dev/null | sort
	echo "objects: $(count_objects)"
}

case "${1:-}" in
backup) cmd_backup ;;
verify) cmd_verify ;;
purge) cmd_purge "${2:-}" ;;
status) cmd_status ;;
*)
	echo "usage: gg-backup.sh backup|verify|purge <sha256>|status" >&2
	exit 2
	;;
esac
