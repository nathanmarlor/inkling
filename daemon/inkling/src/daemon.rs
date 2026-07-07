//! The magic loop (linux-only): watch the pen, and when the user finishes a
//! sketch — pen away, page quiet — capture it, send it to the image model,
//! dissolve the sketch off the page, and draw the illustration back.
//!
//! Self-injection note: we inject into the same evdev node we watch
//! (/dev/input/event1), so during DISSOLVING/DRAWING the watcher thread
//! must ignore everything, and after injection we drain the read queue
//! before re-arming — otherwise the daemon would see its own strokes as
//! new user ink and re-trigger forever.

#![cfg(target_os = "linux")]

use anyhow::{Context, Result};
use inkling_core::dissolve::{plan_dissolve, InkMask};
use inkling_core::geometry::AffineTransform;
use inkling_core::watch::{count_new_ink, PenEvent, SessionWatcher};
use std::io::Read;
use std::os::unix::io::AsRawFd;
use std::time::{Duration, Instant};

use crate::device::capture as cap;
use crate::device::uinput::{Tool, VirtualPen};
use crate::imagegen::OpenRouterClient;

const PEN_NODE: &str = "/dev/input/event1";

const EV_KEY: u16 = 0x01;
const BTN_TOOL_PEN: u16 = 0x140;
const BTN_TOOL_RUBBER: u16 = 0x141;
const BTN_TOUCH: u16 = 0x14a;

/// How a conversion is triggered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TriggerMode {
    /// Original flow: finished-sketch detection turns the whole page after a quiet period.
    Inactivity,
    /// New flow: the user selects strokes and taps the "Convert" selection-menu item,
    /// and only that selection is turned into the illustration, fitted to its bounds.
    Selection,
}

pub struct DaemonConfig {
    pub api_key: String,
    pub model: Option<String>,
    pub dwell_s: f64,
    pub rate_limit_s: f64,
    pub min_new_ink_px: usize,
    pub draw_pps: f64,
    pub erase_pps: f64,
    pub max_points: usize,
    pub calibration_path: String,
    pub archive_dir: String,
    pub pause_file: String,
    pub trigger_mode: TriggerMode,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            model: None,
            dwell_s: 8.0,
            rate_limit_s: 15.0,
            min_new_ink_px: 200,
            draw_pps: 4000.0,
            // Erasing drops events well above ~2000 pps (xochitl consumes
            // erase events slower than draw events), leaving striped residue.
            // Keep the erase rate in the reliable band.
            erase_pps: 2000.0,
            max_points: 30000,
            calibration_path: "/home/root/.config/inkling/calibration.toml".into(),
            archive_dir: "/home/root/.local/share/inkling/archive".into(),
            pause_file: "/home/root/.config/inkling/pause".into(),
            trigger_mode: TriggerMode::Inactivity,
        }
    }
}

/// Complete page reset via eraser: sweep full-width horizontal bands over the
/// inked region. Deterministic fixed passes (NO capture-based completion
/// loop — at high pps xochitl repaints the panel a beat behind the document,
/// so a capture taken right after erasing reads stale ink and would loop
/// forever; the erase itself lands, as verified on-device).
///
/// The injected eraser band is 14px wide (`inkling erase-probe`); spacing 10
/// with two phase-offset passes guarantees gap-free coverage.
pub fn dissolve_page(calibration: &AffineTransform, erase_pps: f64, _seed: u64) -> Result<()> {
    use inkling_core::geometry::Stroke;

    const CORNER: u32 = 180;
    const BAND_SPACING: f32 = 9.0; // < 14px eraser band, generous overlap
    const PAD: f32 = 16.0;

    // One capture up front to bound the work to the actual ink.
    let img = cap::capture_now()?;
    let (w, h) = img.dimensions();
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (w, h, 0u32, 0u32);
    let mut ink_count = 0u32;
    for (x, y, p) in img.enumerate_pixels() {
        let in_corner = (x < CORNER || x >= w - CORNER) && (y < CORNER || y >= h - CORNER);
        if !in_corner && p.0[0] < 100 {
            ink_count += 1;
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
        }
    }
    if ink_count < 400 {
        log::info!("dissolve: page already clean ({ink_count} px)");
        return Ok(());
    }
    let (x0, x1) = ((min_x as f32 - PAD).max(0.0), (max_x as f32 + PAD).min(w as f32));

    // Two phase-offset passes at full speed for guaranteed coverage, then a
    // settle so xochitl finishes draining its input queue and repaints the
    // panel (rM2 exposes no external EPDC refresh, so "refresh" = stop
    // injecting and let xochitl catch up). Waiting is faster than a slow
    // extra erase pass and leaves the panel current.
    log::info!("dissolve: bbox {ink_count} px, 2 passes @ {erase_pps} pps");
    for pass in 0..2u32 {
        let mut pen = VirtualPen::open_existing(PEN_NODE)?;
        pen.tool_in(Tool::Rubber)?;
        let phase = pass as f32 * (BAND_SPACING / 2.0);
        let mut y = min_y as f32 - PAD + phase;
        let mut k = 0u32;
        while y <= max_y as f32 + PAD {
            let (sx, ex) = if k % 2 == 0 { (x0, x1) } else { (x1, x0) };
            let mut stroke = Stroke::new();
            stroke.push(sx, y, 0.9);
            stroke.push(ex, y, 0.9);
            let dense = crate::on_device::densify(&stroke, 6.0);
            pen.stroke_display(&dense, calibration, erase_pps)?;
            y += BAND_SPACING;
            k += 1;
        }
        pen.tool_out()?;
        std::thread::sleep(Duration::from_millis(150));
    }
    // Settle: let the panel catch up to the document before we draw.
    std::thread::sleep(Duration::from_millis(1500));
    Ok(())
}

