mod device;
#[cfg(target_os = "linux")]
mod daemon;
mod imagegen;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use inkling_core::vector::{vectorize, VectorizeOptions, VectorizeResult};

#[derive(Parser)]
#[command(name = "inkling")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Vectorize an image and write an SVG preview (works on any OS).
    PreviewVectorize {
        input: String,
        #[arg(default_value = "vectorize_preview.svg")]
        out: String,
        #[arg(long, default_value_t = 1404)]
        page_width: u32,
    },
    /// Capture the current screen to a PNG (on-device only).
    Capture {
        #[arg(default_value = "capture.png")]
        out: String,
    },
    /// Auto-calibrate the display<->pen transform: injects 3 small marks,
    /// captures after each, solves the affine, saves it (on-device only).
    Calibrate {
        #[arg(long, default_value = "/home/root/.config/inkling/calibration.toml")]
        out: String,
    },
    /// Draw a test square+diagonal in display space (on-device only).
    DrawTest {
        #[arg(long, default_value_t = 4000.0)]
        pps: f64,
        #[arg(long, default_value = "/home/root/.config/inkling/calibration.toml")]
        calibration: String,
    },
    /// Debug which injection path reaches the screen: uinput virtual
    /// device vs direct write to the real digitizer node (on-device only).
    PenDebug {},
    /// Run the magic-notebook daemon: watch for finished sketches, generate
    /// illustrations, dissolve, redraw (on-device only).
    Run {
        #[arg(long, default_value = "/home/root/.config/inkling/config.toml")]
        config: String,
    },
    /// Measure the injected eraser band width: draw a filled block, sweep one
    /// eraser line through it, report the cleared band height (on-device only).
    EraseProbe {
        #[arg(long, default_value = "/home/root/.config/inkling/calibration.toml")]
        calibration: String,
    },
    /// "White box" wipe: fill the drawing area with the eraser in columns
    /// stepping left-to-right (on-device only).
    Wipe {
        #[arg(long, default_value_t = 8000.0)]
        pps: f64,
        #[arg(long, default_value = "/home/root/.config/inkling/calibration.toml")]
        calibration: String,
    },
    /// Inject a lasso loop around the whole page with the NIB — if xochitl's
    /// eraser is in "Erase Selection" mode, this clears the page instantly.
    /// Select the lasso/erase-selection tool first (on-device only).
    Lasso {
        #[arg(long, default_value_t = 2000.0)]
        pps: f64,
        #[arg(long, default_value = "/home/root/.config/inkling/calibration.toml")]
        calibration: String,
    },
    /// Progressive dithered fade: erase the page in scattered patches over
    /// several stages so it visibly dissolves away (on-device only).
    Fade {
        #[arg(long, default_value_t = 8000.0)]
        pps: f64,
        #[arg(long, default_value_t = 42)]
        seed: u64,
        #[arg(long, default_value = "/home/root/.config/inkling/calibration.toml")]
        calibration: String,
    },
    /// Erase all ink on the page in scattered dissolve order (on-device only).
    Dissolve {
        #[arg(long, default_value_t = 64)]
        block_px: u32,
        #[arg(long, default_value_t = 14.0)]
        spacing: f32,
        #[arg(long, default_value_t = 6000.0)]
        pps: f64,
        #[arg(long, default_value_t = 42)]
        seed: u64,
        #[arg(long, default_value = "/home/root/.config/inkling/calibration.toml")]
        calibration: String,
    },
    /// Vectorize an image and draw it on the current page via the virtual
    /// pen (on-device only). The star of the show.
    DrawImage {
        input: String,
        #[arg(long, default_value_t = 4000.0)]
        pps: f64,
        #[arg(long, default_value_t = 16000)]
        max_points: usize,
        #[arg(long, default_value = "/home/root/.config/inkling/calibration.toml")]
        calibration: String,
        /// Input image is in landscape orientation (rotated to portrait
        /// before drawing so it reads correctly when the tablet is held
        /// landscape). On by default per the owner's usage.
        #[arg(long, default_value_t = true)]
        landscape: bool,
    },
}

