//! The worker's installer on Windows (ADR-0045), the counterpart of `install.sh`, per user and
//! without administrator rights:
//!
//! 1. the release key this build trusts, pinned at the first installation;
//! 2. `SHA256SUMS` and its signature, verified before anything in it is believed;
//! 3. the Windows files for the chosen engine, each checked against its signed digest; for the
//!    native CUDA engine, LibTorch from PyTorch, checked against the signed pin (size and SHA-256),
//!    unpacked once per digest and shared by the versions;
//! 4. installed side by side under `%LOCALAPPDATA%\Programs\GPU Goblin\<version>`;
//! 5. each worker's configuration from the signed template (an existing one is kept, only its
//!    enrollment code and sandbox are set);
//! 6. the worker's key (`gg-worker init`) and `gg-worker doctor`, which must pass;
//! 7. the application started with the session, a Start menu entry, an entry in Windows' list of
//!    installed apps to remove it; the workers started.

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::install::{self, Engine, Needed};
use crate::winsys;

/// No console window for the programs the installer runs.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// What the owner chose.
pub struct Plan {
    pub server_url: String,
    pub engine: Engine,
    pub enroll: String,
}

/// Where the programs live: one directory per version.
pub fn programs_dir() -> Result<PathBuf, String> {
    std::env::var_os("LOCALAPPDATA")
        .map(|d| PathBuf::from(d).join("Programs").join("GPU Goblin"))
        .ok_or_else(|| "LOCALAPPDATA is not set".to_owned())
}

fn config_dir() -> Result<PathBuf, String> {
    crate::dirs::config_dir().ok_or_else(|| "APPDATA is not set".to_owned())
}

