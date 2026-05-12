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
            // Per-row x-range derivation. Trapezoid sides are line
            // segments, so x is monotonic in y along each side; the
            // row's true x extent is bounded by sampling both edges
            // at the visible portion of the row. Clamping `y_top` /
            // `y_bot` to `[top, bot]` avoids extrapolating the side
            // lines outside the trapezoid (which would falsely widen
            // the range for the partial first/last row).
            let pyf = py as f32;
            let y_top = pyf.max(top);
            let y_bot = (pyf + 1.0).min(bot);
            let lx_top = left.x_at(y_top);
            let lx_bot = left.x_at(y_bot);
            let rx_top = right.x_at(y_top);
            let rx_bot = right.x_at(y_bot);
            let row_min = lx_top.min(lx_bot).min(rx_top).min(rx_bot);
            let row_max = lx_top.max(lx_bot).max(rx_top).max(rx_bot);
            let row_x0 = (row_min.floor() as i32).max(bbox_x);
            let row_x1 = (row_max.ceil() as i32).min(bbox_x + width as i32);
            if row_x1 <= row_x0 {
                continue;
            }
            for px in row_x0..row_x1 {
                let mut hits = 0_i32;
                for sy in 0..SUBSAMPLES_PER_AXIS {
                    let y = pyf + (sy as f32 + 0.5) / SUBSAMPLES_PER_AXIS as f32;
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

    /// Standalone perf probe for `rasterize_trapezoids`. Ignored by
    /// default — run explicitly with `--release` to time the hot
    /// path without the rest of the workspace:
    ///
    ///   cargo test -p yserver --release --lib \
    ///       kms::vk::ops::traps::tests::bench -- --ignored --nocapture
    ///
    /// Simulates a single GTK-button hover frame: ~30 thin trapezoids
    /// inside a 200×40 bbox (rounded-corner AA + emboss edges). Prints
    /// total time and per-call cost so we can compare against the
    /// pre-fix baseline if the regression ever comes back.
    #[test]
    #[ignore]
    fn bench_button_hover_workload() {
        use std::time::Instant;

        // Mimic GTK's rounded-corner AA: a stack of 1-pixel-tall
        // trapezoids whose left/right edges trace a quarter-circle.
        // Real GTK frames send dozens of these per hover update.
        let mut traps = Vec::new();
        for i in 0..32 {
            let y = i as f32 * 1.25;
            let r = 8.0_f32 - (i as f32 * 0.25);
            traps.push(Trapezoid {
                top: fx(y),
                bottom: fx(y + 1.25),
                left_p1: (fx(8.0 - r), fx(y)),
                left_p2: (fx(8.0 - r * 0.9), fx(y + 1.25)),
                right_p1: (fx(192.0 + r), fx(y)),
                right_p2: (fx(192.0 + r * 0.9), fx(y + 1.25)),
            });
        }

        const ITERS: u32 = 1000;
        // Warmup.
        for _ in 0..10 {
            std::hint::black_box(rasterize_trapezoids(&traps, 0, 0, 200, 40));
        }
        let t0 = Instant::now();
        for _ in 0..ITERS {
            std::hint::black_box(rasterize_trapezoids(&traps, 0, 0, 200, 40));
        }
        let dt = t0.elapsed();
        let per_call_us = dt.as_secs_f64() * 1e6 / f64::from(ITERS);
        eprintln!(
            "rasterize_trapezoids: {ITERS} calls in {:?}  -> {per_call_us:.2} µs/call \
             ({} trapezoids × 200×40 bbox)",
            dt,
            traps.len()
        );
    }

    #[test]
    fn thin_slanted_trapezoid_in_wide_bbox_only_paints_slant() {
        // 2-pixel-wide parallelogram going diagonally from (10,0) at
        // the top to (14,8) at the bottom, inside a 64×8 bbox. Pixels
        // outside the slant must stay zero — historically a row-wide
        // inner loop with a broken cheap-clip burned 4×4 samples on
        // every bbox pixel, hammering CPU on GTK hover effects.
        let trap = Trapezoid {
            top: fx(0.0),
            bottom: fx(8.0),
            left_p1: (fx(10.0), fx(0.0)),
            left_p2: (fx(14.0), fx(8.0)),
            right_p1: (fx(12.0), fx(0.0)),
            right_p2: (fx(16.0), fx(8.0)),
        };
        let mask = rasterize_trapezoids(&[trap], 0, 0, 64, 8);
        // Pixel near the bottom-right of the slant must have ink;
        // pixels at the far left and far right of the bbox must be
        // untouched.
        let row3 = 3 * 64;
        assert!(
            mask[row3 + 12] > 0,
            "expected ink near the slant midpoint at (12,3)"
        );
        assert_eq!(mask[row3 + 0], 0, "left edge of bbox should be empty");
        assert_eq!(mask[row3 + 63], 0, "right edge of bbox should be empty");
        // Bottom row: slant has moved right, columns 0..10 should be empty.
        let row7 = 7 * 64;
        for px in 0..10 {
            assert_eq!(mask[row7 + px], 0, "row 7 col {px} should be empty");
        }
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
