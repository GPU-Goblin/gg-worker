# Lending a machine to GPU Goblin: installation guide

This guide is for a contributor of the private beta: someone who lets the grid run computations on
their Linux computer. It takes about ten minutes. No administrator rights are needed, except to
install one package (bubblewrap) if it is missing.

## What you agree to, in plain words

- **Your machine computes other people's research tasks.** No researcher code runs on it: only the
  platform's own executor, inside a sandbox (bubblewrap namespaces without network, Landlock,
  seccomp, resource limits). The executor sees the data of the tasks it computes: that data comes
  from researchers, who were told it is not confidential with respect to contributors.
- **Peer-to-peer sharing is on.** To spare the server's bandwidth, your worker downloads task inputs
  from other contributors and serves those it already holds. While it is on, **your public IP
  address and a port are visible to the other workers authorized on the same data**, and your
  upload is used (at most 4 connections and 8 GiB per run, by default). Every piece received from a
  peer is checked against a list signed by the server before use. To turn it off: `[p2p]
  enabled = false` in `~/.config/gpu-goblin/worker.toml`, then `systemctl --user restart gg-worker`.
  Your router may be asked for a port mapping (NAT-PMP or UPnP); it is removed when the worker stops.
- **Limits are yours.** RAM, disk, CPU share and upload budget are set in `worker.toml`; the server
  can never raise them.

## Requirements

- Linux x86_64 with systemd (Debian 12, Ubuntu 22.04/24.04, Fedora 39+ are known to fit).
- `curl`, `ssh-keygen` (package `openssh-client`), `sha256sum` (coreutils), `bubblewrap`.
- For the GPU build: a Vulkan driver for your card (`vulkaninfo` lists it).
- About 300 MB for the programs, plus what you allow for tasks (20 GiB by default).

## Your account

Create your contributor account at <https://grid.gpugoblin.com/portal/contribute> (the site
<https://gpugoblin.com> links to it): enter your e-mail address, type the code you receive, then
**create an enrollment code** (`gge_…`). It is shown once; it attaches your machines to your
account, so keep it private. Your account page then lists your machines and the tasks they
computed.

## Install

The operator gives you, **through a channel other than the download** (in person, a signed message,
the lab's page), the fingerprint of the release key, for example `SHA256:vnM64SRz...`.

```bash
curl -fsSLO https://grid.gpugoblin.com/releases/latest/install.sh
grep '^RELEASE_ALLOWED_SIGNERS=' install.sh      # the release key written in the installer
ssh-keygen -lf <(grep -o 'ssh-ed25519 [A-Za-z0-9+/=]*' install.sh)   # must print the fingerprint you were given
bash install.sh --enroll gge_...   # your enrollment code; add --gpu for the GPU build
```

Several graphics cards: add `--gpus all` (or `--gpus 2`) to `--gpu` or `--cuda`. Each card gets
its own worker, attached to your account by the same code: the first as usual, card k with
`~/.config/gpu-goblin/worker-gpuk.toml`, `~/.local/share/gpu-goblin/gpuk/` and the service
`gg-worker@k`.

If the fingerprint differs, stop and tell the operator. The installer:

1. checks the signature of the release's checksum list with `ssh-keygen` (nothing downloaded is run
   to verify anything), then the network the release is meant for;
2. downloads only what it needs and checks every file with `sha256sum`;
3. installs into `~/.local/lib/gpu-goblin/<version>/` (and `~/.local/bin/gg-worker`), writes
   `~/.config/gpu-goblin/worker.toml` (never overwrites yours) and pins the release key in
   `~/.config/gpu-goblin/allowed_signers`;
4. creates your worker key (`~/.local/share/gpu-goblin/worker.key`) and runs `gg-worker doctor`,
   which must pass: otherwise it stops and says why;
5. starts the user service `gg-worker`.