fn main() -> Result<()> {
    env_logger::init();
    let cli = Cli::parse();
    match cli.command {
        Commands::PreviewVectorize { input, out, page_width } => preview_vectorize(&input, &out, page_width),
        Commands::Capture { out } => capture(&out),
        Commands::Calibrate { out } => calibrate(&out),
        Commands::DrawTest { pps, calibration } => draw_test(pps, &calibration),
        Commands::PenDebug {} => pen_debug(),
        Commands::Dissolve { block_px, spacing, pps, seed, calibration } => dissolve(block_px, spacing, pps, seed, &calibration),
        Commands::EraseProbe { calibration } => erase_probe(&calibration),
        Commands::Fade { pps, seed, calibration } => fade(pps, seed, &calibration),
        Commands::Lasso { pps, calibration } => lasso(pps, &calibration),
        Commands::Wipe { pps, calibration } => wipe(pps, &calibration),
        Commands::Run { config } => run_daemon(&config),
        Commands::DrawImage { input, pps, max_points, calibration, landscape } => {
            draw_image(&input, pps, max_points, &calibration, landscape)
        }
    }
}

pub fn vectorize_for_page(input: &str, landscape: bool, max_points: usize) -> Result<(VectorizeResult, u32, u32)> {
    let src = image::open(input).context("loading input image")?;
    // Composite over white BEFORE dropping alpha. Model output is often line art
    // on a transparent background (RGB≈0, alpha=0); a plain to_luma8() would read
    // that as pure black and threshold the whole page as ink (minutes of hatching
    // a black rectangle). Alpha-blend onto white so transparent stays white.
    let mut rgba = src.to_rgba8();
    for p in rgba.pixels_mut() {
        let a = p[3] as u16;
        for c in 0..3 {
            p[c] = ((p[c] as u16 * a + 255 * (255 - a)) / 255) as u8;
        }
        p[3] = 255;
    }
    // Landscape-drawn pages: rotate the image 90° CW into the portrait buffer
    // orientation (verified against the capture pipeline).
    let rgba = if landscape { image::imageops::rotate90(&rgba) } else { rgba };
    let img = image::DynamicImage::ImageRgba8(rgba);

    // Fit inside the page with a generous margin. This must keep every injected
    // pen stroke well clear of the on-screen UI — the toolbar runs down one edge
    // and the close/settings buttons sit in the top corners — because the pen
    // taps those controls (it was changing tool/colour mid-draw). A uniform inset
    // clears an edge-strip toolbar whichever side it's on for the current rotation.
    const PAGE_W: u32 = 1404;
    const PAGE_H: u32 = 1872;
    const MARGIN: u32 = 170;
    let img = img
        .resize(PAGE_W - 2 * MARGIN, PAGE_H - 2 * MARGIN, image::imageops::FilterType::Lanczos3)
        .to_luma8();
    let (w, h) = img.dimensions();
    let ox = (PAGE_W - w) / 2;
    let oy = (PAGE_H - h) / 2;

    // Smoother, flowing strokes: a larger simplify epsilon collapses the
    // tiny jitters skeleton-tracing produces into clean lines, and wider
    // hatch spacing keeps any shading sparse rather than scratchy.
    let mut opts = VectorizeOptions::default();
    opts.simplify_epsilon = 2.5;
    opts.hatch_spacing_light = 14.0;
    opts.hatch_spacing_dark = 9.0;
    let mut result = vectorize(&img, &opts);

    // Enforce the point budget: drop the shortest strokes (hatching detail)
    // first, keeping long outlines. Duration ≈ points / pps.
    let total_points = |r: &VectorizeResult| r.strokes.iter().map(|s| s.points.len()).sum::<usize>();
    if total_points(&result) > max_points {
        let before = total_points(&result);
        result.strokes.sort_by(|a, b| b.len_px().partial_cmp(&a.len_px()).unwrap_or(std::cmp::Ordering::Equal));
        let mut kept = Vec::new();
        let mut points = 0usize;
        for s in result.strokes.drain(..) {
            let n = s.points.len();
            if points + n <= max_points {
                points += n;
                kept.push(s);
            }
        }
        result.strokes = kept;
        result
            .degraded_steps
            .push(format!("point budget: dropped shortest strokes ({} -> {} points)", before, points));
    }

    // Offset strokes into page coordinates.
    for s in &mut result.strokes {
        for p in &mut s.points {
            p.x += ox as f32;
            p.y += oy as f32;
        }
    }
    Ok((result, w, h))
}

