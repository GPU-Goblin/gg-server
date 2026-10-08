# ADR-0043 — The contributor's desktop application

Status: **accepted** (2026-09-26). Decided with the owner: a native desktop application, for Linux
and for Windows natively; pause and resume, lending only when the machine is idle (or at chosen
hours), limits on the GPU (temperature, memory, energy) and the rest, and a dashboard.

## Context

Lending a machine today means a command-line installer (`deploy/worker/install.sh`, signatures
checked with `ssh-keygen`), a user service (`gg-worker`) configured by editing `worker.toml`, and a
page of the web portal listing the account's machines. Linux only: the executor's sandbox is
bubblewrap, Landlock and seccomp (ADR-0009). Someone who lends a gaming PC wants to stop lending in
one click when they play, to lend only while away, to keep the card cool, and to see what it did.

## Decision

Three parts, in this order.

1. **The worker takes orders from its owner** (`gg-worker`, every platform).
   - A **policy** (`policy.toml`, next to `worker.toml`), read again whenever it changes:
     - `paused`;
     - `only_when_idle` (the session locked, or idle for `idle_after_secs`);
     - `schedule`: windows of days and hours; outside them, nothing is claimed;
     - `gpu_max_temp_c`: above it, nothing new is claimed until the card cools down;
     - `yield_to_other_gpu_apps`: while another program computes on the card (a game, a render),
       nothing new is claimed;
     - memory limits (`max_ram_bytes`, `device_memory_bytes`), taken at the next claim.
   - A task already running finishes, unless the owner asks for an immediate pause: the attempt is
     then **abandoned properly** (the server gives the task to another worker at once, no sanction).
   - A **local control API**: HTTP on `127.0.0.1` only, with a bearer token in a file only the user
     can read (`control.token`, mode 0600). `GET /v1/status` (state and why, the task running, what
     the machine did, credits earned), `POST /v1/pause {now}`, `POST /v1/resume`, `GET`/`PUT
     /v1/policy`.
   - The sensors are read without privileges: on Linux, `loginctl` for the session's lock and idle
     state, `nvidia-smi` (NVIDIA) or the hwmon sysfs files (AMD) for temperature and the other
     processes on the card. Capping the card's power needs administrator rights: it is not done
     by the worker; the temperature guard is.
2. **`gg-desk`, the application** (Rust, egui; a tray icon). It talks only to the local control
   API: dashboard, pause and resume, settings, and on first launch the enrollment code. It installs
   and updates the worker through the signed release, as `install.sh` does (the same checks). A
   pure-Rust interface: no web view to ship or patch, the same code on Linux and Windows.
3. **The worker on Windows, natively.** The executor needs a sandbox of its own there: an
   AppContainer without network capability, a Job Object bounding memory and processes and killing
   everything with the job, file access limited to the task's directory. Its design is written and
   reviewed in its own ADR before the code, and it is tested on a Windows machine: a sandbox that
   has not run on Windows is not announced as working.

## Consequences

- The owner's choices are local and win: the server never learns more than what the claims already
  say (a worker that does not claim is simply not working).
- A task abandoned on an immediate pause costs the grid one reassignment, never the contributor a
  sanction.
- Windows doubles the surface to secure; until part 3 is done and tested, Windows contributors
  cannot lend.

## Plan and status

- Part 1: *done on Linux (2026-09-26)*. `gg_worker::control`, the policy gate before every claim,
  an immediate pause that abandons the running task, the control API (`control_listen`,
  `127.0.0.1:7460` by default; the address actually bound is written to `control.addr` next to
  `control.token`; a second worker of the machine runs without it), `accepted` in `/worker/me`.
  The GPU conditions apply only to a worker computing on the card; the card counts as used by
  another program above 50 % (an idle desktop measured 17 %). Tests: `gg-worker` `control` unit
  tests, `tests/control.rs` (the API), `tests/policy.rs` (the real agent asks for nothing while
  paused); by hand, on a local grid: an immediate pause abandoned the running 2048³ product, the
  worker waited, resumed, and the 5 tasks were accepted.
