//! What the worker's installer decides, independent of the system: which release key is trusted,
//! whether the checksum list is signed by it, which files to take from it, whether a download
//! matches its signed digest, and each worker's configuration. `install.sh` does the same on Linux
//! with OpenSSH and coreutils; on Windows the application does it with these functions
//! (ADR-0045), and nothing downloaded is run before it is checked.

use std::path::Path;

use gg_protocol::keys::PublicKey;
use gg_protocol::release::{
    CHECKSUMS_FILE, SSHSIG_NAMESPACE, SSHSIG_PRINCIPAL, parse_ssh_public_key, sshsig_verify,
};
use sha2::{Digest, Sha256};

/// The release key this build of the application trusts, as one `allowed_signers` line; set when
/// the release is built (`GG_RELEASE_ALLOWED_SIGNERS`). A build without it installs nothing.
pub const RELEASE_KEY: Option<&str> = option_env!("GG_RELEASE_ALLOWED_SIGNERS");

/// The network this build installs for (`install.sh`'s `DEFAULT_NETWORK`), set when the release is
/// built (`GG_RELEASE_NETWORK`, `cargo xtask windows --network`).
pub const NETWORK: &str = match option_env!("GG_RELEASE_NETWORK") {
    Some(n) => n,
    None => "gg-beta-1",
};

/// The engine a worker computes with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Engine {
    Cpu,
    /// Vulkan.
    Gpu,
    /// The native CUDA engine (NVIDIA): the GPU build of the worker with the CUDA executor, and
    /// LibTorch from PyTorch, checked against the signed pin.
    Cuda,
}

/// The key of an `allowed_signers` line, accepted only for the release principal and namespace.
pub fn trusted_key(allowed_signers: &str) -> Result<PublicKey, String> {
    let line = allowed_signers
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .ok_or("no release key")?;
    let mut f = line.split_whitespace();
    if f.next() != Some(SSHSIG_PRINCIPAL) {
        return Err(format!("the release key is not for {SSHSIG_PRINCIPAL}"));
    }
    if f.next() != Some(&format!("namespaces=\"{SSHSIG_NAMESPACE}\"")[..]) {
        return Err(format!(
            "the release key is not limited to {SSHSIG_NAMESPACE}"
        ));
    }
    let key: Vec<&str> = f.collect();
    parse_ssh_public_key(&key.join(" ")).map_err(|e| e.to_string())
}

/// A checksum list whose signature was verified.
#[derive(Debug)]
pub struct Signed {
    pub version: String,
    pub network: String,
    /// File name and SHA-256 digest (hex), in the order of the list.
    pub files: Vec<(String, String)>,
}

/// Verify `SHA256SUMS` against its signature and the trusted key, then read it. Nothing in it is
/// believed before the signature holds.
pub fn read_signed(
    sums: &str,
    sig: &str,
    key: &PublicKey,
    network: &str,
) -> Result<Signed, String> {
    let signer = sshsig_verify(sig, SSHSIG_NAMESPACE, sums.as_bytes())
        .map_err(|e| format!("{CHECKSUMS_FILE}: {e}"))?;
    if signer != *key {
        return Err(format!(
            "{CHECKSUMS_FILE} is signed by another key than the trusted release key"
        ));
    }
    let comment = |k: &str| {
        sums.lines()
            .find_map(|l| l.strip_prefix(&format!("# {k}: ")))
            .map(str::to_owned)
    };
    let version = comment("version").ok_or("no version in the signed list")?;
    let got = comment("network").ok_or("no network in the signed list")?;
    if got != network {
        return Err(format!(
            "this release is for network '{got}', this grid is '{network}'"
        ));
    }
    if version.is_empty()
        || version.len() > 64
        || !version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._+-".contains(c))
    {
        return Err("unexpected version string in the signed list".into());
    }
    let mut files = Vec::new();
    for l in sums
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
    {
        let (digest, name) = l
            .split_once("  ")
            .ok_or_else(|| format!("malformed line in {CHECKSUMS_FILE}: {l}"))?;
        if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("malformed digest in {CHECKSUMS_FILE}: {l}"));
        }
        files.push((name.to_owned(), digest.to_ascii_lowercase()));
    }
    Ok(Signed {
        version,
        network: got,
        files,
    })
}