/// Ink-tracing fade: map the actual drawn lines and run the eraser ALONG them,
/// instead of sweeping the whole (mostly-blank) page. The eraser only travels
/// where ink is — roughly the same path length as drawing it, a fraction of a
/// full-area sweep. And it fades beautifully: erase the traced lines in
/// scattered order and the drawing's own strokes vanish one by one.
///
/// `stages`: >1 spreads the erase over that many scattered beats (the fade);
/// 1 wipes in a single pass.
pub fn fade_page(calibration: &AffineTransform, erase_pps: f64, seed: u64) -> Result<()> {
    use inkling_core::vector::{simplify, thin_zhang_suen, trace_skeleton, Pt};
    const CORNER: u32 = 180;
    const STAGES: usize = 6;

    let img = cap::capture_now()?;
    let (w, h) = img.dimensions();

    // Ink mask (exclude the UI corners), then reduce to 1px centerlines and
    // trace them into polylines — the actual drawn lines.
    let mask: Vec<bool> = img
        .enumerate_pixels()
        .map(|(x, y, p)| {
            let in_corner = (x < CORNER || x >= w - CORNER) && (y < CORNER || y >= h - CORNER);
            !in_corner && p.0[0] < 110
        })
        .collect();
    let ink_count = mask.iter().filter(|&&b| b).count();
    if ink_count < 300 {
        log::info!("fade: page already clean ({ink_count} px)");
        return Ok(());
    }
    let skeleton = thin_zhang_suen(w, h, &mask);
    let raw = trace_skeleton(w, h, &skeleton);
    // Simplify lightly; the 14px eraser tolerates a coarse centerline.
    let mut lines: Vec<Vec<Pt>> = raw.iter().map(|l| simplify(l, 2.0)).filter(|l| l.len() > 1).collect();

    // Order left-to-right by each line's leftmost x, so the eraser wipes
    // across the page from left to right rather than in scattered blocks.
    let _ = seed;
    lines.sort_by(|a, b| {
        let ax = a.iter().map(|p| p.x).fold(f32::MAX, f32::min);
        let bx = b.iter().map(|p| p.x).fold(f32::MAX, f32::min);
        ax.partial_cmp(&bx).unwrap_or(std::cmp::Ordering::Equal)
    });
    let total = lines.len();
    log::info!("fade: tracing {total} ink lines ({ink_count} px) left-to-right over {STAGES} stages");

    // Erase a growing share of the lines each stage. The eraser follows each
    // centerline; a 14px band fully covers the ~2-4px drawn stroke.
    let mut done = 0usize;
    for stage in 0..STAGES {
        let target = (total * (stage + 1)) / STAGES;
        if done >= target {
            continue;
        }
        let mut pen = VirtualPen::open_existing(PEN_NODE)?;
        pen.tool_in(Tool::Rubber)?;
        for line in &lines[done..target] {
            let mut stroke = inkling_core::geometry::Stroke::new();
            for p in line {
                stroke.push(p.x, p.y, 0.9);
            }
            let dense = crate::on_device::densify(&stroke, 5.0);
            pen.stroke_display(&dense, calibration, erase_pps)?;
        }
        pen.tool_out()?;
        done = target;
        std::thread::sleep(Duration::from_millis(300));
    }

    // Cleanup: line-tracing follows centerlines, so it misses SOLID filled
    // areas (dark tyres, dense shading have no centerline). Finish with an
    // AREA-fill erase over whatever ink remains — small scattered blocks,
    // serpentine-filled so solid regions clear too — looped until the page is
    // genuinely clean (with a real settle before each check so we're not
    // reading a stale, still-repainting panel).
    for pass in 0..4u32 {
        std::thread::sleep(Duration::from_millis(700));
        let after = cap::capture_now()?;
        let residual: Vec<bool> = after
            .enumerate_pixels()
            .map(|(x, y, p)| {
                let in_corner = (x < CORNER || x >= w - CORNER) && (y < CORNER || y >= h - CORNER);
                !in_corner && p.0[0] < 110
            })
            .collect();
        let n = residual.iter().filter(|&&b| b).count();
        if n < 600 {
            log::info!("fade: clean after cleanup pass {pass} ({n} px)");
            break;
        }
        log::info!("fade cleanup pass {pass}: {n} residual px");
        let rmask = InkMask::new(w, h, &residual);
        let mut blocks = plan_dissolve(&rmask, 18, 5.0, seed.wrapping_add(pass as u64 + 1));
        // Left-to-right cleanup order too, matching the fade.
        blocks.sort_by(|a, b| {
            let ax = a.points.iter().map(|p| p.x).fold(f32::MAX, f32::min);
            let bx = b.points.iter().map(|p| p.x).fold(f32::MAX, f32::min);
            ax.partial_cmp(&bx).unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut pen = VirtualPen::open_existing(PEN_NODE)?;
        pen.tool_in(Tool::Rubber)?;
        for b in &blocks {
            let dense = crate::on_device::densify(b, 5.0);
            pen.stroke_display(&dense, calibration, erase_pps)?;
        }
        pen.tool_out()?;
    }
    std::thread::sleep(Duration::from_millis(600));
    Ok(())
}

/// "White box" wipe: fill the drawing's bounding box with the eraser in
/// vertical columns stepping LEFT-TO-RIGHT — like painting a white rectangle
/// across the page. Because it *fills* (not traces), it clears everything
/// including solid dark areas that line-tracing misses. Reads as a white wipe
/// sweeping left to right.
pub fn wipe_page(calibration: &AffineTransform, erase_pps: f64) -> Result<()> {
    use inkling_core::geometry::Stroke;
    const CORNER: u32 = 180;
    const COL_SPACING: f32 = 10.0; // < 12px eraser band → full coverage
    const PAD: f32 = 14.0;

    let img = cap::capture_now()?;
    let (w, h) = img.dimensions();
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (w, h, 0u32, 0u32);
    let mut ink = 0u32;
    for (x, y, p) in img.enumerate_pixels() {
        let in_corner = (x < CORNER || x >= w - CORNER) && (y < CORNER || y >= h - CORNER);
        if !in_corner && p.0[0] < 110 {
            ink += 1;
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
        }
    }
    if ink < 300 {
        log::info!("wipe: page already clean ({ink} px)");
        return Ok(());
    }
    let (y0, y1) = ((min_y as f32 - PAD).max(0.0), (max_y as f32 + PAD).min(h as f32));
    let mut cols: Vec<f32> = Vec::new();
    let mut x = min_x as f32 - PAD;
    while x <= max_x as f32 + PAD {
        cols.push(x);
        x += COL_SPACING;
    }
    log::info!("wipe: {} columns L->R over bbox ({ink} px) @ {erase_pps} pps", cols.len());

    let mut pen = VirtualPen::open_existing(PEN_NODE)?;
    pen.tool_in(Tool::Rubber)?;
    for (i, &cx) in cols.iter().enumerate() {
        // Alternate vertical direction so consecutive columns flow.
        let (sy, ey) = if i % 2 == 0 { (y0, y1) } else { (y1, y0) };
        let mut stroke = Stroke::new();
        stroke.push(cx, sy, 0.9);
        stroke.push(cx, ey, 0.9);
        let dense = crate::on_device::densify(&stroke, 6.0);
        pen.stroke_display(&dense, calibration, erase_pps)?;
    }
    pen.tool_out()?;
    std::thread::sleep(Duration::from_millis(1200)); // settle for panel repaint
    Ok(())
}

// The 16-byte layout below assumes a 32-bit `struct input_event`: an 8-byte
// `timeval` (two 32-bit words) followed by u16 type, u16 code, i32 value. That
// holds on the armv7 target this daemon runs on; on a 64-bit target `timeval`
// is 16 bytes and these offsets would be wrong, so fail the build loudly rather
// than silently misparse pen events.
#[cfg(not(target_pointer_width = "32"))]
compile_error!("parse_event assumes a 32-bit input_event layout (armv7); build for a 32-bit target");

/// Raw 16-byte input_event parse (type at 8, code at 10, value at 12).
fn parse_event(buf: &[u8; 16]) -> (u16, u16, i32) {
    let type_ = u16::from_ne_bytes([buf[8], buf[9]]);
    let code = u16::from_ne_bytes([buf[10], buf[11]]);
    let value = i32::from_ne_bytes([buf[12], buf[13], buf[14], buf[15]]);
    (type_, code, value)
}

fn set_nonblocking(fd: i32) -> Result<()> {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        anyhow::ensure!(flags >= 0, "F_GETFL failed");
        anyhow::ensure!(libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) >= 0, "F_SETFL failed");
    }
    Ok(())
}

