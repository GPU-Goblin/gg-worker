//! Task bundle layout shared by the agent and the executor.
//!
//! ```text
//! <work_dir>/<attempt_id>/
//!   task.cbor        canonical TaskSpecV1 (written by the agent)
//!   inputs/<name>    verified input artifacts
//!   outputs/<name>.ggt
//!   outputs/report.json   ExecutorReport (written by the executor)
//! ```
//!
//! Inside the sandbox the bundle is mounted at [`SANDBOX_BUNDLE_DIR`]; the executor binary at
//! [`SANDBOX_EXECUTOR_PATH`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use gg_core::Sha256Digest;
use gg_protocol::messages::{ArtifactRef, ExecutionProfileV1};
use serde::{Deserialize, Serialize};

/// Mount point of the bundle inside the sandbox.
pub const SANDBOX_BUNDLE_DIR: &str = "/bundle";
/// Path of the LibTorch libraries inside the sandbox (native CUDA engine, ADR-0033).
pub const SANDBOX_LIBTORCH_DIR: &str = "/opt/libtorch";
/// Path of the compiled-kernel cache inside the sandbox (ADR-0032).
pub const SANDBOX_KERNEL_CACHE_DIR: &str = "/kernel-cache";
/// Path of the executor binary inside the sandbox.
pub const SANDBOX_EXECUTOR_PATH: &str = "/gg-executor";

/// Bundle paths.
#[derive(Clone, Debug)]
pub struct Bundle {
    /// Root.
    pub root: PathBuf,
}

impl Bundle {
    /// Bundle for an attempt.
    pub fn new(work_dir: &Path, attempt: &str) -> Self {
        Self {
            root: work_dir.join(attempt),
        }
    }
    /// Spec file.
    pub fn task_file(&self) -> PathBuf {
        self.root.join("task.cbor")
    }
    /// Inputs dir.
    pub fn inputs_dir(&self) -> PathBuf {
        self.root.join("inputs")
    }
    /// Outputs dir.
    pub fn outputs_dir(&self) -> PathBuf {
        self.root.join("outputs")
    }
    /// Report file.
    pub fn report_file(&self) -> PathBuf {
        self.outputs_dir().join("report.json")
    }
    /// Input path by name (names are validated by the protocol: no separators).
    pub fn input_path(&self, name: &str) -> PathBuf {
        self.inputs_dir().join(name)
    }
    /// Create directories.
    pub fn create(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(self.inputs_dir())?;
        std::fs::create_dir_all(self.outputs_dir())
    }
    /// Remove the bundle.
    pub fn remove(&self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
    /// Input map for the catalog.
    pub fn inputs(&self, names: impl IntoIterator<Item = String>) -> BTreeMap<String, PathBuf> {
        names
            .into_iter()
            .map(|n| (n.clone(), self.input_path(&n)))
            .collect()
    }
}

/// Output produced by the executor. `file_name` is relative to the bundle's `outputs/` directory
/// because the executor may see the bundle at a different mount point than the agent.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReportOutput {
    /// Name.
    pub name: String,
    /// File name inside `outputs/` (no directory components).
    pub file_name: String,
    /// Hash.
    pub hash: Sha256Digest,
    /// Size.
    pub size: u64,
}

impl ReportOutput {
    /// Artifact reference.
    pub fn artifact_ref(&self) -> ArtifactRef {
        ArtifactRef {
            hash: self.hash,
            size: self.size,
        }
    }
}

/// Report written by the executor.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecutorReport {
    /// Outputs.
    pub outputs: Vec<ReportOutput>,
    /// Declared execution profile.
    pub execution_profile: ExecutionProfileV1,
    /// Declared metrics.
    pub metrics: BTreeMap<String, f64>,
}