/// A file to install: its name in the release, its name once installed, its signed digest.
#[derive(Debug, PartialEq, Eq)]
pub struct Needed {
    pub source: String,
    pub target: String,
    pub digest: String,
}

/// The Windows files of the release for `engine`, each listed exactly once.
pub fn needed_windows(signed: &Signed, engine: Engine) -> Result<Vec<Needed>, String> {
    let (worker, executor) = match engine {
        Engine::Cpu => ("gg-worker-windows.exe", "gg-executor-windows.exe"),
        Engine::Gpu => ("gg-worker-gpu-windows.exe", "gg-executor-gpu-windows.exe"),
        Engine::Cuda => ("gg-worker-gpu-windows.exe", "gg-executor-cuda-windows.exe"),
    };
    let mut files = vec![
        (worker, "gg-worker.exe"),
        (executor, "gg-executor.exe"),
        ("gg-desk-windows.exe", "gg-desk.exe"),
        ("worker.toml.template", "worker.toml.template"),
    ];
    if engine == Engine::Cuda {
        files.push(("libtorch-windows.sha256", "libtorch-windows.sha256"));
    }
    files
        .into_iter()
        .map(|(source, target)| {
            let found: Vec<&(String, String)> =
                signed.files.iter().filter(|(n, _)| n == source).collect();
            match found[..] {
                [(_, digest)] => Ok(Needed {
                    source: source.to_owned(),
                    target: target.to_owned(),
                    digest: digest.clone(),
                }),
                _ => Err(format!(
                    "{source} is not listed exactly once in the signed {CHECKSUMS_FILE}"
                )),
            }
        })
        .collect()
}

/// The LibTorch archive the release pins: where PyTorch serves it, its size and digest.
#[derive(Debug, PartialEq, Eq)]
pub struct LibTorchPin {
    pub url: String,
    pub size: u64,
    pub digest: String,
    pub name: String,
}

/// Read the signed pin (`libtorch-windows.sha256`): only an official PyTorch address, a zip archive
/// name, a size and a SHA-256 are accepted.
pub fn libtorch_pin(text: &str) -> Result<LibTorchPin, String> {
    let comment = |k: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(&format!("# {k}: ")))
            .map(str::trim)
    };
    let url = comment("url").ok_or("no url in the LibTorch pin")?;
    let ok_url = url
        .strip_prefix("https://download.pytorch.org/libtorch/")
        .is_some_and(|rest| {
            rest.ends_with(".zip")
                && rest
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "./%+_-".contains(c))
        });
    if !ok_url {
        return Err(format!(
            "unexpected LibTorch address in the signed pin: {url}"
        ));
    }
    let size = comment("size")
        .and_then(|v| v.parse::<u64>().ok())
        .ok_or("malformed size in the LibTorch pin")?;
    let (digest, name) = text
        .lines()
        .find(|l| !l.starts_with('#') && !l.trim().is_empty())
        .and_then(|l| l.split_once("  "))
        .ok_or("no digest in the LibTorch pin")?;
    let ok_name = name.ends_with(".zip")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || ".+_-".contains(c));
    if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) || !ok_name {
        return Err("malformed LibTorch pin".into());
    }
    Ok(LibTorchPin {
        url: url.to_owned(),
        size,
        digest: digest.to_ascii_lowercase(),
        name: name.to_owned(),
    })
}

/// Whether `bytes` have the signed digest.
pub fn matches(bytes: &[u8], digest: &str) -> bool {
    hex::encode(Sha256::digest(bytes)) == digest
}