/// Run the installation, reporting each step through `say`. Blocks: run it on its own thread.
pub fn run(plan: &Plan, rt: &tokio::runtime::Handle, say: &dyn Fn(String)) -> Result<(), String> {
    let config_dir = config_dir()?;
    let data_dir = crate::dirs::data_dir();
    let programs = programs_dir()?;

    // 1. The trusted key, pinned.
    let key_line = install::RELEASE_KEY
        .ok_or("this build of GPU Goblin carries no release key: it cannot install anything")?;
    let key = install::trusted_key(key_line)?;
    let pinned = config_dir.join("allowed_signers");
    if let Ok(previous) = std::fs::read_to_string(&pinned)
        && install::trusted_key(&previous)? != key
    {
        return Err(format!(
            "the release key differs from the one pinned at installation ({}); a new key must be \
             announced by the operator and checked out of band",
            pinned.display()
        ));
    }

    // 2. The signed list.
    let release_url = format!("{}/releases/latest", plan.server_url.trim_end_matches('/'));
    say(format!("Verifying the release signature ({release_url})"));
    let client = reqwest::Client::builder()
        .https_only(!plan.server_url.starts_with("http://127.0.0.1"))
        .build()
        .map_err(|e| e.to_string())?;
    let fetch = |name: &str| -> Result<Vec<u8>, String> {
        let url = format!("{release_url}/{name}");
        rt.block_on(async {
            let r = client
                .get(&url)
                .send()
                .await
                .and_then(|r| r.error_for_status())
                .map_err(|e| format!("cannot download {url}: {e}"))?;
            r.bytes()
                .await
                .map(|b| b.to_vec())
                .map_err(|e| format!("cannot download {url}: {e}"))
        })
    };
    let sums = String::from_utf8(fetch("SHA256SUMS")?).map_err(|_| "SHA256SUMS is not text")?;
    let sig = String::from_utf8(fetch("SHA256SUMS.sshsig")?)
        .map_err(|_| "SHA256SUMS.sshsig is not text")?;
    let signed = install::read_signed(&sums, &sig, &key, install::NETWORK)?;
    say(format!(
        "Signed release {} for network {}",
        signed.version, signed.network
    ));

    // 3. The files, checked.
    let needed = install::needed_windows(&signed, plan.engine)?;
    let dest = programs.join(&signed.version);
    let partial = programs.join(format!("{}.partial", signed.version));
    let _ = std::fs::remove_dir_all(&partial);
    std::fs::create_dir_all(&partial).map_err(|e| e.to_string())?;
    let mut template = String::new();
    for Needed {
        source,
        target,
        digest,
    } in &needed
    {
        say(format!("Downloading {source}"));
        let bytes = fetch(source)?;
        if !install::matches(&bytes, digest) {
            return Err(format!(
                "{source} does not match its signed digest: do not use this release"
            ));
        }
        if target == "worker.toml.template" {
            template = String::from_utf8(bytes.clone()).map_err(|_| "the template is not text")?;
        }
        std::fs::write(partial.join(target), &bytes).map_err(|e| e.to_string())?;
    }
    for (name, text) in [("SHA256SUMS", &sums), ("SHA256SUMS.sshsig", &sig)] {
        std::fs::write(partial.join(name), text).map_err(|e| e.to_string())?;
    }
    let libtorch_lib = if plan.engine == Engine::Cuda {
        // LibTorch's libraries need Microsoft's Visual C++ runtime (ours have it built in).
        ensure_vc_runtime(&client, rt, say)?;
        let pin = std::fs::read_to_string(partial.join("libtorch-windows.sha256"))
            .map_err(|e| e.to_string())?;
        let pin = install::libtorch_pin(&pin)?;
        Some(ensure_libtorch(&pin, &programs, &client, rt, say)?)
    } else {
        None
    };

    // 4. Side by side: the running version is not touched.
    say(format!(
        "Installing version {} into {}",
        signed.version,
        dest.display()
    ));
    if dest.exists() {
        // A reinstall of the same version: its programs may be running.
        stop_workers(&dest);
        std::fs::remove_dir_all(&dest).map_err(|e| format!("{}: {e}", dest.display()))?;
    }
    std::fs::rename(&partial, &dest).map_err(|e| e.to_string())?;

    // 5. Configuration.
    std::fs::create_dir_all(&config_dir).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&data_dir).map_err(|e| e.to_string())?;
    std::fs::write(&pinned, format!("{}\n", key_line.trim())).map_err(|e| e.to_string())?;
    let config = config_dir.join("worker.toml");
    let engine = match plan.engine {
        Engine::Cpu => "cpu",
        Engine::Gpu => "gpu",
        Engine::Cuda => "cuda",
    };
    let name = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "worker".into());
    let mut text = match std::fs::read_to_string(&config) {
        Ok(existing) => {
            say(format!("Keeping your existing {}", config.display()));
            let mut t = install::set_config(&existing, "sandbox", "\"appcontainer\"");
            if !plan.enroll.is_empty() {
                t = install::set_config(&t, "enrollment_token", &format!("\"{}\"", plan.enroll));
            }
            t
        }
        Err(_) => install::worker_config(
            &template,
            &plan.server_url,
            &data_dir,
            &name,
            engine,
            &plan.enroll,
            None,
        )?,
    };
    if let Some(lib) = &libtorch_lib {
        // Asked for explicitly: also over an existing configuration (as install.sh --cuda).
        text = install::set_config(&text, "engine", "\"cuda\"");
        let dir = lib.to_string_lossy().replace('\\', "/");
        text = install::set_config(&text, "libtorch_dir", &format!("\"{dir}\""));
        if !text
            .lines()
            .any(|l| l.trim_start().starts_with("device_memory_bytes"))
            && let Some(bytes) = card_memory(0)
        {
            // 85 % of the card, a margin for the display and the driver.
            text = install::set_config(
                &text,
                "device_memory_bytes",
                &(bytes / 100 * 85).to_string(),
            );
            say(format!(
                "device_memory_bytes set to 85 % of the card's {} MiB",
                bytes >> 20
            ));
        }
        say(format!("engine = \"cuda\", libtorch_dir = {dir}"));
    }
    std::fs::write(&config, text).map_err(|e| e.to_string())?;

    // 6. Identity and checks.
    let worker = dest.join("gg-worker.exe");
    say("Creating the worker's key".into());
    let out = hidden(&worker, &["--config", &config.to_string_lossy(), "init"])?;
    say(format!("worker public key: {}", out.trim()));
    say("gg-worker doctor (sandbox, engine)".into());
    let doctor = Command::new(&worker)
        .args(["--config", &config.to_string_lossy(), "doctor"])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run gg-worker: {e}"))?;
    say(String::from_utf8_lossy(&doctor.stdout).into_owned());
    if !doctor.status.success() {
        say(String::from_utf8_lossy(&doctor.stderr).into_owned());
        return Err("gg-worker doctor did not pass: nothing was started".into());
    }

    // 7. The session, the Start menu, the list of installed apps; then start.
    let desk = dest.join("gg-desk.exe");
    winsys::set_run_at_logon("GPU Goblin", &format!("\"{}\" --tray", desk.display()))?;
    start_menu_entry(&desk)?;
    winsys::register_uninstall(&desk, &signed.version)?;
    // Older versions are removed once this one runs.
    for old in std::fs::read_dir(&programs)
        .map_err(|e| e.to_string())?
        .filter_map(Result::ok)
        .map(|e| e.path())
        // LibTorch is shared by the versions and kept by digest.
        .filter(|p| p.is_dir() && *p != dest && !p.ends_with("libtorch"))
    {
        stop_workers(&old);
        let _ = std::fs::remove_dir_all(&old);
    }
    say("Starting GPU Goblin".into());
    // Nothing of the installer's is inherited: the application outlives it, and would otherwise
    // keep the installer's output (a log file) open for as long as it runs (measured).
    Command::new(&desk)
        .arg("--tray")
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot start {}: {e}", desk.display()))?;
    say("Installed. GPU Goblin starts with your session.".into());
    Ok(())
}

