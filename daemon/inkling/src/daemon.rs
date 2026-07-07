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
use crate::device::touch;
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
    /// New flow: the user selects strokes and taps the "AI" selection-menu button,
    /// and only that selection is turned into the illustration, fitted to its bounds.
    Selection,
}

/// Which way the artist holds the tablet — i.e. the orientation of the sketch the
/// model should see, and which way up to draw the spinner.
///
/// NOTE: the selection flow is coordinate-correct only in **portrait** (the native
/// framebuffer orientation). Held landscape, xochitl reports selection coordinates in
/// rotated space while the capture and pen digitizer stay portrait, so selection and
/// redraw land in the wrong place. `Landscape`/`Auto` therefore only rotate the model
/// input + spinner; they do NOT fix that coordinate mismatch. Default is `Portrait`.
/// `Auto` follows the document's DocumentView.portrait flag (can be unreliable when
/// xochitl pools views — pin `portrait`/`landscape` if it flip-flops).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Orientation {
    Auto,
    Landscape,
    Portrait,
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
    pub orientation: Orientation,
    /// TTF used to render handwritten-question answers back onto the page.
    pub answer_font: String,
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
            // Portrait is the only coordinate-correct selection orientation (see the
            // Orientation note). `landscape`/`auto` are opt-in via [mode] orientation.
            orientation: Orientation::Portrait,
            // reMarkable ships Noto Sans; readable and already on the device.
            answer_font: "/usr/share/fonts/ttf/noto/NotoSans-Regular.ttf".into(),
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
    // The selection overlay is a legitimate solid mid-grey fill (~194) that can cover
    // a large area — a big selection must not read as noise. Noise is SPREAD across
    // many grey values; a fill is one tone. Score mid-grey excluding the dominant
    // tone (±6 to cover dithering at the fill edges).
    let mut hist = [0usize; 256];
    for &p in raw {
        hist[p as usize] += 1;
    }
    let modal = (60..=205usize).max_by_key(|&v| hist[v]).unwrap_or(60);
    let lo = modal.saturating_sub(6);
    let hi = (modal + 6).min(205);
    let mid: usize = (60..=205usize).filter(|v| !(lo..=hi).contains(v)).map(|v| hist[v]).sum();
    if mid * 100 / raw.len() > 15 {
        return true;
    }
    // A real page is mostly paper. An all-black or all-dark frame (mis-aligned /proc
    // read, sleep screen) has no mid-grey at all and sails past the noise check —
    // one such frame fed the model a black rectangle and it drew dense mud.
    let near_white: usize = hist[206..].iter().sum();
    near_white * 100 / raw.len() < 50
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

const LOAD_CX: f32 = 702.0;
const LOAD_CY: f32 = 936.0;

/// Radius of the full spinner (ring ticks) at scale 1.0 — used both to draw it and
/// to bound the native select-and-delete that removes it in selection mode.
const LOAD_RADIUS: f32 = 180.0;

/// Draw the hourglass outline once — a "working…" indicator so the wait for the
/// illustration isn't a dead blank screen. `k` scales the figure (1.0 = full-page).
///
/// The hourglass is two straight edges joined by crossing diagonals. WHICH pair of
/// edges is straight decides the axis, so the two orientations use different corner
/// orders (swapping w/h alone only stretches the same bow-tie — the bug the artist
/// saw). Held landscape the panel is rotated 90° from the artist, so the framebuffer
/// figure is a horizontal bow-tie; held portrait it's an upright ⧗. Either way it
/// reads upright to the artist.
fn draw_loading_hourglass_at(pen: &mut VirtualPen, calibration: &AffineTransform, pps: f64, cx: f32, cy: f32, k: f32, landscape: bool) -> Result<()> {
    use inkling_core::geometry::Stroke;
    let tl = |w: f32, h: f32| (cx - w, cy - h);
    let tr = |w: f32, h: f32| (cx + w, cy - h);
    let bl = |w: f32, h: f32| (cx - w, cy + h);
    let br = |w: f32, h: f32| (cx + w, cy + h);
    let corners = if landscape {
        // vertical straight edges + X = bow-tie lying on its side (reads upright when
        // the device is held landscape). TL -> BL -> TR -> BR -> TL.
        let (w, h) = (85.0_f32 * k, 60.0_f32 * k);
        [tl(w, h), bl(w, h), tr(w, h), br(w, h), tl(w, h)]
    } else {
        // horizontal straight edges (top & bottom) + X = upright hourglass ⧗.
        // TL -> TR -> BL -> BR -> TL.
        let (w, h) = (60.0_f32 * k, 85.0_f32 * k);
        [tl(w, h), tr(w, h), bl(w, h), br(w, h), tl(w, h)]
    };
    let mut s = Stroke::new();
    for (x, y) in corners { s.push(x, y, 0.85); }
    let dense = crate::on_device::densify(&s, 3.0);
    pen.stroke_display(&dense, calibration, pps)?;
    Ok(())
}

