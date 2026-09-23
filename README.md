# gg-worker

The agent you install to lend your CPU or GPU to [GPU Goblin](https://github.com/GPU-Goblin/gg-server),
a platform that trains models and runs large computations for researchers who cannot afford a
computing center.

**What runs on your machine is GPU Goblin's own code, never a researcher's.** The worker receives a
description of the computation (for example a `gg-graph/v1` model) and tensors, and computes them
with the same bounded, verified code as everyone else. The computation runs in a separate process,
`gg-executor`, confined by bubblewrap, Landlock and seccomp, with no network access. Every result is
recomputed by a validator before it is accepted.

> **Status: research software, version 0.1.0.** There is no public GPU Goblin network yet, and no
> monetary value is attached to credits. See
> [gg-server/docs/STATUS.md](https://github.com/GPU-Goblin/gg-server/blob/main/docs/STATUS.md).

## Requirements

- Linux x86_64 (the sandbox relies on Linux namespaces, Landlock and seccomp).
- [bubblewrap](https://github.com/containers/bubblewrap) (`bwrap`), e.g. `apt install bubblewrap`.
- Rust 1.96 to build from source (pinned by `rust-toolchain.toml`).
- For the GPU engine: a Vulkan driver. The GPU is always selected explicitly; the worker never
  falls back to the CPU silently.

## Installing (contributors)

Install a signed release with [deploy/worker/INSTALL.md](deploy/worker/INSTALL.md): the installer
checks the release signature with `ssh-keygen` and every file with `sha256sum` **before** running
anything, installs into your home directory without administrator rights, runs `gg-worker doctor`,
and sets the worker up as a systemd user service. The guide also says, in plain words, what you
agree to (peer-to-peer sharing is on, and what it makes visible) and how to update or uninstall.

## Building from source

```bash
cargo build --release                               # CPU engine
cargo build --release --features gg-worker/gpu      # WGPU/Vulkan engine
```

This builds `target/release/gg-worker` (the agent) and `target/release/gg-executor` (the confined
process it starts for each task; it must stay next to the agent, or be set with `executor_path`).

```bash
cp deploy/worker.example.toml worker.toml           # then set server_url, engine, kinds, limits
gg-worker --config worker.toml init                 # creates the Ed25519 key, prints the public key
gg-worker --config worker.toml doctor               # measures the sandbox before anything runs
gg-worker --config worker.toml run                  # registers, waits for authorization, then works
```

`doctor` checks the namespaces, Landlock, seccomp, resource limits, cgroups and the GPU under
confinement, and runs an escape probe inside the sandbox. With `public = true`, every check must
pass before the worker starts, and the server must be reached over https.

After `run` registers the key, an administrator of the control plane authorizes the worker
(`gg admin worker-authorize <worker_id>`). Until then it waits and runs nothing. If the server
requires a newer version, the worker stops and says so instead of retrying.

## Configuration

[deploy/worker.example.toml](deploy/worker.example.toml) documents every setting. **Local policy
always wins over what the server asks.** The main ones:

| Setting | Meaning |
|---|---|
| `engine` | `cpu`, `gpu` or `gpu:<index>` |
| `kinds` | computation types this worker accepts (e.g. `graph.grad.v1`, `graph.stage.v1`) |
| `max_ram_bytes`, `max_disk_bytes`, `max_download_bytes` | hard limits per task |
| `device_memory_bytes` | memory of the card to announce, with a margin; without it, no model-training task is offered to a GPU worker |
| `cache_max_bytes` | local artifact cache, re-hashed on every hit (0 disables it) |
| `cpu_quota_percent`, `cgroups` | CPU share and memory limits through systemd user scopes |
| `[p2p]` | peer-to-peer sharing: off in `worker.example.toml`, **on** in the installer's configuration (see below) |

Environment variables `GG_*` override the file.

## Peer-to-peer sharing

With `[p2p] enabled = true`, the worker fetches artifacts from other workers as well as from the
control plane, and with `share = true` it serves the chunks it holds. Every chunk received from a
peer is checked against a manifest signed by the control plane before it is written; a peer that
serves wrong bytes is dropped. Connections between peers use TLS 1.3 with a pinned raw public key.

Sharing makes your `ip:port` visible to the workers authorized on the same artifact. Behind a home
router, the worker asks for a port mapping (NAT-PMP, then UPnP-IGD), checks it, renews it and
removes it when it stops sharing. `gg-worker nat-probe` reports what your router offers without
changing anything durable.

Design: ADR-0012 (peer-to-peer transport) and ADR-0013 (TLS and NAT traversal), in
[gg-server/docs/adr](https://github.com/GPU-Goblin/gg-server/tree/main/docs/adr).

## What the worker does not protect

The owner of the computing machine can see what it processes. Each deployment publishes its data
rule, which researchers accept before they create a project: the beta accepts any research data,
with the plain caution that contributors' machines see it. See
[gg-server/docs/THREAT_MODEL.md](https://github.com/GPU-Goblin/gg-server/blob/main/docs/THREAT_MODEL.md).

## Development

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace          # the sandbox tests need bwrap
```

The shared crates (`gg-core`, `gg-protocol`, `gg-compute`, `gg-client`) come from
[gg-core](https://github.com/GPU-Goblin/gg-core) at a pinned tag. The end-to-end demonstration of a
whole grid (server, validator and several workers) lives in
[gg-server](https://github.com/GPU-Goblin/gg-server): `cargo xtask demo` builds the worker from a
checkout of this repository placed next to it.

## License

[PolyForm Noncommercial 1.0.0](LICENSE): free for research and any noncommercial use,
source-available, not open source in the OSI sense. See [LICENSING.md](LICENSING.md).
