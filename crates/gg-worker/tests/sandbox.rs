//! Sandbox tests: the breakout probe must hold inside the real sandbox, and a restricted executor
//! must still compute. Requires bubblewrap on the host (the test fails, it does not skip). The
//! Windows sandbox has its own tests (`sandbox_windows.rs`).
#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::path::Path;

use gg_compute::EngineSelection;
use gg_core::{ExperimentId, JobId, NetworkId, ProjectId, Sha256Digest, TaskId};
use gg_protocol::messages::{ArtifactRef, TaskSpecV1};
use gg_protocol::tensor_file::TensorF32;
use gg_worker::bundle::{Bundle, ExecutorReport, SANDBOX_BUNDLE_DIR};
use gg_worker::restrict::ProbeReport;
use gg_worker::sandbox::{find_bwrap, plan, std_command};

fn executor() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_gg-executor"))
}

#[test]
fn breakout_probe_holds_inside_the_sandbox() {
    let bwrap = find_bwrap().expect("bubblewrap (bwrap) must be installed for sandbox tests");
    let work = tempfile::tempdir().unwrap();
    let bundle = Bundle::new(work.path(), "probe");
    bundle.create().unwrap();
    let canary = work.path().join("canary.txt");
    std::fs::write(&canary, b"host secret").unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port().to_string();
    let canary_s = canary.to_string_lossy().into_owned();
    let inner = [
        "probe",
        "--task-dir",
        SANDBOX_BUNDLE_DIR,
        "--canary-port",
        &port,
        "--canary-file",
        &canary_s,
    ];
    let p = plan(
        &bwrap,
        executor(),
        &bundle.root,
        EngineSelection::FlexCpu,
        &inner,
    )
    .unwrap();
    let out = std_command(&p).output().unwrap();
    assert!(
        out.status.success(),
        "probe failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: ProbeReport = serde_json::from_slice(&out.stdout).unwrap();
    let failed: Vec<_> = report
        .checks
        .iter()
        .filter(|c| !c.ok)
        .map(|c| format!("{}: {}", c.name, c.detail))
        .collect();
    assert!(failed.is_empty(), "protections not holding: {failed:?}");
    let applied = report.applied.expect("restrictions applied");
    assert_eq!(applied.landlock, "fully_enforced");
    assert!(applied.no_new_privs && applied.seccomp);

    // Without self-restriction, the namespace layer alone must still isolate mounts/network/pids,
    // but Landlock/seccomp checks fail — proving the probe measures something real.
    let inner = [
        "probe",
        "--task-dir",
        SANDBOX_BUNDLE_DIR,
        "--canary-port",
        &port,
        "--canary-file",
        &canary_s,
        "--no-restrict",
    ];
    let p = plan(
        &bwrap,
        executor(),
        &bundle.root,
        EngineSelection::FlexCpu,
        &inner,
    )
    .unwrap();
    let out = std_command(&p).output().unwrap();
    let report: ProbeReport = serde_json::from_slice(&out.stdout).unwrap();
    let by_name: BTreeMap<_, _> = report
        .checks
        .iter()
        .map(|c| (c.name.as_str(), c.ok))
        .collect();
    assert!(by_name["mount_isolated"] && by_name["pid_namespace"] && by_name["network_interfaces"]);
    assert!(
        !by_name["seccomp_active"]
            && !by_name["landlock_enforced"]
            && !by_name["bundle_root_read_only"]
    );
}

#[test]
fn restricted_executor_still_computes_a_matmul() {
    let bwrap = find_bwrap().expect("bubblewrap (bwrap) must be installed for sandbox tests");
    let work = tempfile::tempdir().unwrap();
    let bundle = Bundle::new(work.path(), "task");
    bundle.create().unwrap();
    let mut d = gg_protocol::drbg::Drbg::new("sandbox-test", 1);
    let a = TensorF32::new(vec![8, 12], d.sym_f32_vec(96)).unwrap();
    let b = TensorF32::new(vec![12, 4], d.sym_f32_vec(48)).unwrap();
    let mut refs = Vec::new();
    for (name, t) in [("a", &a), ("b", &b)] {
        let bytes = t.encode();
        std::fs::write(bundle.input_path(name), &bytes).unwrap();
        refs.push(ArtifactRef {
            hash: Sha256Digest::of_bytes(&bytes),
            size: bytes.len() as u64,
        });
    }
    let def = gg_compute::catalog::matmul_definition(8, 12, 4, refs[0], refs[1]).unwrap();
    let spec = TaskSpecV1 {
        spec_version: 1,
        network: NetworkId::new("gg-test").unwrap(),
        project_id: ProjectId::new(),
        experiment_id: ExperimentId::new(),
        job_id: JobId::new(),
        task_id: TaskId::new(),
        index: 0,
        definition: def,
    };
    std::fs::write(
        bundle.task_file(),
        gg_protocol::canonical::to_bytes(&spec).unwrap(),
    )
    .unwrap();
    let inner = [
        "run",
        "--task-dir",
        SANDBOX_BUNDLE_DIR,
        "--engine",
        "cpu",
        "--restrict",
    ];
    let p = plan(
        &bwrap,
        executor(),
        &bundle.root,
        EngineSelection::FlexCpu,
        &inner,
    )
    .unwrap();
    let out = std_command(&p).output().unwrap();
    assert!(
        out.status.success(),
        "executor failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("landlock=fully_enforced"));
    let report: ExecutorReport =
        serde_json::from_slice(&std::fs::read(bundle.report_file()).unwrap()).unwrap();
    assert_eq!(report.outputs.len(), 1);
    let c = TensorF32::read_from_path(
        &bundle.outputs_dir().join(&report.outputs[0].file_name),
        &Default::default(),
    )
    .unwrap();
    let oracle = gg_compute::matmul::matmul_naive_f64(&a, &b);
    assert!(
        gg_compute::compare::compare_f32(
            "c",
            &c,
            &oracle,
            gg_protocol::messages::ToleranceV1 {
                abs: 1e-5,
                rel: 1e-5
            }
        )
        .passed
    );
    // Nothing but the outputs and report was written into the bundle.
    let names: Vec<String> = std::fs::read_dir(bundle.root)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(sorted, vec!["inputs", "outputs", "task.cbor"]);
}

/// Write a matmul task bundle named `name` under `root`; returns its inputs.
fn write_matmul_bundle(root: &Path, name: &str, seed: u64) -> (Bundle, TensorF32, TensorF32) {
    let bundle = Bundle::new(root, name);
    bundle.create().unwrap();
    let mut d = gg_protocol::drbg::Drbg::new("sandbox-test", seed);
    let a = TensorF32::new(vec![8, 12], d.sym_f32_vec(96)).unwrap();
    let b = TensorF32::new(vec![12, 4], d.sym_f32_vec(48)).unwrap();
    let mut refs = Vec::new();
    for (input, t) in [("a", &a), ("b", &b)] {
        let bytes = t.encode();
        std::fs::write(bundle.input_path(input), &bytes).unwrap();
        refs.push(ArtifactRef {
            hash: Sha256Digest::of_bytes(&bytes),
            size: bytes.len() as u64,
        });
    }
    let def = gg_compute::catalog::matmul_definition(8, 12, 4, refs[0], refs[1]).unwrap();
    let spec = TaskSpecV1 {
        spec_version: 1,
        network: NetworkId::new("gg-test").unwrap(),
        project_id: ProjectId::new(),
        experiment_id: ExperimentId::new(),
        job_id: JobId::new(),
        task_id: TaskId::new(),
        index: 0,
        definition: def,
    };
    std::fs::write(
        bundle.task_file(),
        gg_protocol::canonical::to_bytes(&spec).unwrap(),
    )
    .unwrap();
    (bundle, a, b)
}

/// An executor session (ADR-0034): restricted once, then several tasks in the same process, each
/// computed right; a request naming anything but a bundle of the session root is refused; the
/// session ends when its input closes.
#[test]
fn an_executor_session_serves_several_tasks_in_one_sandbox() {
    use std::io::{BufRead, BufReader, Write};
    let bwrap = find_bwrap().expect("bubblewrap (bwrap) must be installed for sandbox tests");
    let root = tempfile::tempdir().unwrap();
    let tasks: Vec<_> = (1..=2u64)
        .map(|i| write_matmul_bundle(root.path(), &format!("t{i}"), i))
        .collect();
    let inner = [
        "serve",
        "--root",
        SANDBOX_BUNDLE_DIR,
        "--engine",
        "cpu",
        "--restrict",
        "--max-file-bytes",
        "67108864",
    ];
    let p = plan(
        &bwrap,
        executor(),
        root.path(),
        EngineSelection::FlexCpu,
        &inner,
    )
    .unwrap();
    let mut child = std_command(&p)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut ask = |task: &str| -> serde_json::Value {
        writeln!(stdin, "{}", serde_json::json!({ "task": task })).unwrap();
        stdin.flush().unwrap();
        serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap()
    };
    for (i, (bundle, a, b)) in tasks.iter().enumerate() {
        let answer = ask(&format!("t{}", i + 1));
        assert_eq!(answer["ok"], true, "{answer}");
        let report: ExecutorReport =
            serde_json::from_slice(&std::fs::read(bundle.report_file()).unwrap()).unwrap();
        let c = TensorF32::read_from_path(
            &bundle.outputs_dir().join(&report.outputs[0].file_name),
            &Default::default(),
        )
        .unwrap();
        let tol = gg_protocol::messages::ToleranceV1 {
            abs: 1e-5,
            rel: 1e-5,
        };
        let oracle = gg_compute::matmul::matmul_naive_f64(a, b);
        assert!(gg_compute::compare::compare_f32("c", &c, &oracle, tol).passed);
    }
    for bad in ["../etc", "t1/../t2", "", "a b"] {
        let answer = ask(bad);
        assert_eq!(answer["ok"], false, "{bad}: {answer}");
    }
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        stderr.matches("landlock=fully_enforced").count(),
        1,
        "{stderr}"
    );
}

/// The worker's own session code (`Session` over `Launch`, as the agent starts it): a task
/// answered in the real sandbox, a bad name refused, the session killed.
#[tokio::test]
async fn the_agents_session_launch_serves_a_task() {
    use gg_worker::launch::Launch;
    use gg_worker::session::{Answer, Session};
    let bwrap = find_bwrap().expect("bubblewrap (bwrap) must be installed for sandbox tests");
    let root = tempfile::tempdir().unwrap();
    let (bundle, _, _) = write_matmul_bundle(root.path(), "t1", 1);
    let inner = [
        "serve",
        "--root",
        SANDBOX_BUNDLE_DIR,
        "--engine",
        "cpu",
        "--restrict",
        "--max-file-bytes",
        "67108864",
    ];
    let p = plan(
        &bwrap,
        executor(),
        root.path(),
        EngineSelection::FlexCpu,
        &inner,
    )
    .unwrap();
    let mut launch = Launch::Command(gg_worker::sandbox::command(&p));
    launch.env("RUST_LOG", "info");
    let scope = (ProjectId::new(), ExperimentId::new());
    let mut s = Session::spawn(launch, scope).unwrap();
    s.send("t1").await.unwrap();
    assert!(matches!(s.answer().await, Answer::Done));
    assert!(bundle.report_file().is_file());
    s.send("../etc").await.unwrap();
    assert!(matches!(s.answer().await, Answer::Failed(_)));
    s.kill().await;
}