/// One animation frame: add a short radial tick around the hourglass. Called
/// repeatedly while generating, they sweep round like a loading spinner. Circular
/// so it's rotation-agnostic; additive (cleared with everything before the redraw).
fn animate_loading_at(pen: &mut VirtualPen, calibration: &AffineTransform, pps: f64, frame: u32, cx: f32, cy: f32, k: f32) {
    use inkling_core::geometry::Stroke;
    // Ring sits well clear of the hourglass corners so the spinner ticks don't crowd it.
    let (r0, r1) = (150.0_f32 * k, LOAD_RADIUS * k);
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
// File contract with the inklingfb extension + the "AI" selection-menu button:
//   /tmp/inkling_convert_go   : extension signals a convert is requested
//   /tmp/inkling_selinfo      : daemon asks for the native selection info; the
//   /tmp/inkling_selinfo_out  :   extension replies "count x y w h portrait" (view px,
//                                 tight bbox of the selected strokes — SceneSelection-
//                                 Handler.viewSelectionRect — plus the DocumentView's
//                                 portrait orientation flag)
//   /tmp/inkling_seldelete    : daemon asks for the native delete — the extension emits
//                                 the SceneSelectionHandler's deleteSelection() signal,
//                                 i.e. exactly what the selection menu's trash button
//                                 does (xochitl deletes with its own edit id, undoable)
const CONVERT_GO: &str = "/tmp/inkling_convert_go";
const SELINFO_TRIGGER: &str = "/tmp/inkling_selinfo";
const SELINFO_OUT: &str = "/tmp/inkling_selinfo_out";
const SELDELETE_TRIGGER: &str = "/tmp/inkling_seldelete";
const TOOL_TRIGGER: &str = "/tmp/inkling_tool"; // "pen" | "sel" — native penHandler switch

/// White-out the selection UI's chrome captured inside the crop: the rectangle's
/// black border line (runs along every edge) and the handles that straddle it
/// (squares on the corners, rotate circle at top-centre). Without this the model
/// sees a framed sketch and draws the frame into the illustration.
fn mask_selection_ui(crop: &mut image::GrayImage) {
    let (w, h) = crop.dimensions();
    let mut fill = |x0: u32, y0: u32, fw: u32, fh: u32| {
        for y in y0..(y0 + fh).min(h) {
            for x in x0..(x0 + fw).min(w) {
                crop.put_pixel(x, y, image::Luma([255]));
            }
        }
    };
    const EDGE: u32 = 6; // the border line itself
    const HANDLE: u32 = 26; // half of a corner square that reaches inside the rect
    const ROT_W: u32 = 60; // rotate-circle intrusion at top-centre
    const ROT_H: u32 = 30;
    fill(0, 0, w, EDGE); // top edge
    fill(0, h.saturating_sub(EDGE), w, EDGE); // bottom edge
    fill(0, 0, EDGE, h); // left edge
    fill(w.saturating_sub(EDGE), 0, EDGE, h); // right edge
    fill(0, 0, HANDLE, HANDLE); // corners
    fill(w.saturating_sub(HANDLE), 0, HANDLE, HANDLE);
    fill(0, h.saturating_sub(HANDLE), HANDLE, HANDLE);
    fill(w.saturating_sub(HANDLE), h.saturating_sub(HANDLE), HANDLE, HANDLE);
    fill(w.saturating_sub(ROT_W) / 2, 0, ROT_W, ROT_H); // rotate handle, top-centre
}

/// The live selection as the extension reports it.
struct SelectionInfo {
    /// number of selected strokes (0 = nothing selected)
    count: u32,
    /// document orientation flag (DocumentView.portrait)
    portrait: bool,
    /// selection bbox in xochitl VIEW coords — correct in portrait, but rotated when
    /// the tablet is held landscape, so it is only a fallback for the capture-derived
    /// (panel-space) bounds below.
    view_bounds: (u32, u32, u32, u32),
}

/// Ask the extension for the live selection info. Returns None if there's no live
/// selection (count 0) or no reply.
fn native_selection_info() -> Option<SelectionInfo> {
    let _ = std::fs::remove_file(SELINFO_OUT);
    std::fs::write(SELINFO_TRIGGER, []).ok()?;
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(100));
        let Ok(s) = std::fs::read_to_string(SELINFO_OUT) else { continue };
        let v: Vec<f64> = s.split_whitespace().filter_map(|t| t.parse().ok()).collect();
        if v.len() >= 5 && v[0] >= 1.0 && v[3] >= 4.0 && v[4] >= 4.0 {
            let x = v[1].max(0.0) as u32;
            let y = v[2].max(0.0) as u32;
            let w = (v[3] as u32).min(cap::WIDTH.saturating_sub(x));
            let h = (v[4] as u32).min(cap::HEIGHT.saturating_sub(y));
            let portrait = v.get(5).copied().unwrap_or(0.0) >= 0.5;
            return Some(SelectionInfo { count: v[0] as u32, portrait, view_bounds: (x, y, w, h) });
        }
        return None; // replied, but no live selection
    }
    None
}