On its first start the worker logs in with its own key and attaches itself to your account: it
appears on your account page within a minute and starts working. (Without `--enroll`, it prints its
public key and waits for an operator to authorize it.)

To keep the worker running when you are logged out: `loginctl enable-linger "$USER"`.

Every failure stops the installer with `ERROR:` and "Nothing was started". Run it again once the
cause is fixed; it is idempotent.

## The GPU Goblin application

When the release carries it, the installer also installs `gg-desk`: it appears in your menu as
"GPU Goblin" and starts with your session (an icon in the system tray). It shows what the machine
is doing and what it earned, pauses and resumes lending, and sets when and how much to lend (below).
Closing its window keeps the icon; "Quit" in the icon's menu stops the application, not the worker.
With several cards it shows each one, with its own pause. Opened on a machine without a worker,
it offers to sign in or create your account (your address, then the code sent to it) and installs
the worker attached to it, without copying any code. It speaks fifteen languages (side bar). `--no-desktop` skips the menu entry and
the start with the session; so does a machine without a desktop.

On a server, or over SSH, the same program works in the terminal:

```bash
gg-desk                       # without a display: a live view (p pause, s stop now, r resume, q quit)
gg-desk status                # what each worker does; --json for monitoring
gg-desk pause [--now] [--gpu 1]
gg-desk resume
gg-desk set --schedule "mon-fri 19:00-08:00" --schedule "sat,sun 00:00-24:00" --max-temp 80
gg-desk set --help            # every setting
```

Its exit code is 0 when done, 1 when refused or failed, 2 when no worker answers.

## When your machine lends

The worker obeys `~/.local/share/gpu-goblin/policy.toml` (next to its key), which the GPU Goblin
application edits for you; you may edit it by hand too, it is read again within seconds:

```toml
paused = false                 # true: lend nothing until you set it back
only_when_idle = true          # lend only while your session is locked, or idle for:
idle_after_secs = 300
yield_to_other_gpu_apps = true # a game or a render on the card: no new task
gpu_max_temp_c = 80            # the card hotter than this: no new task until it cools down
[[schedule]]                   # no window: at any time
days = ["mon", "tue", "wed", "thu", "fri"]
from = "19:00"
to = "08:00"                   # before `from`: through the night
```

A task already running finishes; the application's "pause now" abandons it instead, and the grid
gives it to another machine (no penalty). The worker listens on `127.0.0.1:7460` for the
application only, behind the token in `control.token` (readable by you alone); set
`control_listen = ""` in `worker.toml` to turn that off.

## Native CUDA engine (NVIDIA, optional)

NVIDIA cards can run model tasks on a native CUDA engine, several times faster than the Vulkan one on
convolutional networks (ADR-0033, ADR-0038):

```bash
bash install.sh --cuda --enroll <code>
```

On top of the steps above, `--cuda`:

1. checks that the NVIDIA driver is there (`/dev/nvidiactl`) and that `unzip` is installed;
2. installs the GPU build of the worker and the CUDA build of the executor (only the executor uses
   LibTorch, inside its sandbox);
3. downloads the official LibTorch archive from PyTorch (`libtorch-shared-with-deps-2.9.0+cu128`,
   3.9 GB, 6.0 GB unpacked; about 16 GB free are asked for the download and unpacking), and refuses
   it unless its size and SHA-256 are exactly those of `libtorch.sha256`, which the release key
   signs. It is unpacked under `~/.local/lib/gpu-goblin/libtorch/<digest>/` and reused by later
   installs of the same digest;
4. sets `engine = "cuda"`, `libtorch_dir` and, if absent, `device_memory_bytes` (85 % of the card's
   memory as `nvidia-smi` reports it) in `worker.toml`;
5. runs `gg-worker doctor`, which initializes CUDA under the full sandbox before anything starts.

A lab mirror or an offline machine can pass a copy of the archive with `--libtorch-archive FILE`: it
is checked against the same signed digest.

