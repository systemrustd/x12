//! CPU rasterisation for RENDER `Trapezoids` and `Triangles` into an
//! `R8_UNORM` (A8) coverage buffer (sub-phase 4.1.4.7).
//!
//! The Vulkan pipeline doesn't have a stencil-AA path yet (would
//! land in 4.1.4.7's `poly_edge` shader work in the design spec).
//! For now, rasterise CPU-side then upload to
//! [`MaskScratch`](super::super::mask_scratch::MaskScratch) and run
//! the existing 4.1.4.6 Composite path with the scratch as the
//! mask source.
//!
//! Anti-aliasing: 4×4 super-sample per pixel — 16 sub-samples per
//! pixel, the result is the in/out count divided by 16. Matches
//! pixman's default trapezoid AA quality at typical sizes; not
//! pixel-identical to pixman but well within the design's
//! "test suites are the arbiter, not pixel equality" rule
//! (parent plan §risks).
//!
//! Coordinate convention: input rationals are X11 RENDER fixed-point
//! 16.16 (i32). Output buffer is row-major over the bbox the caller
//! supplied; pixel `(x, y)` lives at `(y - bbox_y) * width +
//! (x - bbox_x)`.

const SUBSAMPLES_PER_AXIS: i32 = 4;
const SUBSAMPLES_TOTAL: i32 = SUBSAMPLES_PER_AXIS * SUBSAMPLES_PER_AXIS;

/// 16.16 → f32. Saturates if the input is wildly out of range; doesn't
/// matter for trapezoid rasterisation since we clip to the bbox.
fn fixed_to_f32(v: i32) -> f32 {
    (v as f32) / 65536.0
}

/// One trapezoid edge. Stores the two endpoints in float; `x_at(y)`
/// linearly interpolates the x position. For degenerate edges (p1.y
/// == p2.y) returns the midpoint x at any y query.
#[derive(Debug, Clone, Copy)]
struct Edge {
    p1: (f32, f32),
    p2: (f32, f32),
}

impl Edge {
    fn x_at(self, y: f32) -> f32 {
        let dy = self.p2.1 - self.p1.1;
        if dy.abs() < f32::EPSILON {
            (self.p1.0 + self.p2.0) * 0.5
        } else {
            let t = (y - self.p1.1) / dy;
            self.p1.0 + t * (self.p2.0 - self.p1.0)
        }
    }
}

/// One RENDER trapezoid (matches `pixman_trapezoid_t` field-for-field
/// after fixed-point conversion).
#[derive(Debug, Clone, Copy)]
pub struct Trapezoid {
    pub top: i32,    // 16.16 fixed-point
    pub bottom: i32, // 16.16 fixed-point
    pub left_p1: (i32, i32),
    pub left_p2: (i32, i32),
    pub right_p1: (i32, i32),
    pub right_p2: (i32, i32),
}

/// One RENDER triangle (matches `pixman_triangle_t`).
#[derive(Debug, Clone, Copy)]
pub struct Triangle {
    pub p1: (i32, i32),
    pub p2: (i32, i32),
    pub p3: (i32, i32),
}

/// Bounding box (integer pixels). Inclusive lower bound, exclusive
/// upper. Returns `None` if any trapezoid is degenerate / empty —
/// caller treats that as "no draw".
pub fn trapezoid_bbox(traps: &[Trapezoid]) -> Option<(i32, i32, i32, i32)> {
    if traps.is_empty() {
        return None;
    }
    let mut x0 = f32::INFINITY;
    let mut y0 = f32::INFINITY;
    let mut x1 = f32::NEG_INFINITY;
    let mut y1 = f32::NEG_INFINITY;
    for t in traps {
        let top = fixed_to_f32(t.top);
        let bot = fixed_to_f32(t.bottom);
        if bot <= top {
            continue;
        }
        let lp1 = (fixed_to_f32(t.left_p1.0), fixed_to_f32(t.left_p1.1));
        let lp2 = (fixed_to_f32(t.left_p2.0), fixed_to_f32(t.left_p2.1));
        let rp1 = (fixed_to_f32(t.right_p1.0), fixed_to_f32(t.right_p1.1));
        let rp2 = (fixed_to_f32(t.right_p2.0), fixed_to_f32(t.right_p2.1));
        for &p in &[lp1, lp2, rp1, rp2] {
            x0 = x0.min(p.0);
            x1 = x1.max(p.0);
        }
        y0 = y0.min(top);
        y1 = y1.max(bot);
    }
    if !x0.is_finite() || x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some((
        x0.floor() as i32,
        y0.floor() as i32,
        x1.ceil() as i32,
        y1.ceil() as i32,
    ))
}

