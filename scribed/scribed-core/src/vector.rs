//! Vectorizer (DESIGN.md §6.2): turns a generated line-art raster into pen
//! strokes. Fully offline / device-independent — this is milestone M2.
//! Pipeline: threshold -> Zhang-Suen thinning -> skeleton tracing ->
//! Douglas-Peucker simplification -> tonal hatching -> stroke budget.

use crate::geometry::Stroke;
use image::GrayImage;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pt {
    pub x: f32,
    pub y: f32,
}

pub type Polyline = Vec<Pt>;

/// Binarize a grayscale image: true = ink (darker than `level`).
pub fn threshold(img: &GrayImage, level: u8) -> (u32, u32, Vec<bool>) {
    let (w, h) = img.dimensions();
    let mut mask = Vec::with_capacity((w * h) as usize);
    for p in img.pixels() {
        mask.push(p.0[0] < level);
    }
    (w, h, mask)
}

#[inline]
fn idx(x: i32, y: i32, w: i32, h: i32, mask: &[bool]) -> bool {
    if x < 0 || y < 0 || x >= w || y >= h {
        false
    } else {
        mask[(y * w + x) as usize]
    }
}

/// Zhang-Suen thinning: iteratively erodes the ink mask to a 1px-wide
/// skeleton while preserving connectivity. Standard two-subiteration form.
pub fn thin_zhang_suen(width: u32, height: u32, mask: &[bool]) -> Vec<bool> {
    let (w, h) = (width as i32, height as i32);
    let mut m = mask.to_vec();
    loop {
        let mut changed = false;
        for step in 0..2 {
            let mut to_clear = Vec::new();
            for y in 0..h {
                for x in 0..w {
                    if !idx(x, y, w, h, &m) {
                        continue;
                    }
                    // 8-neighbors in clockwise order starting north.
                    let p = [
                        idx(x, y - 1, w, h, &m),
                        idx(x + 1, y - 1, w, h, &m),
                        idx(x + 1, y, w, h, &m),
                        idx(x + 1, y + 1, w, h, &m),
                        idx(x, y + 1, w, h, &m),
                        idx(x - 1, y + 1, w, h, &m),
                        idx(x - 1, y, w, h, &m),
                        idx(x - 1, y - 1, w, h, &m),
                    ];
                    let b: u32 = p.iter().filter(|&&v| v).count() as u32;
                    if !(2..=6).contains(&b) {
                        continue;
                    }
                    let mut a = 0;
                    for i in 0..8 {
                        if !p[i] && p[(i + 1) % 8] {
                            a += 1;
                        }
                    }
                    if a != 1 {
                        continue;
                    }
                    let cond = if step == 0 {
                        !(p[0] && p[2] && p[4]) && !(p[2] && p[4] && p[6])
                    } else {
                        !(p[0] && p[2] && p[6]) && !(p[0] && p[4] && p[6])
                    };
                    if cond {
                        to_clear.push((x, y));
                    }
                }
            }
            if !to_clear.is_empty() {
                changed = true;
                for (x, y) in to_clear {
                    m[(y * w + x) as usize] = false;
                }
            }
        }
        if !changed {
            break;
        }
    }
    m
}

fn neighbors8(x: i32, y: i32) -> [(i32, i32); 8] {
    [
        (x, y - 1),
        (x + 1, y - 1),
        (x + 1, y),
        (x + 1, y + 1),
        (x, y + 1),
        (x - 1, y + 1),
        (x - 1, y),
        (x - 1, y - 1),
    ]
}