The engine computes in strict float32: the sandbox sets `NVIDIA_TF32_OVERRIDE=0` for the executor.
Building it yourself: `cargo build --release --bin gg-executor --features gg-worker/cuda` with
`LIBTORCH` pointing to the unpacked archive.

## cgroup delegation (resource limits)

Each task runs in a transient systemd scope (`systemd-run --user --scope`) with a memory, CPU and
process limit. The configuration written by the installer says `cgroups = "require"`: the worker
refuses to run tasks without these limits. It works when your user session delegates the `memory`,
`cpu` and `pids` controllers, which is the default of systemd 252 and later (Debian 12, Ubuntu 24.04).
Check:

```bash
cat /sys/fs/cgroup/user.slice/user-$(id -u).slice/user@$(id -u).service/cgroup.subtree_control
# expected to contain: cpu memory pids
```

If `cpu` or `memory` is missing (older systems), an administrator adds the delegation once:

```bash
sudo mkdir -p /etc/systemd/system/user@.service.d
printf '[Service]\nDelegate=pids memory cpu\n' | sudo tee /etc/systemd/system/user@.service.d/delegate.conf
sudo systemctl daemon-reload
# then log out and in again (or reboot), and run: gg-worker --config ~/.config/gpu-goblin/worker.toml doctor
```

`gg-worker doctor` reports `cgroup_limits ... enforces memory.max/cpu.max/pids.max` when it works.
Without an administrator, `cgroups = "auto"` lets the worker run with rlimits only; the doctor then
says so with a warning.

## Everyday use

```bash
systemctl --user status gg-worker
journalctl --user -u gg-worker -f                    # what it does, including peer-to-peer counters
systemctl --user stop gg-worker                      # stops cleanly; the current task goes back to the grid
gg-worker --config ~/.config/gpu-goblin/worker.toml doctor
gg-worker nat-probe                                  # what your router offers for peer-to-peer
```

## Update

Updates are never automatic. When the operator announces a version:

```bash
d=$(mktemp -d) && cd "$d"
for f in SHA256SUMS SHA256SUMS.sshsig install.sh; do curl -fsSLO "https://grid.gpugoblin.com/releases/latest/$f"; done
# 1. the list is signed by the key pinned at your first installation
ssh-keygen -Y verify -f ~/.config/gpu-goblin/allowed_signers -I release@gpu-goblin -n gg-release \
  -s SHA256SUMS.sshsig < SHA256SUMS
# 2. the new installer is the one on that list
grep '  install.sh$' SHA256SUMS | sha256sum --strict -c
# 3. only then run it: it installs the new version next to the old one, switches, restarts
bash install.sh              # add --gpu if you use the GPU build
```

Your `worker.toml` and your worker key are kept. If step 1 fails, stop: the release is not signed by
the key you trust. If the operator announced a **new release key**, check its fingerprint out of band
and pass it explicitly: `bash install.sh --allowed-signers <file with the new line>`.

Rollback to the previous version (it stays installed):

```bash
ls ~/.local/lib/gpu-goblin/                          # versions present
ln -sfn <previous version> ~/.local/lib/gpu-goblin/current
systemctl --user restart gg-worker
```

## Uninstall

```bash
systemctl --user disable --now gg-worker 'gg-worker@*'
rm -f ~/.config/systemd/user/gg-worker.service ~/.config/systemd/user/gg-worker@.service \
  ~/.local/bin/gg-worker ~/.local/bin/gg-desk \
  ~/.local/share/applications/gpu-goblin.desktop ~/.config/autostart/gpu-goblin.desktop
rm -rf ~/.local/lib/gpu-goblin ~/.local/share/gpu-goblin ~/.config/gpu-goblin
```

Tell the operator, who withdraws your worker's authorization (`gg admin worker-authorize <id>
--revoke --yes`); deleting `worker.key` is enough to make the old identity unusable.
