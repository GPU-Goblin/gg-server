# Installing a lab node

A lab node (`gg-node`, ADR-0041) belongs to a laboratory. It:

- keeps the laboratory's datasets and trained weights on the laboratory's machine;
- serves them to the contributors' workers, and pulls their results;
- does the optimizer step of the laboratory's training runs;
- answers the Python library.

The control plane only schedules. One node serves every researcher of the laboratory whom its owner
allows.

## Install

On a Linux x86_64 machine that stays on, as the user who will keep the data:

```bash
curl -fsSLO https://grid.gpugoblin.com/releases/latest/install-node.sh
less install-node.sh
bash install-node.sh
```

The installer does the following:

1. It checks the release signature with `ssh-keygen` against the release key.
2. It installs `gg-node` under `~/.local/lib/gpu-goblin-node/`.
3. It writes `~/.config/gpu-goblin/node.toml`. There, the same release key is trusted to sign the
   list of servers at `https://gpugoblin.com/servers.json`.
4. It creates the node's key and prints it.
5. It starts the user service `gg-node`.

To keep the service running after you log out: `loginctl enable-linger $USER`.

## Register, share, attach

With the researcher token of the node's owner:

```bash
gg node register <public key printed by the installer> --name lab-box
gg node attach <node id> --project <project id>
gg node grant <node id> <researcher id>        # another researcher of the laboratory
```

A researcher you allowed attaches the node to their own projects with `gg node attach`. The node
works only for attached projects. `gg node detach` is refused while it still steps a run or pulls a
job's outputs there.

## The network

- **Workers** fetch datasets and checkpoints from the node, and it fetches their outputs. The port of
  `p2p_listen` (7451 by default) must be reachable from the Internet: forward it on the router, or use
  IPv6. The control plane checks that it answers before sending anyone there.
- **The local API** (`api_listen`) listens on the node's own machine by default. To let researchers
  reach it from their own computers on the laboratory's network, set `api_listen` to its address and
  give `api_tls_cert` and `api_tls_key` (PEM). The node refuses to listen beyond its machine without
  TLS, because researchers' tokens travel on it.

## Servers

`gg-node servers` prints the servers the node works with. They come from the signed list, or from a
`[[servers]]` table in `node.toml` for a private deployment. The node:

- refuses a list that is unsigned, signed by another key, expired, or older than one it already
  accepted;
- works only with a server that presents the key the list names;
- tries again every minute a server where its key is not registered yet.

## Watching it

- `journalctl --user -u gg-node -f` shows the log.
- `curl -H "Authorization: Bearer $GG_TOKEN" http://127.0.0.1:7450/v1/status` shows, for each server,
  the projects the node works for, what it holds, and what it is pulling.