/// Trace a thinned skeleton into polylines by walking from degree-1
/// endpoints first (so open strokes come out as clean single paths), then
/// mopping up any remaining unvisited pixels (closed loops) afterwards.
pub fn trace_skeleton(width: u32, height: u32, skeleton: &[bool]) -> Vec<Polyline> {
    let (w, h) = (width as i32, height as i32);
    let on = |x: i32, y: i32| -> bool { x >= 0 && y >= 0 && x < w && y < h && skeleton[(y * w + x) as usize] };
    let degree = |x: i32, y: i32| -> u32 {
        neighbors8(x, y).iter().filter(|&&(nx, ny)| on(nx, ny)).count() as u32
    };

    let mut visited = vec![false; (w * h) as usize];
    let mut lines = Vec::new();

    let walk_from = |start: (i32, i32), visited: &mut Vec<bool>| -> Polyline {
        let mut path = vec![Pt { x: start.0 as f32, y: start.1 as f32 }];
        visited[(start.1 * w + start.0) as usize] = true;
        let mut cur = start;
        loop {
            let next = neighbors8(cur.0, cur.1)
                .into_iter()
                .find(|&(nx, ny)| on(nx, ny) && !visited[(ny * w + nx) as usize]);
            match next {
                Some((nx, ny)) => {
                    visited[(ny * w + nx) as usize] = true;
                    path.push(Pt { x: nx as f32, y: ny as f32 });
                    cur = (nx, ny);
                }
                None => break,
            }
        }
        path
    };

    // Pass 1: endpoints (degree <= 1) start clean open strokes.
    for y in 0..h {
        for x in 0..w {
            if on(x, y) && !visited[(y * w + x) as usize] && degree(x, y) <= 1 {
                let path = walk_from((x, y), &mut visited);
                if path.len() > 1 {
                    lines.push(path);
                }
            }
        }
    }
    // Pass 2: whatever's left is closed loops or isolated pixels.
    for y in 0..h {
        for x in 0..w {
            if on(x, y) && !visited[(y * w + x) as usize] {
                let path = walk_from((x, y), &mut visited);
                if path.len() > 1 {
                    lines.push(path);
                }
            }
        }
    }
    lines
}

/// Ramer-Douglas-Peucker polyline simplification.
pub fn simplify(points: &[Pt], epsilon: f32) -> Polyline {
    if points.len() < 3 {
        return points.to_vec();
    }
    fn perp_dist(p: Pt, a: Pt, b: Pt) -> f32 {
        let (dx, dy) = (b.x - a.x, b.y - a.y);
        let len2 = dx * dx + dy * dy;
        if len2 == 0.0 {
            return ((p.x - a.x).powi(2) + (p.y - a.y).powi(2)).sqrt();
        }
        let t = ((p.x - a.x) * dx + (p.y - a.y) * dy) / len2;
        let (proj_x, proj_y) = (a.x + t * dx, a.y + t * dy);
        ((p.x - proj_x).powi(2) + (p.y - proj_y).powi(2)).sqrt()
    }
    let mut max_dist = 0.0f32;
    let mut idx_max = 0usize;
    for i in 1..points.len() - 1 {
        let d = perp_dist(points[i], points[0], points[points.len() - 1]);
        if d > max_dist {
            max_dist = d;
            idx_max = i;
        }
    }
    if max_dist > epsilon {
        let mut left = simplify(&points[..=idx_max], epsilon);
        let right = simplify(&points[idx_max..], epsilon);
        left.pop();
        left.extend(right);
        left
    } else {
        vec![points[0], points[points.len() - 1]]
    }
}

/// Parallel hatch lines at `angle_deg`, spaced `spacing` px apart, clipped
/// to pixels where `tone_mask` is true. Used for tonal shading regions.
pub fn hatch_lines(width: u32, height: u32, tone_mask: &[bool], angle_deg: f32, spacing: f32) -> Vec<Polyline> {
    let (w, h) = (width as f32, height as f32);
    let theta = angle_deg.to_radians();
    let (dx, dy) = (theta.cos(), theta.sin());
    // Perpendicular direction steps between hatch lines.
    let (px, py) = (-dy, dx);
    let diag = (w * w + h * h).sqrt();
    let n_lines = (diag / spacing).ceil() as i32;
    let cx = w / 2.0;
    let cy = h / 2.0;

    let in_mask = |x: f32, y: f32| -> bool {
        let (xi, yi) = (x.round() as i32, y.round() as i32);
        if xi < 0 || yi < 0 || xi >= width as i32 || yi >= height as i32 {
            false
        } else {
            tone_mask[(yi * width as i32 + xi) as usize]
        }
    };

    let mut lines = Vec::new();
    for i in -n_lines..=n_lines {
        let ox = cx + px * (i as f32) * spacing;
        let oy = cy + py * (i as f32) * spacing;
        // Walk the full line and collect masked runs as separate segments.
        let steps = diag.ceil() as i32;
        let mut current: Polyline = Vec::new();
        for s in -steps / 2..=steps / 2 {
            let x = ox + dx * s as f32;
            let y = oy + dy * s as f32;
            if in_mask(x, y) {
                current.push(Pt { x, y });
            } else if current.len() > 1 {
                lines.push(std::mem::take(&mut current));
            } else {
                current.clear();
            }
        }
        if current.len() > 1 {
            lines.push(current);
        }
    }
    lines
}

