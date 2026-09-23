//! Compiled GPU kernels kept between tasks (ADR-0032). Each task runs in a fresh sandboxed
//! executor, which otherwise compiles its kernels again: 5.1 s of a 7.2 s ViT-tiny task. The
//! executor writes them into a directory of the worker, the only thing that outlives the task.
//!
//! The directory is named after a fingerprint of the executor binary: an update starts from an
//! empty cache, and the previous ones are removed. It is bounded: over its budget, it is emptied at
//! start-up rather than trimmed (a cache is only ever a speed-up).

use std::path::{Path, PathBuf};

use gg_core::Sha256Digest;

/// Prepare the cache for the executor at `executor` under `work_dir`, within `max_bytes` (0: no
/// cache). Returns the directory to lend to the executor.
pub fn prepare(
    work_dir: &Path,
    executor: &Path,
    max_bytes: u64,
) -> std::io::Result<Option<PathBuf>> {
    if max_bytes == 0 {
        return Ok(None);
    }
    let fingerprint = Sha256Digest::of_bytes(&std::fs::read(executor)?).to_hex();
    let root = work_dir.join("kernel-cache");
    std::fs::create_dir_all(&root)?;
    let dir = root.join(&fingerprint[..16]);
    for entry in std::fs::read_dir(&root)? {
        let entry = entry?;
        if entry.path() != dir {
            remove(&entry.path())?;
        }
    }
    if dir.exists() && size(&dir)? > max_bytes {
        tracing::info!(dir = %dir.display(), max_bytes, "kernel cache over its budget: emptied");
        std::fs::remove_dir_all(&dir)?;
    }
    // The GPU driver's own cache (SPIR-V to machine code) goes into `driver/`, which the driver
    // does not create itself: without it, 4.7 of the 5.5 s of a ViT-tiny first call remained.
    std::fs::create_dir_all(dir.join("driver"))?;
    std::fs::create_dir_all(dir.join("cuda"))?;
    Ok(Some(dir))
}

fn remove(p: &Path) -> std::io::Result<()> {
    if p.is_dir() {
        std::fs::remove_dir_all(p)
    } else {
        std::fs::remove_file(p)
    }
}

/// Bytes under `dir`, links not followed.
fn size(dir: &Path) -> std::io::Result<u64> {
    let mut total = 0;
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let meta = path.symlink_metadata()?;
        total += if meta.is_dir() {
            size(&path)?
        } else {
            meta.len()
        };
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_directory_per_executor_bounded_and_disabled_at_zero() {
        let work = tempfile::tempdir().unwrap();
        let exe = work.path().join("gg-executor");
        std::fs::write(&exe, b"executor v1").unwrap();
        assert!(prepare(work.path(), &exe, 0).unwrap().is_none());
        let d1 = prepare(work.path(), &exe, 1 << 20).unwrap().unwrap();
        assert!(d1.join("driver").is_dir());
        std::fs::create_dir_all(d1.join("spirv")).unwrap();
        std::fs::write(d1.join("spirv/vulkan"), vec![0u8; 1000]).unwrap();
        // Same executor: same directory, content kept.
        assert_eq!(prepare(work.path(), &exe, 1 << 20).unwrap().unwrap(), d1);
        assert!(d1.join("spirv/vulkan").exists());
        // Over its budget: emptied.
        assert_eq!(prepare(work.path(), &exe, 500).unwrap().unwrap(), d1);
        assert!(!d1.join("spirv/vulkan").exists());
        // Another executor: another directory, the previous one removed.
        std::fs::write(&exe, b"executor v2").unwrap();
        let d2 = prepare(work.path(), &exe, 1 << 20).unwrap().unwrap();
        assert_ne!(d1, d2);
        assert!(!d1.exists());
        assert!(d2.exists());
    }
}