/// The unpacked LibTorch of `pin` (its `lib` directory), downloaded from PyTorch and checked
/// against the signed size and digest the first time, then reused by every version.
fn ensure_libtorch(
    pin: &install::LibTorchPin,
    programs: &Path,
    client: &reqwest::Client,
    rt: &tokio::runtime::Handle,
    say: &dyn Fn(String),
) -> Result<PathBuf, String> {
    use sha2::{Digest, Sha256};
    use std::io::Write;
    let base = programs.join("libtorch");
    let dir = base.join(&pin.digest);
    let lib = dir.join("libtorch").join("lib");
    if std::fs::read_to_string(dir.join(".verified")).is_ok_and(|d| d.trim() == pin.digest)
        && lib.join("torch_cuda.dll").is_file()
    {
        say(format!(
            "LibTorch already installed and verified: {}",
            dir.display()
        ));
        return Ok(lib);
    }
    std::fs::create_dir_all(&base).map_err(|e| e.to_string())?;
    // The archive, then about twice its size unpacked.
    if let Some(free) = winsys::free_bytes(&base)
        && free < pin.size.saturating_mul(3)
    {
        return Err(format!(
            "not enough free space for LibTorch: about {} GB needed in {}",
            pin.size * 3 / 1_000_000_000,
            base.display()
        ));
    }
    let archive = base.join(format!(".download-{}.zip", pin.digest));
    say(format!(
        "Downloading LibTorch from PyTorch ({} MB): {}",
        pin.size / 1_000_000,
        pin.url
    ));
    let got = rt.block_on(async {
        let mut r = client
            .get(&pin.url)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| format!("cannot download LibTorch: {e}"))?;
        let mut file =
            std::io::BufWriter::new(std::fs::File::create(&archive).map_err(|e| e.to_string())?);
        let (mut hasher, mut done, mut next) = (Sha256::new(), 0u64, 10u64);
        while let Some(chunk) = r
            .chunk()
            .await
            .map_err(|e| format!("cannot download LibTorch: {e}"))?
        {
            done += chunk.len() as u64;
            if done > pin.size {
                return Err("the LibTorch archive is larger than the signed size".to_owned());
            }
            hasher.update(&chunk);
            file.write_all(&chunk).map_err(|e| e.to_string())?;
            if done * 100 / pin.size.max(1) >= next {
                say(format!("  LibTorch: {next} %"));
                next += 10;
            }
        }
        file.flush().map_err(|e| e.to_string())?;
        Ok::<_, String>((done, hex::encode(hasher.finalize())))
    });
    let (size, digest) = match got {
        Ok(v) => v,
        Err(e) => {
            let _ = std::fs::remove_file(&archive);
            return Err(e);
        }
    };
    if size != pin.size || digest != pin.digest {
        let _ = std::fs::remove_file(&archive);
        return Err(
            "the LibTorch archive does not match the signed size and digest: do not use it".into(),
        );
    }
    say("Unpacking LibTorch".into());
    let partial = base.join(format!("{}.partial", pin.digest));
    let _ = std::fs::remove_dir_all(&partial);
    std::fs::create_dir_all(&partial).map_err(|e| e.to_string())?;
    // Windows' own tar (bsdtar) reads zip archives.
    let tar = Command::new("tar.exe")
        .arg("-xf")
        .arg(&archive)
        .arg("-C")
        .arg(&partial)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot unpack LibTorch: {e}"))?;
    let _ = std::fs::remove_file(&archive);
    if !tar.status.success() || !partial.join("libtorch/lib/torch_cuda.dll").is_file() {
        let _ = std::fs::remove_dir_all(&partial);
        return Err(format!(
            "the LibTorch archive could not be unpacked, or holds no libtorch/lib/torch_cuda.dll: {}",
            String::from_utf8_lossy(&tar.stderr).trim()
        ));
    }
    std::fs::write(partial.join(".verified"), &pin.digest).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::rename(&partial, &dir).map_err(|e| e.to_string())?;
    say(format!("LibTorch installed: {}", dir.display()));
    Ok(lib)
}

