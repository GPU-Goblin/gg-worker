//! Write `assets/gpu-goblin.ico`, the Windows icon of the programs (16 to 256 pixels), from the
//! mark the application draws. Run it again after changing the mark:
//!
//!     cargo run -p gg-desk --example make_ico
//!
//! `build.rs` embeds the file in the Windows executables.

use gg_desk::{logo, theme::Palette};

fn png(size: u32) -> Vec<u8> {
    let p = Palette::DARK;
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, size, size);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut w = enc.write_header().expect("png header");
    w.write_image_data(&logo::rgba(size, p.accent, p.accent2, None))
        .expect("png data");
    w.finish().expect("png end");
    out
}

fn main() {
    let sizes = [16u32, 20, 24, 32, 40, 48, 64, 256];
    let images: Vec<Vec<u8>> = sizes.iter().map(|&s| png(s)).collect();
    // ICONDIR, then one ICONDIRENTRY per image (PNG images, Windows Vista and later).
    let mut ico = Vec::new();
    ico.extend_from_slice(&[0, 0, 1, 0]);
    ico.extend_from_slice(&(sizes.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * sizes.len() as u32;
    for (s, img) in sizes.iter().zip(&images) {
        let d = if *s >= 256 { 0 } else { *s as u8 };
        ico.extend_from_slice(&[d, d, 0, 0]);
        ico.extend_from_slice(&1u16.to_le_bytes());
        ico.extend_from_slice(&32u16.to_le_bytes());
        ico.extend_from_slice(&(img.len() as u32).to_le_bytes());
        ico.extend_from_slice(&offset.to_le_bytes());
        offset += img.len() as u32;
    }
    for img in &images {
        ico.extend_from_slice(img);
    }
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/gpu-goblin.ico");
    std::fs::write(path, &ico).expect("write the icon");
    println!("{path}: {} bytes, sizes {sizes:?}", ico.len());
}