pub fn triangle_bbox(tris: &[Triangle]) -> Option<(i32, i32, i32, i32)> {
    if tris.is_empty() {
        return None;
    }
    let mut x0 = f32::INFINITY;
    let mut y0 = f32::INFINITY;
    let mut x1 = f32::NEG_INFINITY;
    let mut y1 = f32::NEG_INFINITY;
    for t in tris {
        for p in [t.p1, t.p2, t.p3] {
            let x = fixed_to_f32(p.0);
            let y = fixed_to_f32(p.1);
            x0 = x0.min(x);
            x1 = x1.max(x);
            y0 = y0.min(y);
            y1 = y1.max(y);
        }
    }
    if !x0.is_finite() || x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some((
        x0.floor() as i32,
        y0.floor() as i32,
        x1.ceil() as i32,
        y1.ceil() as i32,
    ))
}

/// Rasterise `traps` into an A8 coverage buffer of size
/// `width * height` rows starting at `(bbox_x, bbox_y)`. Output is
/// the alpha each pixel should contribute (0..=255).
pub fn rasterize_trapezoids(
    traps: &[Trapezoid],
    bbox_x: i32,
    bbox_y: i32,
    width: u32,
    height: u32,
) -> Vec<u8> {
    let mut out = vec![0u8; (width * height) as usize];
    if width == 0 || height == 0 {
        return out;
    }
    for trap in traps {
        let top = fixed_to_f32(trap.top);
        let bot = fixed_to_f32(trap.bottom);
        if bot <= top {
            continue;
        }
        let left = Edge {
            p1: (fixed_to_f32(trap.left_p1.0), fixed_to_f32(trap.left_p1.1)),
            p2: (fixed_to_f32(trap.left_p2.0), fixed_to_f32(trap.left_p2.1)),
        };
        let right = Edge {
            p1: (fixed_to_f32(trap.right_p1.0), fixed_to_f32(trap.right_p1.1)),
            p2: (fixed_to_f32(trap.right_p2.0), fixed_to_f32(trap.right_p2.1)),
        };

        let py0 = (top.floor() as i32).max(bbox_y);
        let py1 = (bot.ceil() as i32).min(bbox_y + height as i32);
        for py in py0..py1 {
            let row = (py - bbox_y) as usize;
            for px in bbox_x..bbox_x + width as i32 {
                // Quick clip: skip pixels that can't possibly intersect.
                let left_px = left.x_at(py as f32 + 0.5);
                let right_px = right.x_at(py as f32 + 0.5);
                let pxf = px as f32;
                if (pxf + 1.0 <= left_px.min(right_px).floor()
                    || pxf >= left_px.max(right_px).ceil())
                    && (pxf + 1.0 <= top || pxf >= bot)
                {
                    continue;
                }
                let mut hits = 0_i32;
                for sy in 0..SUBSAMPLES_PER_AXIS {
                    let y = py as f32 + (sy as f32 + 0.5) / SUBSAMPLES_PER_AXIS as f32;
                    if y < top || y >= bot {
                        continue;
                    }
                    let lx = left.x_at(y);
                    let rx = right.x_at(y);
                    if rx <= lx {
                        continue;
                    }
                    for sx in 0..SUBSAMPLES_PER_AXIS {
                        let x = px as f32 + (sx as f32 + 0.5) / SUBSAMPLES_PER_AXIS as f32;
                        if x >= lx && x < rx {
                            hits += 1;
                        }
                    }
                }
                if hits == 0 {
                    continue;
                }
                let col = (px - bbox_x) as usize;
                let cov = (hits * 255 / SUBSAMPLES_TOTAL) as u8;
                let idx = row * (width as usize) + col;
                // Combine multiple trapezoids with sat-add — RENDER's
                // trapezoid mask is meant to be the union, and pixman
                // does the same.
                out[idx] = out[idx].saturating_add(cov);
            }
        }
    }
    out
}