/// Microsoft's official installer of the Visual C++ runtime (x64, Visual Studio 2015 to 2022).
const VC_REDIST_URL: &str = "https://aka.ms/vs/17/release/vc_redist.x64.exe";

/// The Visual C++ runtime LibTorch's libraries are linked against (MSVCP140, VCRUNTIME140 and
/// VCRUNTIME140_1): a fresh Windows does not have it, and the CUDA executor then cannot start
/// (STATUS_DLL_NOT_FOUND, measured). When it is missing, Microsoft's own installer is downloaded,
/// accepted only if Windows finds it validly signed by Microsoft, and run; it installs for the whole
/// machine, so Windows asks for an administrator's approval.
fn ensure_vc_runtime(
    client: &reqwest::Client,
    rt: &tokio::runtime::Handle,
    say: &dyn Fn(String),
) -> Result<(), String> {
    let system = std::env::var_os("SystemRoot")
        .map(|r| PathBuf::from(r).join("System32"))
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows\System32"));
    let present = || {
        ["msvcp140.dll", "vcruntime140.dll", "vcruntime140_1.dll"]
            .iter()
            .all(|f| system.join(f).is_file())
    };
    if present() {
        return Ok(());
    }
    say(
        "The CUDA engine needs Microsoft's Visual C++ runtime: downloading it from Microsoft"
            .into(),
    );
    let exe = std::env::temp_dir().join("gpu-goblin-vc_redist.x64.exe");
    let bytes = rt.block_on(async {
        client
            .get(VC_REDIST_URL)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| format!("cannot download {VC_REDIST_URL}: {e}"))?
            .bytes()
            .await
            .map_err(|e| format!("cannot download {VC_REDIST_URL}: {e}"))
    })?;
    std::fs::write(&exe, &bytes).map_err(|e| e.to_string())?;
    // Windows' own check of the signature; the path travels in the environment.
    let check = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "$s = Get-AuthenticodeSignature -LiteralPath $env:GG_FILE; \
             if ($s.Status -eq 'Valid' -and $s.SignerCertificate.Subject -match 'O=Microsoft Corporation') \
             { 'trusted' } else { 'refused ' + $s.Status }",
        ])
        .env("GG_FILE", &exe)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot check the Visual C++ installer: {e}"))?;
    let verdict = String::from_utf8_lossy(&check.stdout).trim().to_owned();
    if verdict != "trusted" {
        let _ = std::fs::remove_file(&exe);
        return Err(format!(
            "the Visual C++ installer downloaded from Microsoft is not validly signed by Microsoft \
             ({verdict}): not run"
        ));
    }
    say("Installing the Visual C++ runtime (Windows asks for an administrator's approval)".into());
    let code = winsys::run_elevated(&exe, "/install /passive /norestart");
    let _ = std::fs::remove_file(&exe);
    match code {
        // 1638: a newer version is already there; 3010: installed, a restart is advised.
        Ok(0 | 1638 | 3010) if present() => {
            say("Visual C++ runtime installed".into());
            Ok(())
        }
        Ok(c) => Err(format!(
            "the Visual C++ runtime was not installed (code {c}): the CUDA engine cannot start \
             without it; install it from {VC_REDIST_URL}, or choose the graphics card (Vulkan)"
        )),
        Err(e) => Err(format!(
            "the Visual C++ runtime was not installed ({e}): the CUDA engine cannot start without \
             it; install it from {VC_REDIST_URL}, or choose the graphics card (Vulkan)"
        )),
    }
}

