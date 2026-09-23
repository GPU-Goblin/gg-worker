# Changelog

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.1.0] — unreleased

First public version of the contributor agent.

- `gg-worker`: registration, claims with bounded long polling, leases and heartbeats, verified
  downloads with a local SHA-256 cache, local resume after an executor crash, `doctor` and
  `nat-probe`.
- `gg-executor`: one confined process per task, under bubblewrap, Landlock, seccomp, resource
  limits and systemd cgroups; CPU and WGPU/Vulkan engines.
- Peer-to-peer transfers: chunks verified against a server-signed manifest, TLS 1.3 with pinned raw
  public keys, several chunks in flight per peer, swarm distribution between readers, NAT-PMP and
  UPnP-IGD port mappings; direct hand-off of stage activations before their durable copy
  (ADR-0026).
- The lease is kept alive for the whole attempt (downloads and uploads included) and sized on the
  announced heartbeat rhythm: a vanished worker is replaced within seconds.
- Plain http refused to a remote server in public mode; stops on SIGTERM; stops and asks to be
  updated when the server requires a newer version.
- Attaches itself to a contributor account with an enrollment code (`install.sh --enroll`,
  `enrollment_token`), inside its own key-proven session: no administrator step.
- The end of a failed executor's error output is kept in the local log (a panic's location was
  lost).
- Compiled GPU kernels are kept between tasks (ADR-0032, `kernel_cache_max_bytes`): CubeCL's cache
  and the driver's, in a directory bound in the sandbox, per executor version. A ViT-tiny task goes
  from 6.7 to 1.1 s.
- Native CUDA engine (`engine = "cuda"`, feature `cuda`, ADR-0033): LibTorch bound read-only in the
  sandbox (`libtorch_dir`), CUDA device nodes, strict float32 imposed; `doctor` checks CUDA under
  the full restriction. Not yet packaged by the installer.
- Executor sessions on GPU engines (ADR-0034, `persistent_executor`): one sandboxed executor per
  training run, restricted once, ended at a run change, after 200 tasks and after any failure. A
  CUDA ViT-tiny task goes from 1.2 to 0.33 s.
- Signed distribution: `deploy/worker/install.sh` (verified with `ssh-keygen` and `sha256sum` before
  anything runs), systemd user unit, contributor guide.

The full history of the project, before it was split into several repositories, is summarized in
[gg-server/CHANGELOG.md](https://github.com/GPU-Goblin/gg-server/blob/main/CHANGELOG.md).