fn preview_vectorize(input: &str, out: &str, page_width: u32) -> Result<()> {
    let img = image::open(input).context("loading input image")?;
    let img = img.resize(page_width, u32::MAX, image::imageops::FilterType::Lanczos3).to_luma8();
    let tonal = std::env::var("SCRIBED_TONAL").is_ok();
    let result = if tonal {
        inkling_core::vector::vectorize_tonal(&img, &inkling_core::vector::TonalOptions::default())
    } else {
        vectorize(&img, &VectorizeOptions::default())
    };

    println!("strokes: {}", result.strokes.len());
    println!("points: {}", result.strokes.iter().map(|s| s.points.len()).sum::<usize>());
    println!("estimated draw time: {:.1}s", result.estimated_draw_seconds);
    for d in &result.degraded_steps {
        println!("degraded: {d}");
    }

    let (w, h) = img.dimensions();
    let mut svg = String::new();
    svg.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{w}\" height=\"{h}\" viewBox=\"0 0 {w} {h}\">\n<rect width=\"100%\" height=\"100%\" fill=\"white\"/>\n"
    ));
    for stroke in &result.strokes {
        if stroke.points.is_empty() {
            continue;
        }
        let pts: Vec<String> = stroke.points.iter().map(|p| format!("{:.1},{:.1}", p.x, p.y)).collect();
        svg.push_str(&format!(
            "<polyline points=\"{}\" fill=\"none\" stroke=\"black\" stroke-width=\"1.6\" stroke-linecap=\"round\" stroke-linejoin=\"round\"/>\n",
            pts.join(" ")
        ));
    }
    svg.push_str("</svg>\n");
    std::fs::write(out, svg).context("writing SVG preview")?;
    println!("saved {out}");
    Ok(())
}

// ---------------- on-device commands ----------------

#[cfg(not(target_os = "linux"))]
fn capture(_out: &str) -> Result<()> {
    anyhow::bail!("capture runs on the reMarkable only")
}
#[cfg(not(target_os = "linux"))]
fn erase_probe(_calibration: &str) -> Result<()> {
    anyhow::bail!("erase-probe runs on the reMarkable only")
}
#[cfg(not(target_os = "linux"))]
fn fade(_pps: f64, _seed: u64, _calibration: &str) -> Result<()> {
    anyhow::bail!("fade runs on the reMarkable only")
}
#[cfg(not(target_os = "linux"))]
fn lasso(_pps: f64, _calibration: &str) -> Result<()> {
    anyhow::bail!("lasso runs on the reMarkable only")
}
#[cfg(not(target_os = "linux"))]
fn wipe(_pps: f64, _calibration: &str) -> Result<()> {
    anyhow::bail!("wipe runs on the reMarkable only")
}
#[cfg(not(target_os = "linux"))]
fn calibrate(_out: &str) -> Result<()> {
    anyhow::bail!("calibrate runs on the reMarkable only")
}
#[cfg(not(target_os = "linux"))]
fn draw_test(_pps: f64, _calibration: &str) -> Result<()> {
    anyhow::bail!("draw-test runs on the reMarkable only")
}
#[cfg(not(target_os = "linux"))]
fn draw_image(_input: &str, _pps: f64, _max_points: usize, _calibration: &str, _landscape: bool) -> Result<()> {
    anyhow::bail!("draw-image runs on the reMarkable only")
}
#[cfg(not(target_os = "linux"))]
fn pen_debug() -> Result<()> {
    anyhow::bail!("pen-debug runs on the reMarkable only")
}
#[cfg(not(target_os = "linux"))]
fn dissolve(_block_px: u32, _spacing: f32, _pps: f64, _seed: u64, _calibration: &str) -> Result<()> {
    anyhow::bail!("dissolve runs on the reMarkable only")
}
#[cfg(not(target_os = "linux"))]
fn run_daemon(_config: &str) -> Result<()> {
    anyhow::bail!("the daemon runs on the reMarkable only")
}
#[cfg(target_os = "linux")]
fn run_daemon(config_path: &str) -> Result<()> {
    let mut cfg = daemon::DaemonConfig::default();
    match std::fs::read_to_string(config_path) {
        Ok(s) => {
            let v: toml::Value = s.parse().context("parsing config toml")?;
            let get = |table: &str, key: &str| v.get(table).and_then(|t| t.get(key)).cloned();
            // Accept both float (2.0) and integer (2) TOML literals for numbers —
            // `as_float()` alone silently ignores integer literals.
            let getf = |t: &str, k: &str| {
                get(t, k).and_then(|x| x.as_float().or_else(|| x.as_integer().map(|i| i as f64)))
            };
            let geti = |t: &str, k: &str| get(t, k).and_then(|x| x.as_integer());
            let gets = |t: &str, k: &str| get(t, k).and_then(|x| x.as_str().map(String::from));

            if let Some(k) = gets("imagegen", "api_key") { cfg.api_key = k; }
            if let Some(m) = gets("imagegen", "model") { cfg.model = Some(m); }
            if let Some(d) = getf("watch", "dwell_s") { cfg.dwell_s = d; }
            if let Some(r) = getf("watch", "rate_limit_s") { cfg.rate_limit_s = r; }
            if let Some(n) = geti("watch", "min_new_ink_px") { cfg.min_new_ink_px = n.max(0) as usize; }
            if let Some(p) = getf("ink", "draw_pps") { cfg.draw_pps = p; }
            if let Some(p) = getf("ink", "erase_pps") { cfg.erase_pps = p; }
            if let Some(n) = geti("ink", "max_points") { cfg.max_points = n.max(0) as usize; }
            if let Some(a) = gets("archive", "dir") { cfg.archive_dir = a; }
            if let Some(pf) = gets("control", "pause_file") { cfg.pause_file = pf; }
        }
        Err(e) => anyhow::bail!("cannot read config {config_path}: {e}"),
    }
    daemon::run(cfg)
}

