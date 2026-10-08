#!/usr/bin/env bash
# GPU Goblin lab node installer (Linux x86_64, no root).
#
#   curl -fsSLO https://grid.gpugoblin.com/releases/latest/install-node.sh
#   less install-node.sh                 # read it: it is short, and it is what you trust
#   bash install-node.sh [--release-url URL] [--allowed-signers FILE]
#
# What it does, in order, stopping at the first failure:
#   1. downloads SHA256SUMS and SHA256SUMS.sshsig from the release directory;
#   2. checks the signature with ssh-keygen against the release key written below (or the
#      --allowed-signers file you pass): nothing downloaded is run to verify anything;
#   3. downloads gg-node and its files, and checks them with sha256sum;
#   4. installs them under ~/.local/lib/gpu-goblin-node/<version>/ and points "current" at it;
#   5. writes ~/.config/gpu-goblin/node.toml from the signed template (never overwrites yours): the
#      node trusts the same release key for the signed list of servers;
#   6. creates the node's key and prints it: a researcher registers it on their account;
#   7. installs and starts the systemd user service gg-node (unless --no-start).
#
# The release key used the first time is pinned in ~/.config/gpu-goblin/allowed_signers; a later run
# with another key stops.
set -euo pipefail
umask 022

# --- What the operator sets before handing out this file ------------------------------------------
DEFAULT_RELEASE_URL="https://grid.gpugoblin.com/releases/latest"
DEFAULT_DIRECTORY_URL="https://gpugoblin.com/servers.json"
DEFAULT_NETWORK="gg-beta-1"
RELEASE_ALLOWED_SIGNERS='release@gpu-goblin namespaces="gg-release" REPLACE_WITH_THE_RELEASE_PUBLIC_KEY'
# ------------------------------------------------------------------------------------------------

PRINCIPAL="release@gpu-goblin"
NAMESPACE="gg-release"

release_url="$DEFAULT_RELEASE_URL"
directory_url="$DEFAULT_DIRECTORY_URL"
network="$DEFAULT_NETWORK"
allowed_signers_file=""
start=yes
prefix="$HOME/.local/lib/gpu-goblin-node"
config_dir="${XDG_CONFIG_HOME:-$HOME/.config}/gpu-goblin"
data_dir="${XDG_DATA_HOME:-$HOME/.local/share}/gpu-goblin-node"
bin_dir="$HOME/.local/bin"
unit_dir="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"

usage() {
	sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
	cat <<USAGE
Options:
  --release-url URL       directory holding the release (default $DEFAULT_RELEASE_URL)
  --directory-url URL     signed list of servers (default $DEFAULT_DIRECTORY_URL)
  --network ID            expected network of the release (default $DEFAULT_NETWORK)
  --allowed-signers FILE  OpenSSH allowed_signers file with the release key (instead of the one above)
  --no-start              install, but do not enable or start the service
USAGE
}

die() {
	printf '\nERROR: %s\nNothing was started.\n' "$*" >&2
	exit 1
}
step() { printf '\n==> %s\n' "$*"; }

while [[ $# -gt 0 ]]; do
	case "$1" in
	--release-url) release_url="${2:?}" && shift 2 ;;
	--directory-url) directory_url="${2:?}" && shift 2 ;;
	--network) network="${2:?}" && shift 2 ;;
	--allowed-signers) allowed_signers_file="${2:?}" && shift 2 ;;
	--no-start) start=no && shift ;;
	-h | --help) usage && exit 0 ;;
	*) die "unknown option $1 (see --help)" ;;
	esac
done
release_url="${release_url%/}"

only_https() {
	case "$2" in
	https://*) ;;
	http://127.0.0.1* | http://localhost* | http://\[::1\]*) echo "note: plain http is only accepted on this machine" ;;
	*) die "$1 must use https" ;;
	esac
}

step "Checking this machine"
[[ "$(uname -s)" == Linux && "$(uname -m)" == x86_64 ]] || die "Linux x86_64 only (this is $(uname -sm))"
[[ "$(id -u)" -ne 0 ]] || die "run this as the user who keeps the laboratory's data, not as root"
for c in curl ssh-keygen sha256sum systemctl; do
	command -v "$c" >/dev/null || die "$c is required (curl, openssh-client, coreutils, systemd)"
done
only_https --release-url "$release_url"
only_https --directory-url "$directory_url"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
fetch() {
	curl -fsSL --proto '=https,http' --retry 3 --max-time 600 -o "$work/$1" "$release_url/$1" ||
		die "cannot download $release_url/$1"
}

# --- 1-2. Signature of the checksum list, with OpenSSH ------------------------------------------
step "Verifying the release signature ($release_url)"
if [[ -n "$allowed_signers_file" ]]; then
	cp "$allowed_signers_file" "$work/allowed_signers"
else
	[[ "$RELEASE_ALLOWED_SIGNERS" != *REPLACE_WITH* ]] ||
		die "this copy of the installer carries no release key: ask the operator for it (--allowed-signers FILE)"
	printf '%s\n' "$RELEASE_ALLOWED_SIGNERS" >"$work/allowed_signers"
fi
pinned="$config_dir/allowed_signers"
if [[ -f "$pinned" && -z "$allowed_signers_file" ]] && ! cmp -s "$pinned" "$work/allowed_signers"; then
	die "the release key differs from the one pinned at installation ($pinned). If the operator announced a new key, check it out of band, then run again with --allowed-signers <file>"