- Part 2: *first version on Linux (2026-09-26)*: `gg-desk` (crate `crates/gg-desk`, egui 0.33 on
  glow, tray over D-Bus with ksni: no GTK, no web view). Overview (state and why, running task,
  credits, accepted computations, card temperature and use, session), pause, pause now, resume;
  settings (only when away and after how long, hour windows by day through midnight, giving way
  to games, temperature limit, memory lent), checked like the worker checks them; setup (the
  enrollment code, checked for its shape, and the signed `install.sh` run as it is, its output
  shown). Closing the window keeps it in the tray; without a tray host, closing quits. The
  installer installs it when the release carries it, with a menu entry and a session autostart
  (`--no-desktop` to skip); `gg release` publishes it. Tests: `gg-desk` unit tests, `tests/link.rs`
  (the link against the worker's real control API: files found, pause, policy written, a refusal
  shown, resume); pictures taken with the hidden `--screenshot` option against a worker of the
  local grid, computing and then waiting because the session was in use.
  *Second look (2026-09-26, the owner asked for the level of Twitch, Discord or Waze)*: egui 0.36;
  a side bar with the mark (a goblin's head as a chip, drawn by the program: window, window icon,
  tray icon with the state's dot) and the name; cards, pill buttons, switches, day chips, a
  turning ring and a running bar while computing, live statistics; a dark and a light look, or the
  system's, remembered (`~/.config/gpu-goblin/desk.toml`); the Inter typeface (SIL OFL, shipped
  with its licence) and Phosphor icons in a family of their own (each font draws glyphs the other
  needs). Always in the system tray: started with the session (`--tray`) it lives there until
  opened; closing the window hides it; the tray's tooltip and menu give the state. On Linux the
  window runs on X11 (XWayland under Wayland): Wayland cannot hide a window and show it again.
  Checked on KDE Plasma (Wayland): the item registers with the StatusNotifierWatcher, its tooltip
  gives the state, the window starts unmapped and is mapped by the tray's activation.
  *Credits, servers, several cards (2026-09-26, asked by the owner)*:
  - An "About" page: the mark, the version, "designed and developed by Nicolas Michel — LAAS-CNRS"
    (the author's laboratory) with a link to his LinkedIn profile, the contact address `contact@gpugoblin.com` and the site, thanks to
    the people who lend and use the grid and to the open-source work it is built on (each with what
    it does for the grid), the licence.
  - A console mode, the same binary: `gg-desk status [--json]`, `pause [--now]`, `resume`, `set`
    (the policy's every field, schedules written `mon-fri 08:00-18:00`, checked as the worker checks
    them) and `console`, a live view answering keys (p, s, r, q) that restores the terminal on any
    exit. Without a display (`DISPLAY` and `WAYLAND_DISPLAY` empty) `gg-desk` alone opens the live
    view, or prints the status when its output is not a terminal. Exit codes: 0 done, 1 failed or
    refused, 2 no worker answers. The binary links only the C library (X11 and Wayland are opened
    at run time), so it runs on a server as it is. On a server, "only when away" counts the
    machine as idle (no session: nobody to yield to).
  - Several graphics cards: one worker per card, all attached to the same account by the same
    enrollment code (it is reusable). The first keeps the usual places (`worker.toml`, the data
    directory, port 7460); card k has `worker-gpuk.toml`, `gpuk/` under the data directory (key,
    policy, work, control files), `engine = "gpu:k"` or `"cuda:k"`, the control API on a free
    loopback port, and the service `gg-worker@k` (a template unit derived from the signed
    `gg-worker.service`). `install.sh --gpus all|N` writes them (all: `nvidia-smi -L`, else the
    AMD and NVIDIA cards in `/sys/class/drm`), runs `init` and `doctor` for each, sizes the CUDA
    memory of each card. The application finds the workers in the data directory and its `gpuN`
    directories: one state for the machine (the busiest worker's, counters added up, paused only
    when all are), a card per graphics card with its state, temperature, use and its own pause;
    orders and settings go to all of them (or `--gpu k` in the console). The sandbox numbers CUDA
    cards in bus order (`CUDA_DEVICE_ORDER=PCI_BUS_ID`), as `nvidia-smi` does, so that `cuda:k`
    computes on the card whose temperature worker k watches.
  - Tests: `gg-desk/tests/link.rs` drives two real control APIs (base and `gpu1`): labels, one
    card paused while the machine lends, both paused, then the console binary without a display
    (`resume --gpu 0`, `status --json`, `set` written by both workers, an absent card and an
    impossible temperature refused with nothing changed, exit code 2 with no worker); unit tests
    for schedules and `set`. The installer ran end to end on a release signed by a throwaway key
    with stand-in binaries (`--gpu --gpus 3 --no-start`, no desktop): three configurations with
    their engines, ports, data directories and names, `init` and `doctor` for each, the template
    unit, `gg-desk` installed for the terminal only; the real `gg-worker` loads the generated
    configuration of card 1.
  *Account and languages (2026-09-26, asked by the owner)*:
  - At start, when nobody is signed in, a welcome screen: sign in or
    create an account in one form (an address, then the 8-digit code sent to it; no password),
    through the JSON contributor API (ADR-0030 addendum). The session is kept in
    `~/.config/gpu-goblin/account.toml` (0600); the side bar shows who is signed in and signs out
    (on the server too). Signed in, "Install and start" asks the grid for the enrollment code
    itself and runs the installer with it (and `--server-url` for another grid); typing a code
    stays possible. The application is used signed in (the owner's choice): nobody signed in, it
    opens on the welcome screen, and signing out goes back there.
  - Fifteen languages, chosen in the side bar or on the welcome screen, remembered, the system's
    by default: English, Chinese, Hindi, Spanish, French, Bengali, Portuguese, Russian,
    Indonesian, German, Japanese, Turkish, Vietnamese, Korean, Italian. Every text goes through
    `i18n::tr`, keyed by its English wording; each language is a file `i18n/<code>.toml`.
    Tests: every text of the code is translated in every language and nothing else, the
    placeholders match, and every character of every translation has a glyph in the fonts
    shipped. Fonts: Noto Sans Devanagari and Bengali (OFL), and the Noto Sans CJK faces cut down
    to the characters used (`assets/fonts/subset_cjk.py`, about 100–200 KB each), the face of the
    language spoken first. egui 0.36 shapes complex scripts (harfrust): Devanagari conjuncts
    checked on screen. Arabic and Urdu are left out: egui lays text out left to right only (no
    bidirectional text), so a right-to-left interface would read wrongly. The translations were
    written by machine and need a reading by native speakers; the console stays in English (logs,
    scripts).
  *Not done*: no Windows build; the installer has not run against a
  real release with its services started (no release rehearsed since); on a Vulkan machine mixing
  makes of cards, the adapter order may differ from `nvidia-smi`'s, so a worker could watch the
  temperature of another card than the one it computes on (the computation itself is unaffected);
  going from N cards to fewer leaves the extra services to disable by hand.
- Part 3: not started; needs a Windows machine to test on.