#[cfg(target_os = "linux")]
pub mod on_device {
    use super::*;
    use crate::device::{capture as cap, uinput::{Tool, VirtualPen}};
    use inkling_core::geometry::{AffineTransform, PointPx, PenUnits, Stroke};
    use std::thread::sleep;
    use std::time::Duration;

    pub fn save_png(img: &image::GrayImage, out: &str) -> Result<()> {
        img.save(out).with_context(|| format!("saving {out}"))
    }

    pub fn load_calibration(path: &str) -> Result<AffineTransform> {
        let s = std::fs::read_to_string(path)
            .with_context(|| format!("reading calibration {path} — run `inkling calibrate` first"))?;
        let v: toml::Value = s.parse().context("parsing calibration toml")?;
        let arr = v
            .get("display_to_pen")
            .and_then(|a| a.as_array())
            .context("calibration missing display_to_pen array")?;
        let f: Vec<f64> = arr.iter().filter_map(|x| x.as_float()).collect();
        anyhow::ensure!(f.len() == 6, "display_to_pen must have 6 floats");
        Ok(AffineTransform { a: f[0], b: f[1], c: f[2], d: f[3], e: f[4], f: f[5] })
    }

    pub fn save_calibration(path: &str, t: &AffineTransform) -> Result<()> {
        if let Some(dir) = std::path::Path::new(path).parent() {
            std::fs::create_dir_all(dir)?;
        }
        let body = format!(
            "# inkling display->pen affine, written by `inkling calibrate`\ndisplay_to_pen = [{}, {}, {}, {}, {}, {}]\n",
            t.a, t.b, t.c, t.d, t.e, t.f
        );
        std::fs::write(path, body).with_context(|| format!("writing {path}"))
    }

    /// Centroid of pixels that became dark between two captures.
    fn new_ink_centroid(before: &image::GrayImage, after: &image::GrayImage) -> Result<PointPx> {
        let (mut sx, mut sy, mut n) = (0f64, 0f64, 0u32);
        for (x, y, p_after) in after.enumerate_pixels() {
            let was = before.get_pixel(x, y).0[0];
            let now = p_after.0[0];
            if now < 100 && was >= 100 {
                sx += x as f64;
                sy += y as f64;
                n += 1;
            }
        }
        anyhow::ensure!(n >= 20, "calibration mark not detected (only {n} changed px) — is the notebook page visible?");
        Ok(PointPx::new((sx / n as f64) as f32, (sy / n as f64) as f32))
    }

    fn draw_mark_at_pen_units(pen: &mut VirtualPen, x: i32, y: i32) -> Result<()> {
        // A small X: two short strokes crossing at (x, y).
        let d = 150; // pen units (~13 px)
        for (dx0, dy0, dx1, dy1) in [(-d, -d, d, d), (-d, d, d, -d)] {
            let pts: Vec<(i32, i32, i32)> = (0..=20)
                .map(|i| {
                    let t = i as f32 / 20.0;
                    (
                        x + (dx0 as f32 + (dx1 - dx0) as f32 * t) as i32,
                        y + (dy0 as f32 + (dy1 - dy0) as f32 * t) as i32,
                        2400,
                    )
                })
                .collect();
            pen.stroke_pen_units(&pts, 2000.0)?;
        }
        Ok(())
    }