/// A TOML basic string (backslashes of Windows paths, quotes).
fn toml_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Set `key = value` in a configuration: the existing line is replaced, or one is added at the
/// top (before any table, where top-level keys belong).
pub fn set_config(text: &str, key: &str, value: &str) -> String {
    let is_key = |l: &str| {
        l.split_once('=')
            .is_some_and(|(k, _)| k.trim() == key && !l.trim_start().starts_with('#'))
    };
    if text.lines().any(is_key) {
        text.lines()
            .map(|l| {
                if is_key(l) {
                    format!("{key} = {value}")
                } else {
                    l.to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n"
    } else {
        format!("{key} = {value}\n{text}")
    }
}

/// One worker's configuration on Windows, from the signed template: its data directory, name,
/// engine and enrollment code, the Windows sandbox; `card`: the further cards' number (they
/// answer the application on any free port).
pub fn worker_config(
    template: &str,
    server_url: &str,
    data_dir: &Path,
    name: &str,
    engine: &str,
    enroll: &str,
    card: Option<u32>,
) -> Result<String, String> {
    // The template's markers sit inside quotes: fill them with the escaped text, quotes removed.
    let inner = |s: &str| {
        let q = toml_str(s);
        q[1..q.len() - 1].to_owned()
    };
    let data = data_dir.to_string_lossy().replace('\\', "/");
    let mut text = template
        .replace("@SERVER_URL@", &inner(server_url))
        .replace("@DATA_DIR@", &inner(&data))
        .replace("@DISPLAY_NAME@", &inner(name))
        .replace("@ENGINE@", &inner(engine))
        .replace("@ENROLLMENT_TOKEN@", &inner(enroll));
    if text.contains('@')
        && text
            .split('@')
            .skip(1)
            .step_by(2)
            .any(|m| !m.is_empty() && m.chars().all(|c| c.is_ascii_uppercase() || c == '_'))
    {
        return Err("template markers left in the configuration".into());
    }
    text = set_config(&text, "sandbox", "\"appcontainer\"");
    if card.is_some() {
        text = set_config(&text, "control_listen", "\"127.0.0.1:0\"");
    }
    toml::from_str::<toml::Value>(&text).map_err(|e| format!("configuration: {e}"))?;
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gg_protocol::keys::SecretKey;
    use gg_protocol::release::{allowed_signers_line, sshsig_sign};

    fn sums(network: &str) -> String {
        let d = |s: &[u8]| hex::encode(Sha256::digest(s));
        format!(
            "# network: {network}\n# version: 0.2.0\n{}  gg-worker-windows.exe\n{}  gg-executor-windows.exe\n{}  gg-desk-windows.exe\n{}  worker.toml.template\n{}  gg-worker\n",
            d(b"worker"),
            d(b"executor"),
            d(b"desk"),
            d(b"template"),
            d(b"linux worker"),
        )
    }

    #[test]
    fn a_signed_list_is_read_only_with_the_trusted_key() {
        let key = SecretKey::generate().unwrap();
        let line = allowed_signers_line(&key.public_key());
        let trusted = trusted_key(&line).unwrap();
        let text = sums(NETWORK);
        let sig = sshsig_sign(&key, SSHSIG_NAMESPACE, text.as_bytes());
        let signed = read_signed(&text, &sig, &trusted, NETWORK).unwrap();
        assert_eq!(signed.version, "0.2.0");
        let needed = needed_windows(&signed, Engine::Cpu).unwrap();
        assert_eq!(needed[0].target, "gg-worker.exe");
        assert!(matches(b"worker", &needed[0].digest));
        assert!(!matches(b"tampered", &needed[0].digest));
        // Changed after signing, another key, another network: refused.
        assert!(read_signed(&text.replace("0.2.0", "0.2.1"), &sig, &trusted, NETWORK).is_err());
        let other = SecretKey::generate().unwrap();
        let other_sig = sshsig_sign(&other, SSHSIG_NAMESPACE, text.as_bytes());
        assert!(read_signed(&text, &other_sig, &trusted, NETWORK).is_err());
        assert!(read_signed(&text, &sig, &trusted, "gg-other").is_err());
        // The GPU build is not in this release.
        assert!(needed_windows(&signed, Engine::Gpu).is_err());
    }

    #[test]
    fn only_an_official_libtorch_pin_is_read() {
        let pin = "# url: https://download.pytorch.org/libtorch/cu128/libtorch-win-shared-with-deps-2.9.0%2Bcu128.zip\n# size: 2913799572\ndaca8ba2ff7a71a8a1c090d6ec5ce883353544abdafa55d56a778ab915e30acd  libtorch-win-shared-with-deps-2.9.0+cu128.zip\n";
        let p = libtorch_pin(pin).unwrap();
        assert_eq!(p.size, 2913799572);
        assert_eq!(p.name, "libtorch-win-shared-with-deps-2.9.0+cu128.zip");
        assert!(libtorch_pin(&pin.replace("download.pytorch.org", "evil.example")).is_err());
        assert!(libtorch_pin(&pin.replace(".zip\n# size", ".exe\n# size")).is_err());
        assert!(libtorch_pin(&pin.replace("2913799572", "x")).is_err());
        assert!(libtorch_pin(&pin.replace("daca8ba2", "zzzz8ba2")).is_err());
        // The CUDA engine also needs the pin from the signed list.
        let signed = Signed {
            version: "1".into(),
            network: NETWORK.into(),
            files: vec![
                ("gg-worker-gpu-windows.exe".into(), "0".repeat(64)),
                ("gg-executor-cuda-windows.exe".into(), "1".repeat(64)),
                ("gg-desk-windows.exe".into(), "2".repeat(64)),
                ("worker.toml.template".into(), "3".repeat(64)),
            ],
        };
        assert!(needed_windows(&signed, Engine::Cuda).is_err());
    }

    #[test]
    fn a_key_for_another_use_is_not_trusted() {
        let key = SecretKey::generate().unwrap();
        let line = allowed_signers_line(&key.public_key());
        assert!(trusted_key(&line.replace("gg-release", "file")).is_err());
        assert!(trusted_key(&line.replace("release@", "someone@")).is_err());
        assert!(trusted_key("").is_err());
    }

    #[test]
    fn the_configuration_holds_windows_paths_and_the_windows_sandbox() {
        let template = "server_url = \"@SERVER_URL@\"\nkey_path = \"@DATA_DIR@/worker.key\"\ndisplay_name = \"@DISPLAY_NAME@\"\nenrollment_token = \"@ENROLLMENT_TOKEN@\"\nengine = \"@ENGINE@\"\nsandbox = \"bwrap\"\n\n[p2p]\nenabled = true\n";
        let text = worker_config(
            template,
            "https://grid.gpugoblin.com",
            Path::new(r"C:\Users\Ada\AppData\Local\GPU Goblin\gpu1"),
            "ADA \"PC\" gpu1",
            "gpu:1",
            "gge_abcdefghijklmnop",
            Some(1),
        )
        .unwrap();
        let v: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(v["sandbox"].as_str(), Some("appcontainer"));
        assert_eq!(v["control_listen"].as_str(), Some("127.0.0.1:0"));
        assert_eq!(
            v["key_path"].as_str(),
            Some("C:/Users/Ada/AppData/Local/GPU Goblin/gpu1/worker.key")
        );
        assert_eq!(v["display_name"].as_str(), Some("ADA \"PC\" gpu1"));
        assert_eq!(v["p2p"]["enabled"].as_bool(), Some(true));
        assert!(
            worker_config(
                "x = \"@UNKNOWN_MARK@\"",
                "",
                Path::new("d"),
                "",
                "",
                "",
                None
            )
            .is_err()
        );
    }
}