/// Rasterise `tris` into an A8 coverage buffer of size
/// `width * height` rows starting at `(bbox_x, bbox_y)`.
pub fn rasterize_triangles(
    tris: &[Triangle],
    bbox_x: i32,
    bbox_y: i32,
    width: u32,
    height: u32,
) -> Vec<u8> {
    let mut out = vec![0u8; (width * height) as usize];
    if width == 0 || height == 0 {
        return out;
    }
    for tri in tris {
        let p1 = (fixed_to_f32(tri.p1.0), fixed_to_f32(tri.p1.1));
        let p2 = (fixed_to_f32(tri.p2.0), fixed_to_f32(tri.p2.1));
        let p3 = (fixed_to_f32(tri.p3.0), fixed_to_f32(tri.p3.1));
        let y0 = p1.1.min(p2.1).min(p3.1);
        let y1 = p1.1.max(p2.1).max(p3.1);
        let x0 = p1.0.min(p2.0).min(p3.0);
        let x1 = p1.0.max(p2.0).max(p3.0);
        if y1 <= y0 || x1 <= x0 {
            continue;
        }
        let py0 = (y0.floor() as i32).max(bbox_y);
        let py1 = (y1.ceil() as i32).min(bbox_y + height as i32);
        let px0 = (x0.floor() as i32).max(bbox_x);
        let px1 = (x1.ceil() as i32).min(bbox_x + width as i32);
        for py in py0..py1 {
            let row = (py - bbox_y) as usize;
            for px in px0..px1 {
                let mut hits = 0_i32;
                for sy in 0..SUBSAMPLES_PER_AXIS {
                    let y = py as f32 + (sy as f32 + 0.5) / SUBSAMPLES_PER_AXIS as f32;
                    for sx in 0..SUBSAMPLES_PER_AXIS {
                        let x = px as f32 + (sx as f32 + 0.5) / SUBSAMPLES_PER_AXIS as f32;
                        if point_in_triangle((x, y), p1, p2, p3) {
                            hits += 1;
                        }
                    }
                }
                if hits == 0 {
                    continue;
                }
                let col = (px - bbox_x) as usize;
                let cov = (hits * 255 / SUBSAMPLES_TOTAL) as u8;
                let idx = row * (width as usize) + col;
                out[idx] = out[idx].saturating_add(cov);
            }
        }
    }
    out
}

/// Standard barycentric point-in-triangle. Considers the triangle a
/// closed shape (boundary inclusive). Sign-agnostic — handles both
/// CW and CCW winding.
fn point_in_triangle(p: (f32, f32), a: (f32, f32), b: (f32, f32), c: (f32, f32)) -> bool {
    let s1 = sign(p, a, b);
    let s2 = sign(p, b, c);
    let s3 = sign(p, c, a);
    let has_neg = s1 < 0.0 || s2 < 0.0 || s3 < 0.0;
    let has_pos = s1 > 0.0 || s2 > 0.0 || s3 > 0.0;
    !(has_neg && has_pos)
}

fn sign(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    (p.0 - b.0) * (a.1 - b.1) - (a.0 - b.0) * (p.1 - b.1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fx(v: f32) -> i32 {
        (v * 65536.0) as i32
    }

    #[test]
    fn rasterize_axis_aligned_trapezoid_fills_box() {
        // A 4×4 axis-aligned rectangle from (1, 1) to (5, 5).
        let trap = Trapezoid {
            top: fx(1.0),
            bottom: fx(5.0),
            left_p1: (fx(1.0), fx(1.0)),
            left_p2: (fx(1.0), fx(5.0)),
            right_p1: (fx(5.0), fx(1.0)),
            right_p2: (fx(5.0), fx(5.0)),
        };
        let mask = rasterize_trapezoids(&[trap], 0, 0, 8, 8);
        // Every pixel in [1, 5)×[1, 5) should be ~0xFF coverage.
        for py in 1..5 {
            for px in 1..5 {
                assert_eq!(mask[(py * 8 + px) as usize], 0xFF, "pixel ({px},{py})");
            }
        }
        // Pixel (0,0) — outside — should be 0.
        assert_eq!(mask[0], 0);
    }

    #[test]
    fn rasterize_triangle_covers_interior_and_excludes_exterior() {
        // Right triangle with corners (0,0), (4,0), (0,4).
        let tri = Triangle {
            p1: (fx(0.0), fx(0.0)),
            p2: (fx(4.0), fx(0.0)),
            p3: (fx(0.0), fx(4.0)),
        };
        let mask = rasterize_triangles(&[tri], 0, 0, 6, 6);
        // Pixel (1, 1) is well inside; pixel (5, 5) is outside.
        assert!(mask[1 * 6 + 1] > 200, "expected high coverage near origin");
        assert_eq!(mask[5 * 6 + 5], 0);
    }
}
