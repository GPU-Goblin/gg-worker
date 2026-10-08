//! On Windows, the application carries its icon (Start menu, list of installed apps, taskbar,
//! Explorer) and its manifest (run as the user, current message boxes, sharp at any scale), as a
//! resource file written here directly: no resource compiler needed.

use std::path::Path;

/// One resource entry of a `.res` file: a header with numeric type and name, then the data,
/// aligned on four bytes.
fn entry(out: &mut Vec<u8>, kind: u16, name: u16, flags: u16, data: &[u8]) {
    let mut header = Vec::with_capacity(32);
    header.extend_from_slice(&(data.len() as u32).to_le_bytes()); // DataSize
    header.extend_from_slice(&32u32.to_le_bytes()); // HeaderSize
    header.extend_from_slice(&[0xff, 0xff]);
    header.extend_from_slice(&kind.to_le_bytes()); // TYPE
    header.extend_from_slice(&[0xff, 0xff]);
    header.extend_from_slice(&name.to_le_bytes()); // NAME
    header.extend_from_slice(&0u32.to_le_bytes()); // DataVersion
    header.extend_from_slice(&flags.to_le_bytes()); // MemoryFlags
    header.extend_from_slice(&0x0409u16.to_le_bytes()); // LanguageId
    header.extend_from_slice(&0u32.to_le_bytes()); // Version
    header.extend_from_slice(&0u32.to_le_bytes()); // Characteristics
    out.extend_from_slice(&header);
    out.extend_from_slice(data);
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
}

fn main() {
    println!("cargo:rerun-if-changed=assets/gpu-goblin.ico");
    println!("cargo:rerun-if-changed=assets/gg-desk.manifest");
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let ico = std::fs::read("assets/gpu-goblin.ico").expect("assets/gpu-goblin.ico");
    let manifest = std::fs::read("assets/gg-desk.manifest").expect("assets/gg-desk.manifest");
    let count = u16::from_le_bytes([ico[4], ico[5]]) as usize;
    let mut res = Vec::new();
    // The empty first entry every .res file starts with.
    res.extend_from_slice(&[
        0, 0, 0, 0, 0x20, 0, 0, 0, 0xff, 0xff, 0, 0, 0xff, 0xff, 0, 0,
    ]);
    res.extend_from_slice(&[0; 16]);
    // RT_ICON (3): each image; RT_GROUP_ICON (14): their directory, ids instead of offsets.
    let mut group = vec![0, 0, 1, 0];
    group.extend_from_slice(&(count as u16).to_le_bytes());
    for i in 0..count {
        let e = &ico[6 + 16 * i..6 + 16 * (i + 1)];
        let size = u32::from_le_bytes([e[8], e[9], e[10], e[11]]) as usize;
        let offset = u32::from_le_bytes([e[12], e[13], e[14], e[15]]) as usize;
        let id = (i + 1) as u16;
        entry(&mut res, 3, id, 0x1010, &ico[offset..offset + size]);
        group.extend_from_slice(&e[..12]);
        group.extend_from_slice(&id.to_le_bytes());
    }
    entry(&mut res, 14, 1, 0x1030, &group);
    // RT_MANIFEST (24), id 1: the manifest of an executable.
    entry(&mut res, 24, 1, 0x1030, &manifest);
    let out = Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR")).join("gg-desk.res");
    std::fs::write(&out, res).expect("write gg-desk.res");
    println!("cargo:rustc-link-arg-bins={}", out.display());
}