/// The memory of NVIDIA card `index`, in bytes, from `nvidia-smi` (installed with the driver).
fn card_memory(index: u32) -> Option<u64> {
    let out = Command::new("nvidia-smi")
        .args([
            "--query-gpu=memory.total",
            "--format=csv,noheader,nounits",
            "-i",
            &index.to_string(),
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    let mib: u64 = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()?
        .trim()
        .parse()
        .ok()?;
    out.status.success().then_some(mib << 20)
}

/// Run a program without a console window; its standard output if it succeeds.
fn hidden(program: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new(program)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run {}: {e}", program.display()))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!(
            "{} failed: {}",
            program.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Stop the workers started from `dir` (before that directory is replaced or removed).
fn stop_workers(dir: &Path) {
    winsys::kill_processes_under(dir);
}

/// A Start menu entry for the application, written by the Windows shell itself (PowerShell's
/// `WScript.Shell`); the paths travel in the environment, never inside the command text.
fn start_menu_entry(desk: &Path) -> Result<(), String> {
    let programs = std::env::var_os("APPDATA")
        .map(|d| PathBuf::from(d).join(r"Microsoft\Windows\Start Menu\Programs"))
        .ok_or("APPDATA is not set")?;
    let out = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "$s = (New-Object -ComObject WScript.Shell).CreateShortcut($env:GG_LNK); \
             $s.TargetPath = $env:GG_TARGET; $s.WorkingDirectory = $env:GG_DIR; \
             $s.Description = 'GPU Goblin'; $s.Save()",
        ])
        .env("GG_LNK", programs.join("GPU Goblin.lnk"))
        .env("GG_TARGET", desk)
        .env("GG_DIR", desk.parent().unwrap_or(desk))
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot create the Start menu entry: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "cannot create the Start menu entry: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Remove what the installer added: the programs, the start with the session, the Start menu
/// entry, the entry in the list of installed apps. The worker's key, configuration and account
/// stay unless `everything` (then the machine must be enrolled again).
pub fn uninstall(everything: bool) -> Result<(), String> {
    let programs = programs_dir()?;
    stop_workers(&programs);
    winsys::remove_run_at_logon("GPU Goblin");
    winsys::unregister_uninstall();
    if let Some(d) = std::env::var_os("APPDATA") {
        let _ = std::fs::remove_file(
            PathBuf::from(d).join(r"Microsoft\Windows\Start Menu\Programs\GPU Goblin.lnk"),
        );
    }
    // The running program cannot delete itself: a hidden PowerShell waits until this process has
    // ended (the owner may leave the last message open for long), then removes the directory. A
    // fixed delay left gg-desk.exe behind (measured). The process number and the path travel in
    // the environment, never inside the command text.
    let _ = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Wait-Process -Id $env:GG_PID -Timeout 3600 -ErrorAction SilentlyContinue; \
             Start-Sleep -Milliseconds 500; \
             Remove-Item -LiteralPath $env:GG_DIR -Recurse -Force -ErrorAction SilentlyContinue",
        ])
        .env("GG_PID", std::process::id().to_string())
        .env("GG_DIR", &programs)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if everything {
        let _ = std::fs::remove_dir_all(crate::dirs::data_dir());
        if let Some(c) = crate::dirs::config_dir() {
            let _ = std::fs::remove_dir_all(c);
        }
    }
    Ok(())
}