fi
fetch SHA256SUMS
fetch SHA256SUMS.sshsig
ssh-keygen -Y verify -f "$work/allowed_signers" -I "$PRINCIPAL" -n "$NAMESPACE" \
	-s "$work/SHA256SUMS.sshsig" <"$work/SHA256SUMS" ||
	die "the signature of SHA256SUMS does not verify with the trusted release key: do not use this release"
release_network="$(sed -n 's/^# network: //p' "$work/SHA256SUMS")"
version="$(sed -n 's/^# version: //p' "$work/SHA256SUMS")"
[[ "$release_network" == "$network" ]] || die "this release is for network '$release_network', not '$network'"
[[ "$version" =~ ^[0-9A-Za-z._+-]{1,64}$ ]] || die "unexpected version string in SHA256SUMS"
echo "signed release $version for network $network"

# --- 3. Files, checked against the signed list ------------------------------------------------
needed=(gg-node node.toml.template gg-node.service)
: >"$work/needed.sums"
for f in "${needed[@]}"; do
	n="$(awk -v f="$f" '$2 == f' "$work/SHA256SUMS" | wc -l)"
	[[ "$n" -eq 1 ]] || die "$f is not listed exactly once in the signed SHA256SUMS"
	awk -v f="$f" '$2 == f' "$work/SHA256SUMS" >>"$work/needed.sums"
done
step "Downloading ${needed[*]}"
for f in "${needed[@]}"; do fetch "$f"; done
step "Checking SHA-256 digests"
(cd "$work" && sha256sum --strict -c needed.sums) || die "a downloaded file does not match the signed digest"

# --- 4. Install side by side, then switch -------------------------------------------------------
step "Installing version $version into $prefix/$version"
dest="$prefix/$version"
mkdir -p "$dest.partial"
install -m 0755 "$work/gg-node" "$dest.partial/gg-node"
install -m 0644 "$work/SHA256SUMS" "$work/SHA256SUMS.sshsig" "$dest.partial/"
rm -rf "$dest"
mv "$dest.partial" "$dest"
previous="$(readlink "$prefix/current" 2>/dev/null || true)"
ln -sfn "$version" "$prefix/current.new"
mv -T "$prefix/current.new" "$prefix/current"
mkdir -p "$bin_dir"
ln -sfn "$prefix/current/gg-node" "$bin_dir/gg-node"
if [[ -n "$previous" && "$previous" != "$version" ]]; then
	echo "previous version kept: $prefix/$previous"
fi

# --- 5. Configuration ------------------------------------------------------------------------------
config="$config_dir/node.toml"
mkdir -p "$config_dir" "$data_dir"
chmod 700 "$data_dir"
install -m 0644 "$work/allowed_signers" "$pinned"
# The key that signs the releases also signs the list of servers: `ssh-ed25519 AAAA...`.
release_key="$(awk '{for (i = 1; i < NF; i++) if ($i == "ssh-ed25519") {print $i " " $(i + 1); exit}}' "$work/allowed_signers")"
[[ "$release_key" =~ ^ssh-ed25519\ [A-Za-z0-9+/=]+$ ]] || die "no ssh-ed25519 key in the allowed signers"
if [[ -f "$config" ]]; then
	echo "keeping your existing $config"
else
	sed -e "s#@DATA_DIR@#$data_dir#" -e "s#@DIRECTORY_URL@#$directory_url#" \
		-e "s#@DIRECTORY_KEY@#$release_key#" \
		"$work/node.toml.template" >"$config.new"
	! grep -q '@[A-Z_]*@' "$config.new" || die "template markers left in $config.new"
	chmod 600 "$config.new"
	mv "$config.new" "$config"
	echo "wrote $config"
fi

# --- 6. Identity -------------------------------------------------------------------------------
step "Node identity"
node="$prefix/current/gg-node"
pubkey="$("$node" --config "$config" init)" || die "gg-node init failed"
echo "node public key: $pubkey"

# --- 7. Service -----------------------------------------------------------------------------------
mkdir -p "$unit_dir"
install -m 0644 "$work/gg-node.service" "$unit_dir/gg-node.service"
if [[ "$start" == yes ]]; then
	step "Starting the user service gg-node"
	systemctl --user daemon-reload || die "systemctl --user is not available in this session"
	systemctl --user enable gg-node.service >/dev/null
	systemctl --user restart gg-node.service
	sleep 3
	systemctl --user is-active --quiet gg-node.service ||
		die "gg-node did not stay up: journalctl --user -u gg-node -n 50"
	echo "running. Logs: journalctl --user -u gg-node -f"
	if [[ "$(loginctl show-user "$USER" -p Linger --value 2>/dev/null || echo no)" != yes ]]; then
		echo "note: it stops when you log out; to keep it running: loginctl enable-linger $USER"
	fi
else
	echo "service installed, not started (--no-start): systemctl --user enable --now gg-node"
fi

cat <<DONE

Done: GPU Goblin lab node $version installed.
Register it on your researcher account, then attach it to your projects:
  gg node register $pubkey --name "$(hostname -s 2>/dev/null || echo lab)"
  gg node attach <node-id> --project <project-id>
Other researchers of the laboratory use it once you allow them:
  gg node grant <node-id> <researcher-id>
The Python library reaches it at http://127.0.0.1:7450 (api_listen in $config).
Workers must reach it: forward the port of p2p_listen on your router, or use IPv6.
DONE