    /// The injection path that actually reaches xochitl on this OS build
    /// (verified by pen-debug: uinput hotplug is ignored; direct writes to
    /// the real digitizer node work).
    pub const PEN_NODE: &str = "/dev/input/event1";

    pub fn calibrate(out: &str) -> Result<()> {
        println!("calibrating: injecting 3 marks and watching where they land...");
        let mut pen = VirtualPen::open_existing(PEN_NODE)?;
        pen.tool_in(Tool::Pen)?;

        // Marks in pen units, spread widely, asymmetric (away from edges/toolbar).
        let marks: [(i32, i32); 3] = [(4000, 4000), (17000, 4500), (5000, 12000)];
        let mut correspondences: Vec<(PointPx, PenUnits)> = Vec::new();

        let mut before = cap::capture_now()?;
        for (i, &(mx, my)) in marks.iter().enumerate() {
            draw_mark_at_pen_units(&mut pen, mx, my)?;
            sleep(Duration::from_millis(800)); // let xochitl render
            let after = cap::capture_now()?;
            let centroid = new_ink_centroid(&before, &after)?;
            println!("  mark {i} at pen ({mx},{my}) -> display ({:.1},{:.1})", centroid.x, centroid.y);
            correspondences.push((centroid, PenUnits { x: mx, y: my }));
            before = after;
        }
        pen.tool_out()?;

        let t = AffineTransform::fit(&correspondences).context("affine fit failed (marks collinear?)")?;
        // Report residuals.
        for (disp, pu) in &correspondences {
            let p = t.apply(*disp);
            println!("  residual: ({}, {}) pen units", p.x - pu.x, p.y - pu.y);
        }
        save_calibration(out, &t)?;
        println!("saved calibration to {out}");
        println!("note: 3 small X marks are now on the page — erase or clear the page when done.");
        Ok(())
    }

    pub fn draw_test(pps: f64, calibration: &str) -> Result<()> {
        let t = load_calibration(calibration)?;
        let mut pen = VirtualPen::open_existing(PEN_NODE)?;
        pen.tool_in(Tool::Pen)?;
        let mut square = Stroke::new();
        for &(x, y) in &[(500.0, 700.0), (900.0, 700.0), (900.0, 1100.0), (500.0, 1100.0), (500.0, 700.0), (900.0, 1100.0)] {
            square.push(x, y, 0.6);
        }
        // Resample to a denser polyline for smooth rendering.
        let dense = densify(&square, 3.0);
        pen.stroke_display(&dense, &t, pps)?;
        pen.tool_out()?;
        println!("drew test square+diagonal at display (500,700)-(900,1100), {} events", pen.events_written());
        Ok(())
    }

    pub fn densify(s: &Stroke, spacing: f32) -> Stroke {
        let mut out = Stroke::new();
        if s.points.is_empty() {
            return out;
        }
        out.points.push(s.points[0]);
        for w in s.points.windows(2) {
            let (a, b) = (w[0], w[1]);
            let dist = ((b.x - a.x).powi(2) + (b.y - a.y).powi(2)).sqrt();
            let steps = (dist / spacing).ceil().max(1.0) as usize;
            for i in 1..=steps {
                let t = i as f32 / steps as f32;
                out.points.push(inkling_core::geometry::StrokePoint {
                    x: a.x + (b.x - a.x) * t,
                    y: a.y + (b.y - a.y) * t,
                    pressure: a.pressure + (b.pressure - a.pressure) * t,
                });
            }
        }
        out
    }