/// Locate the selection's bounding box in the CAPTURE (panel px) from its grey
/// overlay fill. Because it reads the actual captured pixels, it is correct in both
/// orientations — unlike the view-space native bounds, which rotate in landscape.
/// xochitl fills the selection with a solid mid-grey (~194); find that grey's dense
/// row/column band (stray grey is sparse; the fill is a solid rectangle).
fn detect_selection_bounds(img: &image::GrayImage) -> Option<(u32, u32, u32, u32)> {
    let (w, h) = (img.width() as usize, img.height() as usize);
    let raw = img.as_raw();
    let mut hist = [0u32; 256];
    for &p in raw {
        if (150..=225).contains(&p) {
            hist[p as usize] += 1;
        }
    }
    let (grey, cnt) = hist.iter().enumerate().max_by_key(|(_, &c)| c).map(|(i, &c)| (i as i32, c))?;
    if cnt < 5000 {
        return None; // no sizeable grey block => no detectable selection fill
    }
    let is_sel = |v: u8| (v as i32 - grey).abs() <= 6;
    let mut rows = vec![0u32; h];
    let mut cols = vec![0u32; w];
    for y in 0..h {
        let ro = y * w;
        for x in 0..w {
            if is_sel(raw[ro + x]) {
                rows[y] += 1;
                cols[x] += 1;
            }
        }
    }
    // Contiguous band where the projection exceeds half its peak = the filled rectangle.
    let band = |arr: &[u32]| -> Option<(usize, usize)> {
        let m = *arr.iter().max()?;
        if m == 0 {
            return None;
        }
        let thr = m / 2;
        Some((arr.iter().position(|&v| v > thr)?, arr.iter().rposition(|&v| v > thr)?))
    };
    let (y0, y1) = band(&rows)?;
    let (x0, x1) = band(&cols)?;
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some((x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32))
}

