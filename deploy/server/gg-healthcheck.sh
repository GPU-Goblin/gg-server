#!/usr/bin/env bash
# GPU Goblin health check, run every 5 minutes by gg-healthcheck.timer. Minimal on purpose: curl, jq,
# df, systemctl. Every problem is written to the journal at error priority
# (journalctl -u gg-healthcheck -p err); when WEBHOOK_URL is set, a message is also POSTed there
# (ntfy-compatible), at most once per REPEAT_HOURS for the same set of problems.
#
# What each alert means and what to do: docs/RUNBOOK.md, "Alerts".
#
# Configuration: /etc/gpu-goblin/monitor.conf (GG_MONITOR_CONF), the domain from
# /etc/gpu-goblin/domain.env, and GG_MONITOR_TOKEN (an admin token, never in a readable file: the
# unit loads it from /etc/gpu-goblin/monitor.env, mode 0600 root).
set -euo pipefail

CONF="${GG_MONITOR_CONF:-/etc/gpu-goblin/monitor.conf}"
DOMAIN_ENV="${GG_DOMAIN_ENV:-/etc/gpu-goblin/domain.env}"
# shellcheck source=monitor.conf
[[ -r "$CONF" ]] && source "$CONF"
# shellcheck source=domain.env
[[ -r "$DOMAIN_ENV" ]] && source "$DOMAIN_ENV"

LOCAL_URL="${LOCAL_URL:-http://127.0.0.1:8080}"
GG_DOMAIN="${GG_DOMAIN:-}"
STATE_DIR="${STATE_DIR:-/var/lib/gg-healthcheck}"
DATA_DIR="${DATA_DIR:-/var/lib/gpu-goblin}"
BACKUP_DIR="${BACKUP_DIR:-/var/backups/gpu-goblin}"
SERVICES="${SERVICES:-gg-server gg-verifier caddy postgresql@17-main}"
DISK_MAX_PERCENT="${DISK_MAX_PERCENT:-85}"
BACKUP_MAX_AGE_HOURS="${BACKUP_MAX_AGE_HOURS:-30}"
CERT_MIN_DAYS="${CERT_MIN_DAYS:-14}"
QUOTA_REJECTIONS_MAX="${QUOTA_REJECTIONS_MAX:-200}"
OUTBOX_MAX="${OUTBOX_MAX:-1000}"
STALL_CHECKS="${STALL_CHECKS:-6}"
WEBHOOK_URL="${WEBHOOK_URL:-}"
REPEAT_HOURS="${REPEAT_HOURS:-6}"
GG_MONITOR_TOKEN="${GG_MONITOR_TOKEN:-}"

mkdir -p "$STATE_DIR"
alerts=()
alert() {
	alerts+=("$1: $2")
	printf '<3>ALERT %s: %s\n' "$1" "$2"
}
ok() { printf '<6>ok %s\n' "$*"; }

# State kept between runs (counters, stall streaks): key=value lines.
state_file="$STATE_DIR/state"
declare -A prev=()
if [[ -r "$state_file" ]]; then
	while IFS='=' read -r k v; do
		[[ -n "$k" ]] && prev["$k"]="$v"
	done <"$state_file"
fi
declare -A next=()

get() { curl -fsS --max-time 10 "$@"; }

# 1. Services.
for s in $SERVICES; do
	if systemctl is-active --quiet "$s"; then ok "service $s active"; else alert service_down "$s is not active"; fi
done

# 2. Health, locally and through the public name (TLS, Caddy, DNS).
if body="$(get "$LOCAL_URL/v1/health")" && [[ "$(jq -r '.status == "ok" and .database' <<<"$body")" == true ]]; then
	ok "local health"
else
	alert health "$LOCAL_URL/v1/health does not answer ok with a database"
fi
if [[ -n "$GG_DOMAIN" ]]; then
	if get "https://$GG_DOMAIN/v1/health" >/dev/null; then ok "public health"; else alert public_health "https://$GG_DOMAIN/v1/health unreachable (Caddy, certificate, DNS or firewall)"; fi
	if command -v openssl >/dev/null; then
		end="$(openssl s_client -connect "$GG_DOMAIN:443" -servername "$GG_DOMAIN" </dev/null 2>/dev/null |
			openssl x509 -noout -enddate 2>/dev/null | cut -d= -f2 || true)"
		if [[ -n "$end" ]]; then
			days=$((($(date -d "$end" +%s) - $(date +%s)) / 86400))
			if ((days < CERT_MIN_DAYS)); then alert certificate "TLS certificate of $GG_DOMAIN expires in $days days (Caddy renews at 30: check its log)"; else ok "certificate valid $days more days"; fi
		fi
	fi
fi

# 3. Metrics and tasks (admin token).
if [[ -z "$GG_MONITOR_TOKEN" ]]; then
	alert monitor_config "GG_MONITOR_TOKEN is not set: metrics and tasks are not checked"