    pub fn draw_image(input: &str, pps: f64, max_points: usize, calibration: &str, landscape: bool) -> Result<()> {
        let t = load_calibration(calibration)?;
        let (result, w, h) = super::vectorize_for_page(input, landscape, max_points)?;
        let strokes = result.strokes;
        let n_points: usize = strokes.iter().map(|s| s.points.len()).sum();
        println!("drawing {} strokes / {} points ({}x{} content) at {} pps ≈ {:.1}s", strokes.len(), n_points, w, h, pps, n_points as f64 / pps);
        for d in &result.degraded_steps {
            println!("note: {d}");
        }

        let started = std::time::Instant::now();
        let mut pen = VirtualPen::open_existing(PEN_NODE)?;
        pen.tool_in(Tool::Pen)?;
        for s in &strokes {
            let dense = densify(s, 3.0);
            pen.stroke_display(&dense, &t, pps)?;
        }
        pen.tool_out()?;
        println!("done in {:.2}s ({} events)", started.elapsed().as_secs_f64(), pen.events_written());
        Ok(())
    }

    pub fn capture(out: &str) -> Result<()> {
        let img = cap::capture_now()?;
        save_png(&img, out)?;
        println!("saved {out}");
        Ok(())
    }

    fn changed_px(before: &image::GrayImage, after: &image::GrayImage) -> u32 {
        let mut n = 0;
        for (x, y, p) in after.enumerate_pixels() {
            if p.0[0] < 100 && before.get_pixel(x, y).0[0] >= 100 {
                n += 1;
            }
        }
        n
    }

    fn debug_stroke(pen: &mut VirtualPen, cx: i32, cy: i32) -> Result<()> {
        pen.tool_in(Tool::Pen)?;
        let pts: Vec<(i32, i32, i32)> = (0..=100)
            .map(|i| {
                let t = i as f32 / 100.0;
                (cx - 1500 + (3000.0 * t) as i32, cy - 1500 + (3000.0 * t) as i32, 2400)
            })
            .collect();
        pen.stroke_pen_units(&pts, 400.0)?; // slow-ish, very visible
        pen.tool_out()?;
        Ok(())
    }

    pub fn dissolve(_block_px: u32, _spacing: f32, pps: f64, seed: u64, calibration: &str) -> Result<()> {
        // Uses the exact production dissolve (fast full-width sweeps, repeat
        // until clean) so this command tests what the daemon actually does.
        let t = load_calibration(calibration)?;
        let started = std::time::Instant::now();
        crate::daemon::dissolve_page(&t, pps, seed)?;
        println!("dissolve done in {:.2}s", started.elapsed().as_secs_f64());
        sleep(Duration::from_millis(800));
        let after = cap::capture_now()?;
        let residual = after.pixels().filter(|p| p.0[0] < 100).count();
        println!("residual dark px: {residual}");
        Ok(())
    }

    pub fn wipe(pps: f64, calibration: &str) -> Result<()> {
        let t = load_calibration(calibration)?;
        let started = std::time::Instant::now();
        crate::daemon::wipe_page(&t, pps)?;
        println!("wipe done in {:.2}s", started.elapsed().as_secs_f64());
        sleep(Duration::from_millis(400));
        let after = cap::capture_now()?;
        let residual = after.pixels().filter(|p| p.0[0] < 100).count();
        println!("residual dark px: {residual}");
        Ok(())
    }

    pub fn lasso(pps: f64, calibration: &str) -> Result<()> {
        let t = load_calibration(calibration)?;
        // A closed loop just inside the page margins, enclosing all content.
        // Uses the NIB (Tool::Pen) so it takes on whatever tool xochitl has
        // selected — the point is to test Erase-Selection mode.
        let (l, r, top, bot) = (70.0f32, 1334.0, 90.0, 1782.0);
        let corners = [
            (l, top), (r, top), (r, bot), (l, bot), (l, top),
        ];
        let mut stroke = Stroke::new();
        for w in corners.windows(2) {
            let (a, b) = (w[0], w[1]);
            let steps = 60;
            for i in 0..=steps {
                let f = i as f32 / steps as f32;
                stroke.push(a.0 + (b.0 - a.0) * f, a.1 + (b.1 - a.1) * f, 0.6);
            }
        }
        let mut pen = VirtualPen::open_existing(PEN_NODE)?;
        pen.tool_in(Tool::Pen)?;
        let dense = densify(&stroke, 4.0);
        pen.stroke_display(&dense, &t, pps)?;
        pen.tool_out()?;
        std::thread::sleep(Duration::from_millis(800));
        let after = cap::capture_now()?;
        let residual = after.pixels().filter(|p| p.0[0] < 100).count();
        println!("after lasso: residual dark px: {residual}");
        println!("(if it dropped near zero, Erase-Selection lasso works)");
        Ok(())
    }

