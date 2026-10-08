//! The mark: a goblin's head as a chip (pointed ears, glowing eyes, the chip's pins) on a rounded
//! square in the brand's gradient. Drawn by the program, at any size: the window's logo, its icon,
//! the tray's icon with the state's dot.

use eframe::egui::Color32;

const DARK: [f32; 3] = [0.06, 0.07, 0.09];

fn inside_rounded(x: f32, y: f32, (x0, y0, x1, y1): (f32, f32, f32, f32), r: f32) -> bool {
    let cx = x.clamp(x0 + r, x1 - r);
    let cy = y.clamp(y0 + r, y1 - r);
    x >= x0 && x <= x1 && y >= y0 && y <= y1 && (x - cx).powi(2) + (y - cy).powi(2) <= r * r
}

fn inside_triangle(p: (f32, f32), a: (f32, f32), b: (f32, f32), c: (f32, f32)) -> bool {
    let s = |p1: (f32, f32), p2: (f32, f32), p3: (f32, f32)| {
        (p1.0 - p3.0) * (p2.1 - p3.1) - (p2.0 - p3.0) * (p1.1 - p3.1)
    };
    let (d1, d2, d3) = (s(p, a, b), s(p, b, c), s(p, c, a));
    let neg = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
    let pos = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
    !(neg && pos)
}

/// The colour of the mark at `(x, y)` in the unit square, `None` outside it.
fn sample(x: f32, y: f32, top: [f32; 3], bottom: [f32; 3], eye: [f32; 3]) -> Option<[f32; 3]> {
    if !inside_rounded(x, y, (0.0, 0.0, 1.0, 1.0), 0.24) {
        return None;
    }
    let eye_at = |ex: f32| (x - ex).powi(2) + (y - 0.555).powi(2) <= 0.062f32.powi(2);
    if eye_at(0.39) || eye_at(0.61) {
        return Some(eye);
    }
    let head = inside_rounded(x, y, (0.25, 0.33, 0.75, 0.79), 0.11);
    let ears = inside_triangle((x, y), (0.29, 0.41), (0.29, 0.60), (0.07, 0.25))
        || inside_triangle((x, y), (0.71, 0.41), (0.71, 0.60), (0.93, 0.25));
    let pins = [0.355, 0.47, 0.585]
        .iter()
        .any(|px| inside_rounded(x, y, (*px, 0.76, px + 0.06, 0.87), 0.02));
    if head || ears || pins {
        return Some(DARK);
    }
    let t = ((x + y) / 2.0).clamp(0.0, 1.0);
    Some([
        top[0] + (bottom[0] - top[0]) * t,
        top[1] + (bottom[1] - top[1]) * t,
        top[2] + (bottom[2] - top[2]) * t,
    ])
}

fn f(c: Color32) -> [f32; 3] {
    [
        c.r() as f32 / 255.0,
        c.g() as f32 / 255.0,
        c.b() as f32 / 255.0,
    ]
}

/// The mark, `size` × `size` RGBA (straight alpha), anti-aliased; with a dot of `status` at the
/// bottom right when given.
pub fn rgba(size: u32, accent: Color32, accent2: Color32, status: Option<Color32>) -> Vec<u8> {
    const SS: u32 = 4;
    let (top, bottom) = (f(accent), f(accent2));
    let eye = [0.80, 1.0, 0.88];
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for py in 0..size {
        for px in 0..size {
            let mut acc = [0.0f32; 4];
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = (px as f32 + (sx as f32 + 0.5) / SS as f32) / size as f32;
                    let y = (py as f32 + (sy as f32 + 0.5) / SS as f32) / size as f32;
                    let dot = status.and_then(|c| {
                        let d = (x - 0.80).powi(2) + (y - 0.80).powi(2);
                        if d <= 0.17f32.powi(2) {
                            Some(if d <= 0.125f32.powi(2) {
                                f(c)
                            } else {
                                [1.0, 1.0, 1.0]
                            })
                        } else {
                            None
                        }
                    });
                    if let Some(c) = dot.or_else(|| sample(x, y, top, bottom, eye)) {
                        acc[0] += c[0];
                        acc[1] += c[1];
                        acc[2] += c[2];
                        acc[3] += 1.0;
                    }
                }
            }
            let n = (SS * SS) as f32;
            if acc[3] == 0.0 {
                out.extend_from_slice(&[0, 0, 0, 0]);
            } else {
                // Straight alpha: the colour of the covered samples, and how many were covered.
                out.extend_from_slice(&[
                    (acc[0] / acc[3] * 255.0) as u8,
                    (acc[1] / acc[3] * 255.0) as u8,
                    (acc[2] / acc[3] * 255.0) as u8,
                    (acc[3] / n * 255.0) as u8,
                ]);
            }
        }
    }
    out
}

/// The same as ARGB32 in network byte order, for the tray.
pub fn argb(size: u32, accent: Color32, accent2: Color32, status: Option<Color32>) -> Vec<u8> {
    rgba(size, accent, accent2, status)
        .chunks_exact(4)
        .flat_map(|p| [p[3], p[0], p[1], p[2]])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mark_has_a_transparent_corner_a_dark_head_and_bright_eyes() {
        let a = Color32::from_rgb(61, 220, 132);
        let b = Color32::from_rgb(43, 199, 217);
        let n = 64u32;
        let px = |img: &[u8], x: u32, y: u32| {
            let i = ((y * n + x) * 4) as usize;
            [img[i], img[i + 1], img[i + 2], img[i + 3]]
        };
        let img = rgba(n, a, b, None);
        assert_eq!(img.len(), (n * n * 4) as usize);
        assert_eq!(px(&img, 0, 0)[3], 0, "the corner is rounded off");
        let head = px(&img, 32, 43);
        assert!(head[0] < 40 && head[3] == 255, "{head:?}");
        let eye = px(&img, 25, 35);
        assert!(eye[1] > 200, "{eye:?}");
        let with_dot = rgba(n, a, b, Some(Color32::from_rgb(240, 85, 85)));
        let dot = px(&with_dot, 51, 51);
        assert!(dot[0] > 200 && dot[1] < 120, "{dot:?}");
        assert_eq!(argb(n, a, b, None)[..4], [0, 0, 0, 0]);
    }
}
