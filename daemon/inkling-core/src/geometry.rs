//! Shared geometric types. Pure data — no device or I/O knowledge.

/// A point in display pixel space (post-rotation, matches what's on screen).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PointPx {
    pub x: f32,
    pub y: f32,
}

impl PointPx {
    pub fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

/// A point in the pen digitizer's native coordinate space, ready for uinput.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PenUnits {
    pub x: i32,
    pub y: i32,
}

/// One point along an inked stroke: display-space position plus pen pressure
/// (0.0-1.0, scaled to the digitizer's ABS_PRESSURE range at injection time).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StrokePoint {
    pub x: f32,
    pub y: f32,
    pub pressure: f32,
}

/// A single pen-down-to-pen-up stroke: a sequence of points to be replayed
/// through the virtual pen with SYN_REPORT after each.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Stroke {
    pub points: Vec<StrokePoint>,
}

impl Stroke {
    pub fn new() -> Self {
        Self { points: Vec::new() }
    }

    pub fn push(&mut self, x: f32, y: f32, pressure: f32) {
        self.points.push(StrokePoint { x, y, pressure });
    }

    pub fn len_px(&self) -> f32 {
        self.points
            .windows(2)
            .map(|w| {
                let (a, b) = (w[0], w[1]);
                ((b.x - a.x).powi(2) + (b.y - a.y).powi(2)).sqrt()
            })
            .sum()
    }

    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }
}

/// A rectangle in display pixel space, e.g. the writable/layout region.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RectPx {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl RectPx {
    pub fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self { x, y, width, height }
    }

    pub fn contains(&self, p: PointPx) -> bool {
        p.x >= self.x && p.x <= self.x + self.width && p.y >= self.y && p.y <= self.y + self.height
    }
}

/// Calibrated affine map from display pixels to pen digitizer units.
/// Solved empirically per-device (DESIGN.md §9.3/§10.3) — never hardcoded.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AffineTransform {
    // [ a b c ]   [x]
    // [ d e f ] * [y]
    //             [1]
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d: f64,
    pub e: f64,
    pub f: f64,
}

impl AffineTransform {
    pub const IDENTITY: Self = Self { a: 1.0, b: 0.0, c: 0.0, d: 0.0, e: 1.0, f: 0.0 };

    pub fn apply(&self, p: PointPx) -> PenUnits {
        let x = self.a * p.x as f64 + self.b * p.y as f64 + self.c;
        let y = self.d * p.x as f64 + self.e * p.y as f64 + self.f;
        PenUnits { x: x.round() as i32, y: y.round() as i32 }
    }

    /// Least-squares fit from >= 3 (display_px -> pen_units) correspondences.
    /// Used by the host-side calibration tool (DESIGN.md §10.3).
    pub fn fit(correspondences: &[(PointPx, PenUnits)]) -> Option<Self> {
        if correspondences.len() < 3 {
            return None;
        }
        // Solve two independent least-squares systems:
        //   pen_x = a*px + b*py + c
        //   pen_y = d*px + e*py + f
        let n = correspondences.len() as f64;
        let (mut sx, mut sy, mut sxx, mut syy, mut sxy) = (0.0, 0.0, 0.0, 0.0, 0.0);
        let (mut spx_x, mut spx_y, mut spy_x, mut spy_y) = (0.0, 0.0, 0.0, 0.0);
        for (px, pu) in correspondences {
            let (x, y) = (px.x as f64, px.y as f64);
            sx += x;
            sy += y;
            sxx += x * x;
            syy += y * y;
            sxy += x * y;
            spx_x += x * pu.x as f64;
            spx_y += y * pu.x as f64;
            spy_x += x * pu.y as f64;
            spy_y += y * pu.y as f64;
        }
        let sum_pen_x: f64 = correspondences.iter().map(|(_, pu)| pu.x as f64).sum();
        let sum_pen_y: f64 = correspondences.iter().map(|(_, pu)| pu.y as f64).sum();

        // Normal equations matrix (shared structure for both solves):
        // [ sxx sxy sx ] [a]   [spx_x]
        // [ sxy syy sy ] [b] = [spx_y]
        // [ sx  sy  n  ] [c]   [sum_pen_x]
        let m = [[sxx, sxy, sx], [sxy, syy, sy], [sx, sy, n]];
        let (a, b, c) = solve3(m, [spx_x, spx_y, sum_pen_x])?;
        let (d, e, f) = solve3(m, [spy_x, spy_y, sum_pen_y])?;
        Some(Self { a, b, c, d, e, f })
    }
}

fn solve3(m: [[f64; 3]; 3], rhs: [f64; 3]) -> Option<(f64, f64, f64)> {
    // Cramer's rule.
    let det = |m: [[f64; 3]; 3]| -> f64 {
        m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
            - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
            + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0])
    };
    let d = det(m);
    if d.abs() < 1e-9 {
        return None;
    }
    let mut mx = m;
    for i in 0..3 {
        mx[i][0] = rhs[i];
    }
    let mut my = m;
    for i in 0..3 {
        my[i][1] = rhs[i];
    }
    let mut mz = m;
    for i in 0..3 {
        mz[i][2] = rhs[i];
    }
    Some((det(mx) / d, det(my) / d, det(mz) / d))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_transform_passes_through() {
        let p = PointPx::new(100.0, 200.0);
        let out = AffineTransform::IDENTITY.apply(p);
        assert_eq!(out, PenUnits { x: 100, y: 200 });
    }

    #[test]
    fn fit_recovers_known_scale_and_offset() {
        // pen = 2*display + (10, -5). Points form a proper 2D grid (not
        // collinear) so the normal-equations matrix is non-singular.
        let corr: Vec<(PointPx, PenUnits)> = (0..3)
            .flat_map(|i| {
                (0..3).map(move |j| {
                    let px = PointPx::new(i as f32 * 37.0, j as f32 * 19.0 + 3.0);
                    let pu = PenUnits { x: (2.0 * px.x + 10.0).round() as i32, y: (2.0 * px.y - 5.0).round() as i32 };
                    (px, pu)
                })
            })
            .collect();
        let t = AffineTransform::fit(&corr).expect("fit should succeed");
        let check = t.apply(PointPx::new(50.0, 80.0));
        assert!((check.x - 110).abs() <= 1, "x={}", check.x);
        assert!((check.y - 155).abs() <= 1, "y={}", check.y);
    }

    #[test]
    fn stroke_length_of_unit_square_diagonal() {
        let mut s = Stroke::new();
        s.push(0.0, 0.0, 0.5);
        s.push(3.0, 4.0, 0.5);
        assert!((s.len_px() - 5.0).abs() < 1e-4);
    }

    #[test]
    fn rect_contains_boundary_inclusive() {
        let r = RectPx::new(0.0, 0.0, 10.0, 10.0);
        assert!(r.contains(PointPx::new(10.0, 10.0)));
        assert!(!r.contains(PointPx::new(10.1, 5.0)));
    }
}