    pub fn fade(pps: f64, seed: u64, calibration: &str) -> Result<()> {
        let t = load_calibration(calibration)?;
        let started = std::time::Instant::now();
        crate::daemon::fade_page(&t, pps, seed)?;
        println!("fade done in {:.2}s", started.elapsed().as_secs_f64());
        sleep(Duration::from_millis(400));
        let after = cap::capture_now()?;
        let residual = after.pixels().filter(|p| p.0[0] < 100).count();
        println!("residual dark px: {residual}");
        Ok(())
    }

    pub fn erase_probe(calibration: &str) -> Result<()> {
        let t = load_calibration(calibration)?;
        // Draw a solid filled block: many tightly-spaced horizontal pen lines
        // over a 400x400 display-px area centered on the page.
        let (cx, cy) = (700.0f32, 936.0f32);
        let half = 200.0f32;
        {
            let mut pen = VirtualPen::open_existing(PEN_NODE)?;
            pen.tool_in(Tool::Pen)?;
            let mut y = cy - half;
            while y <= cy + half {
                let pts: Vec<(i32, i32, i32)> = {
                    let mut v = Vec::new();
                    let mut x = cx - half;
                    while x <= cx + half {
                        let pen_u = t.apply(inkling_core::geometry::PointPx::new(x, y));
                        v.push((pen_u.x, pen_u.y, 2400));
                        x += 3.0;
                    }
                    v
                };
                pen.stroke_pen_units(&pts, 8000.0)?;
                y += 2.0;
            }
            pen.tool_out()?;
        }
        sleep(Duration::from_millis(1000));
        let filled = cap::capture_now()?;

        // One horizontal eraser sweep straight through the middle.
        {
            let mut pen = VirtualPen::open_existing(PEN_NODE)?;
            pen.tool_in(Tool::Rubber)?;
            let pts: Vec<(i32, i32, i32)> = {
                let mut v = Vec::new();
                let mut x = cx - half - 30.0;
                while x <= cx + half + 30.0 {
                    let pen_u = t.apply(inkling_core::geometry::PointPx::new(x, cy));
                    v.push((pen_u.x, pen_u.y, 2400));
                    x += 3.0;
                }
                v
            };
            pen.stroke_pen_units(&pts, 1500.0)?;
            pen.tool_out()?;
        }
        sleep(Duration::from_millis(1000));
        let after = cap::capture_now()?;

        // Measure cleared band height in the central column: rows that were
        // ink in `filled` but white in `after`.
        let col = cx as u32;
        let mut cleared_rows = 0u32;
        let (_, h) = filled.dimensions();
        for y in 0..h {
            let was = filled.get_pixel(col, y).0[0] < 100;
            let now = after.get_pixel(col, y).0[0] < 100;
            if was && !now {
                cleared_rows += 1;
            }
        }
        println!("eraser band width at center column: {cleared_rows} px");
        println!("(use sweep spacing <= this for complete single-pass coverage)");
        Ok(())
    }

    pub fn pen_debug() -> Result<()> {
        let before = cap::capture_now()?;

        println!("[1/2] uinput virtual device path...");
        {
            let mut pen = VirtualPen::new()?;
            debug_stroke(&mut pen, 8000, 6000)?;
        }
        sleep(Duration::from_millis(800));
        let after_uinput = cap::capture_now()?;
        let n1 = changed_px(&before, &after_uinput);
        println!("      -> {n1} new ink px");

        println!("[2/2] direct injection into /dev/input/event1...");
        {
            let mut pen = VirtualPen::open_existing("/dev/input/event1")?;
            debug_stroke(&mut pen, 12000, 9000)?;
        }
        sleep(Duration::from_millis(800));
        let after_direct = cap::capture_now()?;
        let n2 = changed_px(&after_uinput, &after_direct);
        println!("      -> {n2} new ink px");

        println!();
        match (n1 > 50, n2 > 50) {
            (true, true) => println!("VERDICT: both paths work; prefer uinput (cleaner separation from real pen)"),
            (true, false) => println!("VERDICT: uinput works, direct injection does not"),
            (false, true) => println!("VERDICT: only DIRECT injection into event1 works — use open_existing"),
            (false, false) => println!("VERDICT: NEITHER path inked pixels — xochitl input handling needs deeper investigation"),
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
use on_device::{calibrate, capture, dissolve, draw_image, draw_test, erase_probe, fade, lasso, pen_debug, wipe};