pub fn polylines_to_strokes(lines: &[Polyline], pressure: f32) -> Vec<Stroke> {
    lines
        .iter()
        .filter(|l| l.len() > 1)
        .map(|l| {
            let mut s = Stroke::new();
            for p in l {
                s.push(p.x, p.y, pressure);
            }
            s
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct VectorizeOptions {
    pub threshold_level: u8,
    pub simplify_epsilon: f32,
    pub hatch_spacing_light: f32,
    pub hatch_spacing_dark: f32,
    pub max_draw_seconds: f64,
    pub px_per_second: f64,
    pub outline_pressure: f32,
    pub hatch_pressure: f32,
}

impl Default for VectorizeOptions {
    fn default() -> Self {
        Self {
            threshold_level: 128,
            simplify_epsilon: 0.8,
            hatch_spacing_light: 8.0,
            hatch_spacing_dark: 5.0,
            max_draw_seconds: 240.0,
            px_per_second: 300.0,
            outline_pressure: 0.65,
            hatch_pressure: 0.55,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct VectorizeResult {
    pub strokes: Vec<Stroke>,
    pub estimated_draw_seconds: f64,
    pub degraded_steps: Vec<String>,
}

/// Full pipeline: outline strokes always come first (so an aborted draw
/// still leaves a coherent line drawing), then light hatching, then dark
/// (cross-)hatching, degrading detail if the stroke budget is exceeded.
pub fn vectorize(img: &GrayImage, opts: &VectorizeOptions) -> VectorizeResult {
    let (w, h, mask) = threshold(img, opts.threshold_level);
    let skeleton = thin_zhang_suen(w, h, &mask);
    let raw_lines = trace_skeleton(w, h, &skeleton);
    let simplified: Vec<Polyline> = raw_lines.iter().map(|l| simplify(l, opts.simplify_epsilon)).collect();
    let mut strokes = polylines_to_strokes(&simplified, opts.outline_pressure);

    let mut degraded = Vec::new();
    let outline_time = total_draw_seconds(&strokes, opts.px_per_second);
    if outline_time > opts.max_draw_seconds {
        degraded.push(format!(
            "outline alone ({:.0}s) exceeds budget ({:.0}s); skipping all hatching",
            outline_time, opts.max_draw_seconds
        ));
        return VectorizeResult { strokes, estimated_draw_seconds: outline_time, degraded_steps: degraded };
    }

    // Tone quantization: treat mid-gray (not ink, not white) as "light" tone,
    // and leave dark hatching for a caller-provided dark-tone mask in a
    // fuller implementation. Here we approximate a single light-tone pass
    // over non-ink, non-white pixels as the shading region.
    let tone_mask: Vec<bool> = img.pixels().map(|p| p.0[0] >= opts.threshold_level && p.0[0] < 235).collect();
    let mut spacing = opts.hatch_spacing_light;
    let mut remaining_budget = opts.max_draw_seconds - outline_time;

    let hatch = hatch_lines(w, h, &tone_mask, 45.0, spacing);
    let hatch_strokes = polylines_to_strokes(&hatch, opts.hatch_pressure);
    let hatch_time = total_draw_seconds(&hatch_strokes, opts.px_per_second);
    if hatch_time > remaining_budget {
        // Degrade: widen spacing until it fits, or drop hatching entirely.
        spacing *= 2.0;
        let coarser = hatch_lines(w, h, &tone_mask, 45.0, spacing);
        let coarser_strokes = polylines_to_strokes(&coarser, opts.hatch_pressure);
        let coarser_time = total_draw_seconds(&coarser_strokes, opts.px_per_second);
        if coarser_time <= remaining_budget {
            degraded.push(format!("widened hatch spacing {:.0}->{:.0}px to fit budget", opts.hatch_spacing_light, spacing));
            strokes.extend(coarser_strokes);
            remaining_budget -= coarser_time;
        } else {
            degraded.push("dropped light-tone hatching entirely to fit budget".to_string());
        }
    } else {
        strokes.extend(hatch_strokes);
        remaining_budget -= hatch_time;
    }
    let _ = remaining_budget;

    let total = total_draw_seconds(&strokes, opts.px_per_second);
    VectorizeResult { strokes, estimated_draw_seconds: total, degraded_steps: degraded }
}

pub fn total_draw_seconds(strokes: &[Stroke], px_per_second: f64) -> f64 {
    let total_len: f64 = strokes.iter().map(|s| s.len_px() as f64).sum();
    total_len / px_per_second
}

/// Sobel gradient-magnitude edge mask. Edges give clean *definition* (object
/// and feature boundaries) without skeletonizing solid dark fills into a mess
/// — the failure of the plain `vectorize` on shaded images.
pub fn sobel_edges(img: &GrayImage, threshold: u16) -> (u32, u32, Vec<bool>) {
    let (w, h) = img.dimensions();
    let g = |x: i32, y: i32| -> i32 {
        let xi = x.clamp(0, w as i32 - 1) as u32;
        let yi = y.clamp(0, h as i32 - 1) as u32;
        img.get_pixel(xi, yi).0[0] as i32
    };
    let mut mask = vec![false; (w * h) as usize];
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            let gx = -g(x - 1, y - 1) - 2 * g(x - 1, y) - g(x - 1, y + 1)
                + g(x + 1, y - 1) + 2 * g(x + 1, y) + g(x + 1, y + 1);
            let gy = -g(x - 1, y - 1) - 2 * g(x, y - 1) - g(x + 1, y - 1)
                + g(x - 1, y + 1) + 2 * g(x, y + 1) + g(x + 1, y + 1);
            let mag = ((gx * gx + gy * gy) as f64).sqrt() as u16;
            if mag > threshold {
                mask[(y as u32 * w + x as u32) as usize] = true;
            }
        }
    }
    (w, h, mask)
}

/// A grey-tone band: pixels in `[lo, hi)` get hatched, at `angle_deg` /
/// `spacing`, and if `cross` a second perpendicular pass (darker = denser +
/// cross-hatched → reads darker on the page).
#[derive(Debug, Clone, Copy)]
pub struct ToneBand {
    pub lo: u8,
    pub hi: u8,
    pub angle_deg: f32,
    pub spacing: f32,
    pub cross: bool,
}

#[derive(Debug, Clone)]
pub struct TonalOptions {
    pub edge_threshold: u16,
    pub simplify_epsilon: f32,
    pub bands: Vec<ToneBand>,
    pub outline_pressure: f32,
    pub hatch_pressure: f32,
    pub px_per_second: f64,
}

impl Default for TonalOptions {
    fn default() -> Self {
        Self {
            edge_threshold: 60,
            simplify_epsilon: 1.5,
            // Darker bands: tighter spacing + cross-hatch → read darker.
            bands: vec![
                ToneBand { lo: 200, hi: 235, angle_deg: 45.0, spacing: 13.0, cross: false },
                ToneBand { lo: 150, hi: 200, angle_deg: 45.0, spacing: 9.0, cross: false },
                ToneBand { lo: 90, hi: 150, angle_deg: 45.0, spacing: 7.0, cross: true },
                ToneBand { lo: 0, hi: 90, angle_deg: 45.0, spacing: 5.0, cross: true },
            ],
            outline_pressure: 0.7,
            hatch_pressure: 0.5,
            px_per_second: 300.0,
        }
    }
}

/// Tonal vectorizer: clean edge-traced outlines PLUS tone-graded hatching so
/// darker regions read as darker. This is the "hybrid / realistic" path —
/// definition from edges, realism from graded tone.
pub fn vectorize_tonal(img: &GrayImage, opts: &TonalOptions) -> VectorizeResult {
    let (w, h) = img.dimensions();

    // Outline layer from edges.
    let (_, _, edges) = sobel_edges(img, opts.edge_threshold);
    let skeleton = thin_zhang_suen(w, h, &edges);
    let lines = trace_skeleton(w, h, &skeleton);
    let simplified: Vec<Polyline> = lines.iter().map(|l| simplify(l, opts.simplify_epsilon)).collect();
    let mut strokes = polylines_to_strokes(&simplified, opts.outline_pressure);

    // Tone layer: hatch each band by darkness.
    for band in &opts.bands {
        let (lo, hi) = (band.lo, band.hi);
        let tone_mask: Vec<bool> = img.pixels().map(|p| p.0[0] >= lo && p.0[0] < hi).collect();
        if !tone_mask.iter().any(|&b| b) {
            continue;
        }
        let h1 = hatch_lines(w, h, &tone_mask, band.angle_deg, band.spacing);
        strokes.extend(polylines_to_strokes(&h1, opts.hatch_pressure));
        if band.cross {
            let h2 = hatch_lines(w, h, &tone_mask, band.angle_deg + 90.0, band.spacing);
            strokes.extend(polylines_to_strokes(&h2, opts.hatch_pressure));
        }
    }

    let total = total_draw_seconds(&strokes, opts.px_per_second);
    VectorizeResult { strokes, estimated_draw_seconds: total, degraded_steps: Vec::new() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{GrayImage, Luma};

    fn make_image(rows: &[&str]) -> GrayImage {
        let h = rows.len() as u32;
        let w = rows[0].len() as u32;
        let mut img = GrayImage::new(w, h);
        for (y, row) in rows.iter().enumerate() {
            for (x, c) in row.chars().enumerate() {
                img.put_pixel(x as u32, y as u32, Luma([if c == '#' { 0 } else { 255 }]));
            }
        }
        img
    }

    #[test]
    fn threshold_marks_dark_pixels_as_ink() {
        let img = make_image(&["#.#", "...", "#.#"]);
        let (w, h, mask) = threshold(&img, 128);
        assert_eq!((w, h), (3, 3));
        assert_eq!(mask, vec![true, false, true, false, false, false, true, false, true]);
    }

    #[test]
    fn thinning_a_thick_horizontal_bar_yields_single_row() {
        // A 3-row-thick horizontal bar should thin down to ~1 row.
        let img = make_image(&["......", "######", "######", "######", "......"]);
        let (w, h, mask) = threshold(&img, 128);
        let thin = thin_zhang_suen(w, h, &mask);
        let ink_rows: std::collections::HashSet<u32> =
            (0..h).filter(|&y| (0..w).any(|x| thin[(y * w + x) as usize])).collect();
        assert!(ink_rows.len() <= 2, "expected thinning to collapse thickness, got rows {:?}", ink_rows);
    }

    #[test]
    fn trace_skeleton_recovers_a_straight_line() {
        let img = make_image(&[".....", ".....", "#####", ".....", "....."]);
        let (w, h, mask) = threshold(&img, 128);
        let lines = trace_skeleton(w, h, &mask);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].len(), 5);
    }

    #[test]
    fn simplify_collapses_colinear_points() {
        let points: Vec<Pt> = (0..10).map(|i| Pt { x: i as f32, y: 0.0 }).collect();
        let simplified = simplify(&points, 0.5);
        assert_eq!(simplified.len(), 2, "a straight line should simplify to its two endpoints");
    }

    #[test]
    fn simplify_preserves_a_real_corner() {
        let points = vec![
            Pt { x: 0.0, y: 0.0 },
            Pt { x: 5.0, y: 0.0 },
            Pt { x: 10.0, y: 0.0 },
            Pt { x: 10.0, y: 5.0 },
            Pt { x: 10.0, y: 10.0 },
        ];
        let simplified = simplify(&points, 0.5);
        assert_eq!(simplified.len(), 3); // start, corner, end
    }

    #[test]
    fn hatch_lines_stay_within_masked_region() {
        let w = 20u32;
        let h = 20u32;
        let mut mask = vec![false; (w * h) as usize];
        // A 10x10 masked square in the middle.
        for y in 5..15 {
            for x in 5..15 {
                mask[(y * w + x) as usize] = true;
            }
        }
        let lines = hatch_lines(w, h, &mask, 45.0, 3.0);
        assert!(!lines.is_empty());
        for line in &lines {
            for p in line {
                assert!(p.x >= 4.0 && p.x <= 16.0, "x={} out of expected range", p.x);
                assert!(p.y >= 4.0 && p.y <= 16.0, "y={} out of expected range", p.y);
            }
        }
    }

    #[test]
    fn vectorize_a_simple_shape_produces_strokes_within_default_budget() {
        let mut rows = vec![];
        for _ in 0..30 {
            rows.push("..............................".to_string());
        }
        // Draw a filled-ish square border as "ink".
        for y in 5..25 {
            let mut row: Vec<char> = rows[y].chars().collect();
            row[5] = '#';
            row[24] = '#';
            rows[y] = row.into_iter().collect();
        }
        for x in 5..25 {
            let mut row: Vec<char> = rows[5].chars().collect();
            row[x] = '#';
            rows[5] = row.into_iter().collect();
            let mut row2: Vec<char> = rows[24].chars().collect();
            row2[x] = '#';
            rows[24] = row2.into_iter().collect();
        }
        let row_refs: Vec<&str> = rows.iter().map(|s| s.as_str()).collect();
        let img = make_image(&row_refs);
        let opts = VectorizeOptions::default();
        let result = vectorize(&img, &opts);
        assert!(!result.strokes.is_empty());
        assert!(result.estimated_draw_seconds < opts.max_draw_seconds);
    }

    #[test]
    fn tiny_draw_budget_degrades_gracefully_and_logs_it() {
        let rows: Vec<String> = (0..30)
            .map(|y| if y == 15 { "#".repeat(30) } else { ".".repeat(30) })
            .collect();
        let row_refs: Vec<&str> = rows.iter().map(|s| s.as_str()).collect();
        let img = make_image(&row_refs);
        let mut opts = VectorizeOptions::default();
        opts.max_draw_seconds = 0.0001; // force degradation
        let result = vectorize(&img, &opts);
        assert!(!result.degraded_steps.is_empty());
    }
}