elif metrics="$(get -H "Authorization: Bearer $GG_MONITOR_TOKEN" "$LOCAL_URL/v1/admin/metrics")" &&
	summary="$(get -H "Authorization: Bearer $GG_MONITOR_TOKEN" "$LOCAL_URL/v1/admin/tasks/summary")"; then
	metric() { awk -v m="$1" '$1 == m { print $2; found = 1 } END { if (!found) print 0 }' <<<"$metrics"; }
	count() { jq -r --arg s "$1" '[.by_state[] | select(.[0] == $s) | .[1]] | add // 0' <<<"$summary"; }
	quota="$(metric gg_quota_rejections_total)"
	uptime="$(metric gg_uptime_seconds)"
	outbox="$(jq -r '.outbox_pending' <<<"$summary")"
	queued="$(count queued)"
	assigned="$(count assigned)"
	submitted="$(count submitted)"
	disputed="$(count disputed)"
	# Progress is read from the task states (the gg_claims_assigned_total and gg_decisions_total
	# counters are not incremented by this server version).
	decided=$(($(count accepted) + $(count rejected) + $(count inconclusive) + disputed))
	next[quota]="$quota" next[uptime]="$uptime" next[decided]="$decided"

	# The quota counter restarts with the server: compare only within one server lifetime.
	if [[ -n "${prev[uptime]:-}" && "$uptime" -ge "${prev[uptime]}" ]]; then
		delta=$((quota - ${prev[quota]:-0}))
		if ((delta > QUOTA_REJECTIONS_MAX)); then alert quota "$delta requests refused with 429 since the last check (a client in a loop, or quotas too low)"; else ok "quota rejections +$delta"; fi
	fi
	# Validation stalled: results wait and no task reached a decision for STALL_CHECKS checks.
	streak=0
	if ((submitted > 0 && decided == ${prev[decided]:--1})); then streak=$((${prev[verify_streak]:-0} + 1)); fi
	next[verify_streak]="$streak"
	if ((streak >= STALL_CHECKS)); then alert validation_stalled "$submitted results wait for a validator and none was decided for $streak checks"; fi
	# Work stalled: tasks are queued and none is assigned, for STALL_CHECKS checks in a row.
	streak=0
	if ((queued > 0 && assigned == 0)); then streak=$((${prev[claim_streak]:-0} + 1)); fi
	next[claim_streak]="$streak"
	if ((streak >= STALL_CHECKS)); then alert tasks_stuck "$queued tasks queued and none assigned for $streak checks (no authorized worker online, or none fits)"; fi
	if ((disputed > 0)); then alert disputes "$disputed task(s) disputed between validators: a human decision is needed (gg admin dispute-list)"; fi
	if ((outbox > OUTBOX_MAX)); then alert outbox "$outbox outbox events pending"; fi
	ok "tasks queued=$queued assigned=$assigned submitted=$submitted decided=$decided disputed=$disputed outbox=$outbox"
else
	alert metrics "cannot read $LOCAL_URL/v1/admin/metrics or tasks/summary (token revoked? server down?)"
fi

# 4. Disk space.
for d in "$DATA_DIR" "$BACKUP_DIR" /var/lib/postgresql; do
	[[ -d "$d" ]] || continue
	used="$(df --output=pcent "$d" | tail -n 1 | tr -dc '0-9')"
	if ((used >= DISK_MAX_PERCENT)); then alert disk "$d is on a filesystem $used% full"; else ok "disk $d $used%"; fi
done

# 5. Backup age.
if [[ -r "$BACKUP_DIR/LAST_SUCCESS" ]]; then
	age=$((($(date +%s) - $(cat "$BACKUP_DIR/LAST_SUCCESS")) / 3600))
	if ((age >= BACKUP_MAX_AGE_HOURS)); then alert backup "last successful backup $age hours ago (journalctl -u gg-backup)"; else ok "backup $age h old"; fi
else
	alert backup "no successful backup recorded in $BACKUP_DIR/LAST_SUCCESS"
fi

for k in "${!next[@]}"; do printf '%s=%s\n' "$k" "${next[$k]}"; done >"$state_file.new"
mv "$state_file.new" "$state_file"

# 6. Notification, deduplicated: same problems => at most one message per REPEAT_HOURS.
if ((${#alerts[@]} == 0)); then
	rm -f "$STATE_DIR/notified"
	exit 0
fi
if [[ -n "$WEBHOOK_URL" ]]; then
	text="$(printf '%s\n' "${alerts[@]}")"
	digest="$(sha256sum <<<"$text" | cut -d' ' -f1)"
	last_digest="" last_time=0
	[[ -r "$STATE_DIR/notified" ]] && read -r last_digest last_time <"$STATE_DIR/notified"
	now="$(date +%s)"
	if [[ "$digest" != "$last_digest" ]] || ((now - last_time >= REPEAT_HOURS * 3600)); then
		if curl -fsS --max-time 10 -H "Title: GPU Goblin ${GG_DOMAIN:-server}: ${#alerts[@]} problem(s)" \
			-H "Priority: high" --data-binary "$text" "$WEBHOOK_URL" >/dev/null; then
			printf '%s %s\n' "$digest" "$now" >"$STATE_DIR/notified"
		else
			printf '<3>ALERT webhook: could not reach WEBHOOK_URL\n'
		fi
	fi
fi
exit 1