/// Handwritten-question path: ink the answer just below the question, leaving the
/// question in place. The answer is drawn from the font's own glyph OUTLINES as pen
/// strokes (clean, connected letters) — rasterising then skeleton-tracing broke the
/// letters into spidery fragments. `sel` is the question's panel-px bbox.
fn answer_question(
    answer: &str,
    sel: (u32, u32, u32, u32),
    _landscape: bool,
    calibration: &AffineTransform,
    config: &DaemonConfig,
    _ts: u64,
) -> Result<()> {
    let (bx, by, bw, bh) = sel;
    let width = bw.max(240).min(cap::WIDTH.saturating_sub(bx)).max(1);
    let gap = 24;
    let ox = bx as f32 + 6.0;
    let oy = (by + bh + gap) as f32;
    let strokes = text_outline_strokes(answer, ox, oy, width as f32 - 12.0, &config.answer_font)?;

    // Deselect the question WITHOUT deleting it, then draw. A real toolbar tap on the
    // pen tool both switches tool and clears the active selection — the native
    // property tool-switch does NOT deselect, so injecting the answer while the
    // question was still selected dragged/cut it (the question vanished).
    touch::tap(55, 170).ok();
    std::thread::sleep(Duration::from_millis(600));
    let mut pen = VirtualPen::open_existing(PEN_NODE)?;
    pen.tool_in(Tool::Pen)?;
    for s in &strokes {
        let dense = crate::on_device::densify(s, 3.0);
        pen.stroke_display(&dense, calibration, config.draw_pps)?;
    }
    pen.tool_out()?;
    log::info!("answer inked below the question ({} strokes)", strokes.len());
    Ok(())
}

/// One Hershey glyph: horizontal advance (font units) and a set of pen strokes, each
/// a polyline in font units (origin at the left edge, y increasing downward).
struct HersheyGlyph {
    advance: f32,
    strokes: Vec<Vec<(f32, f32)>>,
}

/// The vendored Hershey "futural" single-stroke (plotter) font. Parsed once. ASCII
/// 32..=126 map to the file's lines in order. Single-stroke = each letter is drawn
/// with centre-line strokes like handwriting, not hollow outlines.
fn hershey_font() -> &'static std::collections::HashMap<char, HersheyGlyph> {
    use std::sync::OnceLock;
    static FONT: OnceLock<std::collections::HashMap<char, HersheyGlyph>> = OnceLock::new();
    FONT.get_or_init(|| {
        const DATA: &str = include_str!("futural.jhf");
        let mut map = std::collections::HashMap::new();
        for (i, line) in DATA.lines().enumerate() {
            let ch = char::from_u32(32 + i as u32).unwrap_or(' ');
            let bytes = line.as_bytes();
            if bytes.len() < 10 { map.insert(ch, HersheyGlyph { advance: 16.0, strokes: vec![] }); continue; }
            // bytes[8],[9] = left/right bearing (char - 'R').
            let left = bytes[8] as f32 - (b'R' as f32);
            let right = bytes[9] as f32 - (b'R' as f32);
            let mut strokes: Vec<Vec<(f32, f32)>> = Vec::new();
            let mut cur: Vec<(f32, f32)> = Vec::new();
            let mut j = 10;
            while j + 1 < bytes.len() {
                let (a, b) = (bytes[j], bytes[j + 1]);
                if a == b' ' {
                    // pen up — end current stroke
                    if cur.len() > 1 { strokes.push(std::mem::take(&mut cur)); } else { cur.clear(); }
                } else {
                    cur.push((a as f32 - (b'R' as f32) - left, b as f32 - (b'R' as f32)));
                }
                j += 2;
            }
            if cur.len() > 1 { strokes.push(cur); }
            map.insert(ch, HersheyGlyph { advance: right - left, strokes });
        }
        map
    })
}