/// Trigger file watched by the `inklingfb` xovi extension inside xochitl. Touching
/// it makes xochitl clear the current page (ink + text) and refresh the panel.
const CLEAR_TRIGGER: &str = "/tmp/inklingfb_clear";

/// A mid-refresh or mis-addressed framebuffer read shows up as a band of random
/// mid-grey noise; a real e-ink page is almost entirely near-white with crisp
/// near-black ink. Reject frames with an implausible amount of mid-grey so we
/// never spend an API call (and redraw a hallucination) on a garbage capture.
fn looks_like_garbage(img: &image::GrayImage) -> bool {
    let raw = img.as_raw();
    if raw.is_empty() {
        return true;
    }
    let mid = raw.iter().filter(|&&p| (60..=205).contains(&p)).count();
    mid * 100 / raw.len() > 15
}

/// Clear the page via xochitl's own SceneController (the clean native clear).
/// The extension deletes the trigger file once it has handled the clear, so we
/// poll for that as a liveness check: if it's still there after the timeout the
/// extension isn't loaded/responding, which we log rather than silently redraw
/// on top of the old page. Either way we give the e-ink time to settle.
fn native_clear() -> Result<()> {
    let _ = std::fs::remove_file(CLEAR_TRIGGER); // clear any stale trigger first
    std::fs::write(CLEAR_TRIGGER, b"")?;
    let deadline = Instant::now() + Duration::from_millis(900);
    let mut consumed = false;
    while Instant::now() < deadline {
        if !std::path::Path::new(CLEAR_TRIGGER).exists() {
            consumed = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if !consumed {
        log::warn!("native clear: inklingfb did not consume {CLEAR_TRIGGER} within 900ms — is the extension loaded?");
    }
    std::thread::sleep(Duration::from_millis(600)); // let the e-ink refresh settle
    Ok(())
}

// The pen's coordinate space is rotated 90° vs the framebuffer, so this hourglass
// is drawn "wide" (w>h) with a left/right bow-tie so it reads as an upright ⧗.
const LOAD_CX: f32 = 702.0;
const LOAD_CY: f32 = 936.0;

/// Draw the hourglass outline once — a "working…" indicator so the wait for the
/// illustration isn't a dead blank screen.
fn draw_loading_hourglass(pen: &mut VirtualPen, calibration: &AffineTransform, pps: f64) -> Result<()> {
    use inkling_core::geometry::Stroke;
    let (cx, cy) = (LOAD_CX, LOAD_CY);
    let (w, h) = (85.0_f32, 60.0_f32);
    // TL -> BL -> TR -> BR -> TL: vertical left/right edges with crossing diagonals.
    let corners = [
        (cx - w, cy - h), (cx - w, cy + h), (cx + w, cy - h), (cx + w, cy + h), (cx - w, cy - h),
    ];
    let mut s = Stroke::new();
    for (x, y) in corners { s.push(x, y, 0.85); }
    let dense = crate::on_device::densify(&s, 3.0);
    pen.stroke_display(&dense, calibration, pps)?;
    Ok(())
}

/// One animation frame: add a short radial tick around the hourglass. Called
/// repeatedly while generating, they sweep round like a loading spinner. Circular
/// so it's rotation-agnostic; additive (cleared with everything before the redraw).
fn animate_loading(pen: &mut VirtualPen, calibration: &AffineTransform, pps: f64, frame: u32) {
    use inkling_core::geometry::Stroke;
    let (cx, cy) = (LOAD_CX, LOAD_CY);
    // Ring sits well clear of the hourglass corners (~104px from centre) so the
    // spinner ticks don't crowd it.
    let (r0, r1) = (150.0_f32, 180.0_f32);
    let ang = (frame % 12) as f32 * std::f32::consts::TAU / 12.0;
    let (ca, sa) = (ang.cos(), ang.sin());
    let mut s = Stroke::new();
    s.push(cx + r0 * ca, cy + r0 * sa, 0.8);
    s.push(cx + r1 * ca, cy + r1 * sa, 0.8);
    let dense = crate::on_device::densify(&s, 3.0);
    let _ = pen.stroke_display(&dense, calibration, pps);
}

pub fn run(config: DaemonConfig) -> Result<()> {
    anyhow::ensure!(!config.api_key.is_empty(), "imagegen api_key is required (config [imagegen] api_key)");

    // Single-instance guard. Two daemons would each see the other's drawn
    // illustration as fresh user ink and trigger each other in an endless loop
    // (and double-bill the API). Hold an exclusive lock for our whole lifetime.
    let lock = std::fs::OpenOptions::new().create(true).write(true).open("/tmp/inkling.lock")
        .context("opening /tmp/inkling.lock")?;
    let locked = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    anyhow::ensure!(locked, "another inkling daemon is already running (/tmp/inkling.lock is held) — refusing to start a second");
    std::mem::forget(lock); // keep the fd (and the lock) for the process lifetime
    let calibration = crate::on_device::load_calibration(&config.calibration_path)?;
    let client = OpenRouterClient::new(config.api_key.clone(), config.model.clone());
    std::fs::create_dir_all(&config.archive_dir).ok();

    if config.trigger_mode == TriggerMode::Selection {
        return run_selection_mode(&config, &client, &calibration);
    }

    let mut event_file = std::fs::File::open(PEN_NODE).with_context(|| format!("opening {PEN_NODE} for watching"))?;
    set_nonblocking(event_file.as_raw_fd())?;

    let mut watcher = SessionWatcher::new(config.dwell_s, config.rate_limit_s);
    let started = Instant::now();
    let now_s = |i: Instant| i.elapsed().as_secs_f64();
    let _ = now_s;

    // Baseline for the new-ink gate: whatever is on screen at startup.
    let mut baseline = cap::capture_now()?;
    log::info!("inkling daemon started (dwell {}s, model {})", config.dwell_s, config.model.as_deref().unwrap_or(crate::imagegen::DEFAULT_MODEL));

    let mut buf = [0u8; 16];
    loop {
        // Drain any pending pen events.
        loop {
            match event_file.read(&mut buf) {
                Ok(16) => {
                    let (type_, code, value) = parse_event(&buf);
                    if type_ == EV_KEY {
                        let now = started.elapsed().as_secs_f64();
                        match (code, value) {
                            (BTN_TOOL_PEN, 1) | (BTN_TOOL_RUBBER, 1) => watcher.on_pen_event(PenEvent::ToolIn, now),
                            (BTN_TOOL_PEN, 0) | (BTN_TOOL_RUBBER, 0) => watcher.on_pen_event(PenEvent::ToolOut, now),
                            (BTN_TOUCH, 1) => watcher.on_pen_event(PenEvent::TouchDown, now),
                            (BTN_TOUCH, 0) => watcher.on_pen_event(PenEvent::TouchUp, now),
                            _ => {}
                        }
                    }
                }
                Ok(_) => {}
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                // A signal (EINTR, common around the rM2's aggressive suspend) or a
                // transient device hiccup must not kill the daemon — retry / resume.
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    log::warn!("pen read error (resuming watch): {e}");
                    break;
                }
            }
        }

        let now = started.elapsed().as_secs_f64();
        if watcher.should_trigger(now) && !std::path::Path::new(&config.pause_file).exists() {
            // Arm the rate limit for EVERY attempt (this was never called before,
            // so rate_limit_s did nothing). It's the universal backoff that stops
            // a failed or garbage cycle from re-firing on the very next 50 ms tick.
            watcher.begin_asking(now);
            match run_cycle(&config, &client, &calibration, &baseline) {
                Ok(Outcome::Drawn(new_baseline)) => {
                    baseline = new_baseline;
                    watcher.complete_cycle(true);
                    log::info!("cycle complete");
                }
                Ok(Outcome::NoNewInk) => {
                    watcher.clear_ink_dirty();
                    watcher.complete_cycle(false);
                }
                Ok(Outcome::Garbage) => {
                    // Keep ink_dirty — the real sketch is still pending; the armed
                    // rate limit backs us off so we don't hammer on a bad read.
                    watcher.complete_cycle(false);
                }
                Err(e) => {
                    log::error!("cycle failed: {e:#}");
                    // run_cycle restored the sketch and left `baseline` untouched,
                    // so it still reads as new ink vs the last good baseline and
                    // will retry once the rate-limit window elapses.
                    watcher.complete_cycle(false);
                }
            }
            // Our stroke injection writes to the same evdev node we watch, so the
            // kernel echoes those events back to us. Discard everything queued
            // during the cycle (our own injected strokes, the restored sketch, the
            // hourglass) so our own drawing isn't mistaken for the user resuming.
            // Genuine new user ink is still caught by the capture-based new-ink
            // gate on the next cycle, so nothing is lost by discarding here.
            while let Ok(16) = event_file.read(&mut buf) {}
        }

        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Outcome of one cycle attempt.
enum Outcome {
    /// Illustration generated and drawn — carries the post-draw capture as the
    /// new baseline.
    Drawn(image::GrayImage),
    /// New-ink gate declined (nothing meaningfully new) — clear ink_dirty.
    NoNewInk,
    /// Capture looked like noise (framebuffer mis-aligned/mid-refresh) — KEEP
    /// ink_dirty and retry later; do not disarm on a bad read.
    Garbage,
}

/// Vectorize a PNG (in the landscape orientation the model works in) and draw it
/// onto the page as pen strokes. Used for both the illustration and, on failure,
/// restoring the user's sketch.
fn draw_png(png: &[u8], calibration: &AffineTransform, config: &DaemonConfig) -> Result<()> {
    let tmp = "/tmp/inkling_draw.png";
    std::fs::write(tmp, png)?;
    let (vec_result, _, _) = crate::vectorize_for_page(tmp, true, config.max_points)?;
    std::thread::sleep(Duration::from_millis(100));
    log::info!("drawing {} strokes...", vec_result.strokes.len());
    let mut pen = VirtualPen::open_existing(PEN_NODE)?;
    pen.tool_in(Tool::Pen)?;
    for s in &vec_result.strokes {
        let dense = crate::on_device::densify(s, 3.0);
        pen.stroke_display(&dense, calibration, config.draw_pps)?;
    }
    pen.tool_out()?;
    Ok(())
}

// --- selection-convert mode ([mode] trigger = "selection") ---
// File contract with the inklingfb extension + qmldiff "Convert" button:
//   /tmp/inkling_convert_go : extension signals a convert is requested (bounds written)
//   /tmp/inkling_selbounds  : "x y w h" — selection bbox in portrait framebuffer px
//   /tmp/inklingfb_delsel   : daemon asks the extension to delete the selected strokes
const CONVERT_GO: &str = "/tmp/inkling_convert_go";
const SELBOUNDS: &str = "/tmp/inkling_selbounds";
const DELSEL_TRIGGER: &str = "/tmp/inklingfb_delsel";

fn read_selbounds() -> Result<(u32, u32, u32, u32)> {
    let s = std::fs::read_to_string(SELBOUNDS).context("reading selection bounds")?;
    let v: Vec<u32> = s.split_whitespace().filter_map(|t| t.parse().ok()).collect();
    anyhow::ensure!(v.len() == 4, "selection bounds must be 'x y w h', got {s:?}");
    Ok((v[0], v[1], v[2], v[3]))
}

/// Draw an illustration fitted into a selection bounding box (portrait page px).
fn draw_png_bounds(png: &[u8], region: (u32, u32, u32, u32), calibration: &AffineTransform, config: &DaemonConfig) -> Result<()> {
    let tmp = "/tmp/inkling_draw.png";
    std::fs::write(tmp, png)?;
    let (vec_result, _, _) = crate::vectorize_for_bounds(tmp, true, config.max_points, region)?;
    log::info!("drawing {} strokes into selection...", vec_result.strokes.len());
    let mut pen = VirtualPen::open_existing(PEN_NODE)?;
    pen.tool_in(Tool::Pen)?;
    for s in &vec_result.strokes {
        let dense = crate::on_device::densify(s, 3.0);
        pen.stroke_display(&dense, calibration, config.draw_pps)?;
    }
    pen.tool_out()?;
    Ok(())
}

/// One selection→illustration conversion: crop the capture to the selection bounds,
/// archive before, generate, archive after, delete the selected strokes, draw fitted.
fn convert_selection(config: &DaemonConfig, client: &OpenRouterClient, calibration: &AffineTransform) -> Result<()> {
    let (mut bx, mut by, mut bw, mut bh) = read_selbounds()?;
    let frame = cap::capture_now()?;
    if looks_like_garbage(&frame) {
        anyhow::bail!("capture looks like noise (framebuffer mis-aligned or panel asleep) — aborting convert");
    }
    // Clamp the region to the frame so an off/oversized bbox can't panic the crop.
    let (fw, fh) = (frame.width(), frame.height());
    bx = bx.min(fw.saturating_sub(1));
    by = by.min(fh.saturating_sub(1));
    bw = bw.min(fw - bx).max(1);
    bh = bh.min(fh - by).max(1);
    log::info!("converting selection {bw}x{bh} at ({bx},{by})");

    // History, named by timestamp. BEFORE = the whole screen (landscape view).
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
    std::fs::create_dir_all(&config.archive_dir).ok();
    let _ = save_gray_png(&image::imageops::rotate270(&frame), &format!("{}/{ts}-before.png", config.archive_dir));

    // The model only gets the selection crop (→ landscape drawing view).
    let crop = image::imageops::crop_imm(&frame, bx, by, bw, bh).to_image();
    let land = image::imageops::rotate270(&crop);
    let mut sketch_png = Vec::new();
    image::DynamicImage::ImageLuma8(land)
        .write_to(&mut std::io::Cursor::new(&mut sketch_png), image::ImageFormat::Png)?;

    log::info!("generating illustration...");
    let illus = client.sketch_to_illustration(&sketch_png)?;

    // Remove the user's sketch strokes (the extension deletes the live selection),
    // let the e-ink settle, then draw the illustration into the same bounds.
    std::fs::write(DELSEL_TRIGGER, []).ok();
    std::thread::sleep(Duration::from_millis(900));
    draw_png_bounds(&illus, (bx, by, bw, bh), calibration, config)?;

    // AFTER = the whole screen once the illustration is on the page.
    std::thread::sleep(Duration::from_millis(500));
    if let Ok(after) = cap::capture_now() {
        let _ = save_gray_png(&image::imageops::rotate270(&after), &format!("{}/{ts}-after.png", config.archive_dir));
    }
    log::info!("selection convert complete");
    Ok(())
}

/// Encode a grayscale image to a PNG file.
fn save_gray_png(img: &image::GrayImage, path: &str) -> Result<()> {
    let mut png = Vec::new();
    image::DynamicImage::ImageLuma8(img.clone())
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)?;
    std::fs::write(path, png)?;
    Ok(())
}

/// Selection-driven loop: wait for the extension/qmldiff button to request a convert.
fn run_selection_mode(config: &DaemonConfig, client: &OpenRouterClient, calibration: &AffineTransform) -> Result<()> {
    log::info!(
        "inkling daemon started (SELECTION mode, model {})",
        config.model.as_deref().unwrap_or(crate::imagegen::DEFAULT_MODEL)
    );
    let _ = std::fs::remove_file(CONVERT_GO); // ignore a stale request from a prior run
    loop {
        if std::path::Path::new(CONVERT_GO).exists() {
            if let Err(e) = convert_selection(config, client, calibration) {
                log::error!("selection convert failed: {e:#}");
            }
            let _ = std::fs::remove_file(CONVERT_GO);
            let _ = std::fs::remove_file(SELBOUNDS);
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// Wait for the illustration, showing the animated hourglass while it generates.
fn wait_with_hourglass(
    rx: &std::sync::mpsc::Receiver<Result<Vec<u8>>>,
    calibration: &AffineTransform,
    config: &DaemonConfig,
) -> Result<Vec<u8>> {
    use std::sync::mpsc::TryRecvError;
    match rx.try_recv() {
        Ok(res) => return res, // already here
        Err(TryRecvError::Empty) => {}
        Err(TryRecvError::Disconnected) => anyhow::bail!("generation thread died"),
    }
    log::info!("illustration not ready — showing loading indicator");
    let mut pen = VirtualPen::open_existing(PEN_NODE)?;
    pen.tool_in(Tool::Pen)?;
    draw_loading_hourglass(&mut pen, calibration, config.draw_pps)?;
    let mut frame = 0u32;
    let png = loop {
        match rx.try_recv() {
            Ok(res) => break res?,
            Err(TryRecvError::Empty) => {
                animate_loading(&mut pen, calibration, config.draw_pps, frame);
                frame = frame.wrapping_add(1);
                std::thread::sleep(Duration::from_millis(550));
            }
            Err(TryRecvError::Disconnected) => {
                let _ = pen.tool_out();
                anyhow::bail!("generation thread died")
            }
        }
    };
    pen.tool_out()?;
    Ok(png)
}

/// One full magic cycle. On success the page holds the drawn illustration; on
/// ANY failure the page is cleaned (hourglass/partial output removed) and the
/// user's sketch is restored, so a transient API error never leaves a spinner
/// on the page (which would re-trigger the loop) and never loses their drawing.
fn run_cycle(
    config: &DaemonConfig,
    client: &OpenRouterClient,
    calibration: &AffineTransform,
    baseline: &image::GrayImage,
) -> Result<Outcome> {
    let sketch = cap::capture_now()?;

    // Garbage guard: a mis-aligned / mid-refresh capture is random noise, not a
    // page. Skip (keeping ink_dirty) rather than pay for an API call.
    if looks_like_garbage(&sketch) {
        log::warn!("capture looks like noise (framebuffer mis-aligned or mid-refresh) — skipping cycle");
        return Ok(Outcome::Garbage);
    }

    // New-ink gate: only fresh (darker) ink counts, so erasing doesn't trigger.
    let changed = count_new_ink(baseline.as_raw(), sketch.as_raw(), 30);
    if changed < config.min_new_ink_px {
        log::info!("trigger declined: only {changed} new-ink px");
        return Ok(Outcome::NoNewInk);
    }

    // Archive the sketch, then generate. The model gets the landscape-rotated view.
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
    let dir = format!("{}/{ts}", config.archive_dir);
    std::fs::create_dir_all(&dir).ok();
    let landscape = image::imageops::rotate270(&sketch);
    let mut sketch_png = Vec::new();
    image::DynamicImage::ImageLuma8(landscape)
        .write_to(&mut std::io::Cursor::new(&mut sketch_png), image::ImageFormat::Png)?;
    std::fs::write(format!("{dir}/sketch.png"), &sketch_png).ok();

    log::info!("generating illustration ({changed} new ink px)...");
    let (tx, rx) = std::sync::mpsc::channel();
    {
        let client = client.clone();
        let png = sketch_png.clone();
        std::thread::spawn(move || {
            let _ = tx.send(client.sketch_to_illustration(&png));
        });
    }

    let result = (|| -> Result<image::GrayImage> {
        // Clear the sketch and show the hourglass, overlapping the API round-trip.
        native_clear()?;
        let illustration_png = wait_with_hourglass(&rx, calibration, config)?;
        std::fs::write(format!("{dir}/illustration.png"), &illustration_png).ok();
        native_clear()?; // remove the hourglass before drawing the result
        draw_png(&illustration_png, calibration, config)?;
        std::thread::sleep(Duration::from_millis(600));
        cap::capture_now()
    })();

    let outcome = match result {
        Ok(img) => Ok(Outcome::Drawn(img)),
        Err(e) => {
            // Generation or draw failed. Remove whatever we left on the page
            // (hourglass / partial illustration) so nothing re-triggers, then put
            // the user's sketch back so it isn't lost and can be retried later.
            let _ = native_clear();
            if let Err(re) = draw_png(&sketch_png, calibration, config) {
                log::error!("failed to restore sketch after a failed cycle: {re:#}");
            }
            Err(e)
        }
    };
    outcome
}
