//! The Windows sandbox (ADR-0045), run for real: the breakout probe must hold inside the
//! AppContainer and its job, a restricted executor must still compute, and the agent's session
//! must serve a task in it. Runs on Windows only; there it fails, it does not skip.
#![cfg(windows)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use gg_core::{ExperimentId, JobId, NetworkId, ProjectId, Sha256Digest, TaskId};
use gg_protocol::messages::{ArtifactRef, TaskSpecV1};
use gg_protocol::tensor_file::TensorF32;
use gg_worker::appcontainer::{executor_spec, spawn};
use gg_worker::bundle::{Bundle, ExecutorReport};
use gg_worker::restrict::ProbeReport;
use tokio::io::AsyncReadExt;

const MEMORY: u64 = 1 << 30;

/// The executor next to the test, or `GG_TEST_EXECUTOR` (tests built on another machine carry
/// the build machine's path).
fn executor() -> PathBuf {
    std::env::var_os("GG_TEST_EXECUTOR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_gg-executor")))
}

fn args(a: &[&str]) -> Vec<OsString> {
    a.iter().map(OsString::from).collect()
}

/// Run a spec to its end: exit code, standard output, standard error.
async fn run(spec: &gg_worker::appcontainer::Spec) -> (u32, Vec<u8>, String) {
    let mut child = spawn(spec, false).expect("start in the AppContainer");
    let (mut out, mut err) = (Vec::new(), Vec::new());
    child
        .stdout
        .as_mut()
        .unwrap()
        .read_to_end(&mut out)
        .await
        .unwrap();
    child
        .stderr
        .as_mut()
        .unwrap()
        .read_to_end(&mut err)
        .await
        .unwrap();
    let code = child.wait().await.unwrap();
    (code, out, String::from_utf8_lossy(&err).into_owned())
}

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

fn check_product(bundle: &Bundle, a: &TensorF32, b: &TensorF32) {
    let report: ExecutorReport =
        serde_json::from_slice(&std::fs::read(bundle.report_file()).unwrap()).unwrap();
    let c = TensorF32::read_from_path(
        &bundle.outputs_dir().join(&report.outputs[0].file_name),
        &Default::default(),
    )
    .unwrap();
    let oracle = gg_compute::matmul::matmul_naive_f64(a, b);
    let tol = gg_protocol::messages::ToleranceV1 {
        abs: 1e-5,
        rel: 1e-5,
    };
    assert!(gg_compute::compare::compare_f32("c", &c, &oracle, tol).passed);
}

#[tokio::test]
async fn breakout_probe_holds_inside_the_appcontainer() {
    let work = tempfile::tempdir().unwrap();
    let bundle = Bundle::new(work.path(), "probe");
    bundle.create().unwrap();
    // A file of the user's, elsewhere: the sandbox is not granted it.
    let secret = tempfile::tempdir().unwrap();
    let canary = secret.path().join("canary.txt");
    std::fs::write(&canary, b"user secret").unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port().to_string();
    let mut a = args(&["probe", "--task-dir"]);
    a.push(bundle.root.clone().into());
    a.extend(args(&["--canary-port", &port, "--canary-file"]));
    a.push(canary.clone().into());
    a.extend(args(&["--memory-limit", &MEMORY.to_string()]));
    let spec = executor_spec(
        &executor(),
        &bundle.root,
        false,
        a,
        None,
        None,
        MEMORY,
        None,
    )
    .unwrap();
    let (code, out, err) = run(&spec).await;
    assert_eq!(code, 0, "probe failed: {err}");
    let report: ProbeReport = serde_json::from_slice(&out).unwrap();
    for c in &report.checks {
        println!("{:<22} {:<5} {}", c.name, c.ok, c.detail);
    }
    let failed: Vec<_> = report
        .checks
        .iter()
        .filter(|c| !c.ok)
        .map(|c| format!("{}: {}", c.name, c.detail))
        .collect();
    assert!(failed.is_empty(), "protections not holding: {failed:#?}");
    assert!(report.apply_error.is_none(), "{:?}", report.apply_error);
    drop(listener);
}

#[tokio::test]
async fn outside_the_sandbox_the_executor_refuses_and_the_probe_fails() {
    let work = tempfile::tempdir().unwrap();
    let (bundle, _, _) = write_matmul_bundle(work.path(), "task", 1);
    // The executor started as a plain program must refuse to compute restricted.
    let out = std::process::Command::new(executor())
        .arg("run")
        .arg("--task-dir")
        .arg(&bundle.root)
        .args(["--engine", "cpu", "--restrict"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("AppContainer"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!bundle.report_file().exists());
}

#[tokio::test]
async fn a_restricted_executor_computes_a_matmul() {
    let work = tempfile::tempdir().unwrap();
    let (bundle, a, b) = write_matmul_bundle(work.path(), "task", 2);
    let mut x = args(&["run", "--task-dir"]);
    x.push(bundle.root.clone().into());
    x.extend(args(&["--engine", "cpu", "--restrict"]));
    let spec = executor_spec(
        &executor(),
        &bundle.root,
        false,
        x,
        None,
        None,
        MEMORY,
        Some(100),
    )
    .unwrap();
    let (code, _, err) = run(&spec).await;
    assert_eq!(code, 0, "executor failed: {err}");
    assert!(err.contains("landlock=appcontainer"), "{err}");
    check_product(&bundle, &a, &b);
    let mut names: Vec<String> = std::fs::read_dir(&bundle.root)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, vec!["inputs", "outputs", "task.cbor", "tmp"]);
}

#[tokio::test]
async fn the_agents_session_serves_a_task_in_the_appcontainer() {
    use gg_worker::launch::Launch;
    use gg_worker::session::{Answer, Session};
    let root = tempfile::tempdir().unwrap();
    let (bundle, a, b) = write_matmul_bundle(root.path(), "t1", 3);
    let mut x = args(&["serve", "--root"]);
    x.push(root.path().into());
    x.extend(args(&[
        "--engine",
        "cpu",
        "--restrict",
        "--max-file-bytes",
        "67108864",
    ]));
    let spec = executor_spec(&executor(), root.path(), true, x, None, None, MEMORY, None).unwrap();
    let scope = (ProjectId::new(), ExperimentId::new());
    let mut s = Session::spawn(Launch::AppContainer(spec), scope).unwrap();
    s.send("t1").await.unwrap();
    match s.answer().await {
        Answer::Done => {}
        Answer::Failed(e) => panic!("task failed: {e}"),
        Answer::Exited(e) => panic!("session ended: {e}"),
    }
    check_product(&bundle, &a, &b);
    s.send("..\\t1").await.unwrap();
    assert!(matches!(s.answer().await, Answer::Failed(_)));
    s.kill().await;
}