/// Lay `text` out word-wrapped to `max_w` (px) starting at panel px (ox, oy) and
/// return single-stroke Hershey letters as display-space pen strokes.
fn text_outline_strokes(text: &str, ox: f32, oy: f32, max_w: f32, _font_path: &str) -> Result<Vec<inkling_core::geometry::Stroke>> {
    use inkling_core::geometry::Stroke;
    let font = hershey_font();
    // Hershey cap height is ~21 units; scale so caps are ~40px tall.
    let scale = 40.0 / 21.0;
    let line_h = 34.0 * scale; // generous line spacing (units) * scale
    let space_adv = font.get(&' ').map(|g| g.advance).unwrap_or(16.0);

    let word_w = |w: &str| -> f32 {
        w.chars().map(|c| font.get(&c).map(|g| g.advance).unwrap_or(space_adv)).sum::<f32>() * scale
    };
    let space_w = space_adv * scale * 1.3;

    // Word-wrap.
    let mut lines: Vec<String> = Vec::new();
    for para in text.split('\n') {
        let mut line = String::new();
        let mut lw = 0.0;
        for word in para.split_whitespace() {
            let ww = word_w(word);
            if !line.is_empty() && lw + space_w + ww > max_w {
                lines.push(std::mem::take(&mut line));
                lw = 0.0;
            }
            if !line.is_empty() { line.push(' '); lw += space_w; }
            line.push_str(word);
            lw += ww;
        }
        lines.push(line);
    }

    let mut out = Vec::new();
    for (li, line) in lines.iter().enumerate() {
        // Hershey baseline is at y=0 with caps going negative; place the baseline so
        // the tallest caps (~ -21) sit below oy.
        let base_y = oy + 21.0 * scale + li as f32 * line_h;
        let mut caret = ox;
        for c in line.chars() {
            if c == ' ' { caret += space_w; continue; }
            if let Some(g) = font.get(&c) {
                for poly in &g.strokes {
                    let mut st = Stroke::new();
                    for (x, y) in poly {
                        st.push(caret + x * scale, base_y + y * scale, 0.85);
                    }
                    out.push(st);
                }
                caret += g.advance * scale;
            }
        }
    }
    Ok(out)
}

/// Draw an artist-upright illustration fitted into a selection bounding box (panel
/// px). `landscape` = the tablet was held landscape, so the illustration must be
/// rotated back into panel space (270° — the inverse of the 90° applied to upright
/// the sketch for the model) before it is traced. Portrait draws as-is.
fn draw_png_bounds(
    png: &[u8],
    region: (u32, u32, u32, u32),
    landscape: bool,
    calibration: &AffineTransform,
    config: &DaemonConfig,
) -> Result<()> {
    let tmp = "/tmp/inkling_draw.png";
    if landscape {
        // Rotate into panel space here, then trace with no further rotation — keeps
        // vectorize's own `landscape` meaning (used by the CLI/inactivity flow) intact.
        let img = image::load_from_memory(png).context("decoding illustration")?;
        let rotated = image::DynamicImage::ImageRgba8(image::imageops::rotate270(&img.to_rgba8()));
        rotated.save(tmp).context("writing rotated illustration")?;
    } else {
        std::fs::write(tmp, png)?;
    }
    let (vec_result, _, _) = crate::vectorize_for_bounds(tmp, false, config.max_points, region)?;
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

/// The spinner is drawn on a temporary scratch LAYER (extension trigger
/// "begin"/"end"): removing it is a plain native deleteLayer. Never remove it by
/// programmatic selection + deleteSelection — that crashed xochitl twice.
const SPINLAYER_TRIGGER: &str = "/tmp/inkling_spinlayer";

/// Send a spinlayer command (add/remove the scratch layer) and wait a fixed beat for
/// the queued layer switch to take effect. An earlier version POLLED a layerCount
/// readback to confirm, but the readback is deferred (never reflected the change, so
/// it always timed out) AND the ~40 rapid readbacks each triggered a full GUI-thread
/// tree-walk mid-convert, which wedged the e-ink panel (froze the tablet). A plain
/// wait is both correct-enough and safe. Returns true (best-effort).
fn spinlayer(cmd: &[u8]) -> bool {
    let _ = std::fs::write(SPINLAYER_TRIGGER, cmd);
    // Let the trigger be consumed, then let the queued addLayer+setCurrentLayer run.
    let deadline = Instant::now() + Duration::from_millis(600);
    while std::path::Path::new(SPINLAYER_TRIGGER).exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(40));
    }
    std::thread::sleep(Duration::from_millis(700));
    true
}

/// One selection→illustration conversion: grab the screen, find the selection's
/// panel-space bounds, clean the crop, generate, delete the strokes natively, draw
/// fitted.
fn convert_selection(config: &DaemonConfig, client: &OpenRouterClient, calibration: &AffineTransform) -> Result<()> {
    let frame = cap::capture_now()?;
    if looks_like_garbage(&frame) {
        anyhow::bail!("capture looks like noise — aborting convert");
    }
    let sel = native_selection_info()
        .context("no selection detected (select some strokes, then tap AI)")?;
    // Bounds come from the captured grey overlay (PANEL px — correct in both
    // orientations), not the native view rect (which rotates in landscape). Fall back
    // to the view rect only if the overlay isn't detectable (portrait-correct at least).
    let (bx, by, bw, bh) = detect_selection_bounds(&frame).unwrap_or_else(|| {
        log::warn!("selection grey overlay not found in capture; using native view bounds");
        sel.view_bounds
    });
    // The model must see the sketch the way the artist drew it. The document's
    // orientation setting (xochitl's ⋯ menu → Landscape/Portrait) says how the
    // tablet is held; config [mode] orientation can pin it instead of following.
    let landscape = match config.orientation {
        Orientation::Landscape => true,
        Orientation::Portrait => false,
        Orientation::Auto => !sel.portrait,
    };
    log::info!("converting selection {bw}x{bh} at ({bx},{by}), {} item(s), {} input",
        sel.count, if landscape { "landscape" } else { "portrait" });

    // History, named by timestamp. BEFORE = the whole screen, artist-oriented.
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
    std::fs::create_dir_all(&config.archive_dir).ok();
    let before = if landscape { image::imageops::rotate90(&frame) } else { frame.clone() };
    let _ = save_gray_png(&before, &format!("{}/{ts}-before.png", config.archive_dir));

    // Crop the selection and clean it: the grey overlay + paper go white, ink stays
    // black, so the model gets a clean sketch.
    let mut crop = image::imageops::crop_imm(&frame, bx, by, bw, bh).to_image();
    for p in crop.pixels_mut() {
        p.0[0] = if p.0[0] < 150 { 0 } else { 255 };
    }
    // The capture includes the selection UI's black chrome — the rectangle's border
    // line and the corner/rotate handles. Left in, the model faithfully draws a
    // frame box around the illustration. White them out.
    mask_selection_ui(&mut crop);
    // White breathing margin: a tight crop reads as "fill every pixel" and the model
    // comes back denser/muddier than the old full-page flow (which had paper around
    // the sketch). The redraw region is expanded by the same amounts, so the
    // illustration lands exactly where the sketch was.
    let pad_x = (bw as f32 * 0.12) as u32 + 16;
    let pad_y = (bh as f32 * 0.12) as u32 + 16;
    let mut padded = image::GrayImage::from_pixel(bw + 2 * pad_x, bh + 2 * pad_y, image::Luma([255]));
    image::imageops::replace(&mut padded, &crop, pad_x as i64, pad_y as i64);
    let ex = bx.saturating_sub(pad_x);
    let ey = by.saturating_sub(pad_y);
    let ew = (bw + 2 * pad_x).min(cap::WIDTH - ex);
    let eh = (bh + 2 * pad_y).min(cap::HEIGHT - ey);
    let draw_region = (ex, ey, ew, eh);
    // Upright the sketch for the model. Portrait: the panel IS the artist's view.
    // Landscape: xochitl stores the page rotated 90° in the portrait panel, so rotate
    // the crop 90° to the artist's upright view. (draw_png_bounds applies the inverse
    // 270° to place the result back — the pair must round-trip.)
    let oriented = if landscape { image::imageops::rotate90(&padded) } else { padded };
    let mut sketch_png = Vec::new();
    image::DynamicImage::ImageLuma8(oriented)
        .write_to(&mut std::io::Cursor::new(&mut sketch_png), image::ImageFormat::Png)?;
    // Archive the EXACT model input, so "what did it see?" is always answerable.
    std::fs::write(format!("{}/{ts}-sketch.png", config.archive_dir), &sketch_png).ok();

    // Read the selection first: if it's a handwritten question, answer it in ink and
    // leave the question in place; otherwise fall through to the illustration flow.
    match client.answer_if_question(&sketch_png) {
        Ok(Some(answer)) => {
            log::info!("answering question: {answer}");
            return answer_question(&answer, (bx, by, bw, bh), landscape, calibration, config, ts);
        }
        Ok(None) => {} // a drawing — illustrate it
        Err(e) => log::warn!("Q&A read failed ({e:#}); treating as a drawing"),
    }

    // Generate on a thread so the spinner can run during the API round-trip.
    log::info!("generating illustration...");
    let (tx, rx) = std::sync::mpsc::channel();
    {
        let client = client.clone();
        let png = sketch_png.clone();
        std::thread::spawn(move || {
            let _ = tx.send(client.sketch_to_illustration(&png));
        });
    }

    // Native delete: emit the selection menu's own deleteSelection() via the
    // extension — identical to tapping the trash button (correct internal edit id,
    // clean e-ink refresh, undoable), and it closes the selection so the following
    // pen inject draws ink rather than dragging the selection.
    std::fs::write(SELDELETE_TRIGGER, []).ok();
    std::thread::sleep(Duration::from_millis(700));

    // The user selected with the SELECTION tool, and with it active the injected pen
    // strokes are treated as lasso gestures, not ink. Native switch: the extension
    // writes penHandler.{lineTool,gestureMode,lineThickness} on the GUI thread.
    std::fs::write(TOOL_TRIGGER, b"pen").ok();
    std::thread::sleep(Duration::from_millis(500));

    // Loading spinner on a scratch layer, so removal is a single native deleteLayer.
    // Only draw it if the layer switch is CONFIRMED — otherwise the spinner would ink
    // the artist's own layer and survive cleanup (a stray hourglass on the drawing).
    let (scx, scy) = ((bx + bw / 2) as f32, (by + bh / 2) as f32);
    let k = ((bw.min(bh) as f32) / 2.0 - 10.0).clamp(60.0, LOAD_RADIUS) / LOAD_RADIUS;
    let on_scratch = spinlayer(b"begin");
    let gen = if on_scratch {
        let mut _drawn = false;
        let g = wait_with_hourglass_at(&rx, calibration, config, scx, scy, k, landscape, &mut _drawn);
        // Drop the scratch layer (and the spinner with it). Wait for it to actually
        // go before drawing — else the illustration lands on the doomed layer.
        if !spinlayer(b"end") {
            log::warn!("scratch layer not confirmed gone; skipping spinner cleanup wait");
        }
        std::thread::sleep(Duration::from_millis(200));
        g
    } else {
        // No confirmed scratch layer — skip the spinner rather than risk a stray mark;
        // just wait for the illustration.
        log::warn!("scratch layer not confirmed; drawing without spinner");
        rx.recv().unwrap_or_else(|_| anyhow::bail!("generation thread died"))
    };

    let result = match gen {
        Ok(illus) => draw_png_bounds(&illus, draw_region, landscape, calibration, config),
        Err(e) => {
            // Generation failed AFTER the selection was deleted — put the sketch
            // back (redrawn from our cleaned crop) so the kid never loses a drawing
            // to a network/API error. The delete also stays undoable.
            log::warn!("generation failed ({e:#}); restoring the sketch");
            draw_png_bounds(&sketch_png, draw_region, landscape, calibration, config)
                .map_err(|re| re.context("restore-redraw also failed"))
                .and(Err(e))
        }
    };
    // Finish on the PEN tool (toolbar tap so the highlight moves too) — the kid can
    // keep drawing right away instead of accidentally lassoing.
    touch::tap(55, 170).ok();
    result?;

    // NOTE: no "after" capture here. grabWindow right after the illustration
    // injection contends with xochitl's still-settling render and can wedge the
    // e-ink panel (froze the tablet). The before/sketch archives are enough.
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

/// The injected Convert button (a QML star in xochitl's selection menu) fires an XHR
/// here; any request touches the CONVERT_GO file the main loop polls.
fn spawn_convert_listener() {
    std::thread::spawn(|| {
        let listener = match std::net::TcpListener::bind("127.0.0.1:9137") {
            Ok(l) => l,
            Err(e) => {
                log::warn!("convert listener bind failed ({e}); button taps won't work");
                return;
            }
        };
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let mut buf = [0u8; 512];
            let _ = std::io::Read::read(&mut s, &mut buf);
            if buf.starts_with(b"GET /convert") {
                let _ = std::fs::write(CONVERT_GO, []);
                log::info!("convert requested (menu button)");
            }
            let _ = std::io::Write::write_all(
                &mut s,
                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
            );
        }
    });
}

/// Selection-driven loop: wait for the Convert menu button (or a manual touch of the
/// trigger file) to request a convert.
fn run_selection_mode(config: &DaemonConfig, client: &OpenRouterClient, calibration: &AffineTransform) -> Result<()> {
    log::info!(
        "inkling daemon started (SELECTION mode, model {})",
        config.model.as_deref().unwrap_or(crate::imagegen::DEFAULT_MODEL)
    );
    spawn_convert_listener();
    let _ = std::fs::remove_file(CONVERT_GO); // ignore a stale request from a prior run
    loop {
        if std::path::Path::new(CONVERT_GO).exists() {
            if let Err(e) = convert_selection(config, client, calibration) {
                log::error!("selection convert failed: {e:#}");
            }
            let _ = std::fs::remove_file(CONVERT_GO);
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// Wait for the illustration, showing the animated hourglass while it generates.
/// Returns the PNG and whether the spinner was actually drawn (so callers that
/// can't full-page-clear know they must remove it).
/// `drawn` is set as soon as the spinner ink hits the page, so callers can clean
/// it up even when this returns an error mid-wait.
fn wait_with_hourglass_at(
    rx: &std::sync::mpsc::Receiver<Result<Vec<u8>>>,
    calibration: &AffineTransform,
    config: &DaemonConfig,
    cx: f32,
    cy: f32,
    k: f32,
    landscape: bool,
    drawn: &mut bool,
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
    *drawn = true;
    draw_loading_hourglass_at(&mut pen, calibration, config.draw_pps, cx, cy, k, landscape)?;
    let mut frame = 0u32;
    let png = loop {
        match rx.try_recv() {
            Ok(res) => break res?,
            Err(TryRecvError::Empty) => {
                animate_loading_at(&mut pen, calibration, config.draw_pps, frame, cx, cy, k);
                if frame == 1 {
                    // The first outline is inked during the post-delete refresh dead
                    // zone and the panel can skip it (it only "flashed" at the final
                    // repaint). Ink it again once refreshes are flowing — overdraw
                    // on the scratch layer is invisible and gets deleted anyway.
                    let _ = draw_loading_hourglass_at(&mut pen, calibration, config.draw_pps, cx, cy, k, landscape);
                }
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

/// Full-page variant (inactivity mode): spinner at the page centre; the caller
/// clears the whole page afterwards, so the drawn/not-drawn flag is irrelevant.
/// Inactivity mode is landscape-only, matching that flow's fixed rotation.
fn wait_with_hourglass(
    rx: &std::sync::mpsc::Receiver<Result<Vec<u8>>>,
    calibration: &AffineTransform,
    config: &DaemonConfig,
) -> Result<Vec<u8>> {
    let mut drawn = false;
    wait_with_hourglass_at(rx, calibration, config, LOAD_CX, LOAD_CY, 1.0, true, &mut drawn)
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
