//! Screen capture, ported from `awwaiid/ghostwriter`'s address-finding
//! technique but with geometry corrected against this exact device/firmware
//! (see device_report.md — ghostwriter's own landscape 1872x1404 constants
//! are wrong for this OS build; ours is portrait 1404x1872, verified by
//! stride autocorrelation against real ink).

use anyhow::{bail, Context, Result};
use image::{GrayImage, Luma};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::{Duration, Instant};

pub const WIDTH: u32 = 1404;
pub const HEIGHT: u32 = 1872;
const STRIDE: u64 = WIDTH as u64 * 4;
const FRAME_BYTES: usize = (WIDTH as u64 * HEIGHT as u64 * 4) as usize;

// Primary capture: ask the inklingfb extension to grab the window via
// QQuickWindow::grabWindow() (renders the scene to a QImage on demand). This is
// immune to the /proc/pid/mem address drift that plagues the fallback below —
// see xovi-ext/inklingfb/main.c (grab_screen). Contract:
//   touch /tmp/inkling_grab -> extension writes /tmp/inkling_frame:
//     20-byte header: w, h, bytesPerLine, format, nbytes (int32 LE), then raw pixels.
const GRAB_TRIGGER: &str = "/tmp/inkling_grab";
const GRAB_FRAME: &str = "/tmp/inkling_frame";

fn capture_via_grab() -> Result<GrayImage> {
    let _ = fs::remove_file(GRAB_FRAME);
    fs::write(GRAB_TRIGGER, []).context("writing grab trigger")?;
    let deadline = Instant::now() + Duration::from_millis(1500);
    while !Path::new(GRAB_FRAME).exists() {
        if Instant::now() >= deadline {
            let _ = fs::remove_file(GRAB_TRIGGER);
            bail!("inklingfb grab produced no frame within 1.5s (extension loaded?)");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let buf = fs::read(GRAB_FRAME).context("reading grab frame")?;
    let _ = fs::remove_file(GRAB_FRAME);
    if buf.len() < 20 {
        bail!("grab frame too short ({} bytes)", buf.len());
    }
    let rd = |i: usize| i32::from_le_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]) as usize;
    let (w, h, bpl) = (rd(0), rd(4), rd(8));
    if w != WIDTH as usize || h != HEIGHT as usize {
        bail!("grab returned unexpected dimensions {w}x{h}");
    }
    let px = &buf[20..];
    if px.len() < h * bpl {
        bail!("grab frame pixel data too short");
    }
    // 32-bit RGB32/ARGB32 (little-endian B,G,R,A) — content is grayscale, take green.
    let mut img = GrayImage::new(WIDTH, HEIGHT);
    for y in 0..h {
        let ro = y * bpl;
        for x in 0..w {
            img.put_pixel(x as u32, y as u32, Luma([px[ro + x * 4 + 1]]));
        }
    }
    Ok(img)
}

fn firmware_bytes_per_pixel_and_offset() -> Result<(u64, u64)> {
    let os_release = fs::read_to_string("/etc/os-release").context("reading /etc/os-release")?;
    let img_version = os_release
        .lines()
        .find_map(|l| l.strip_prefix("IMG_VERSION="))
        .map(|v| v.trim_matches('"'))
        .unwrap_or("0.0");
    let mut parts = img_version.split('.');
    let major: u32 = parts.next().unwrap_or("0").parse().unwrap_or(0);
    let minor: u32 = parts.next().unwrap_or("0").parse().unwrap_or(0);
    if major > 3 || (major == 3 && minor >= 24) {
        Ok((4, 2_629_632))
    } else {
        // Unverified on this codebase — no pre-3.24 hardware available.
        // ghostwriter's own constants for that branch: 2 bpp, offset 0.
        Ok((2, 0))
    }
}

pub fn find_xochitl_pid() -> Result<i32> {
    let output = std::process::Command::new("pidof").arg("xochitl").output().context("running pidof xochitl")?;
    let s = String::from_utf8_lossy(&output.stdout);
    let pid = s.split_whitespace().next().context("no xochitl process found")?;
    pid.parse::<i32>().context("parsing xochitl pid")
}

/// Find the framebuffer read address: the first anonymous rw mapping *after*
/// `/dev/fb0` large enough to hold `pointer_offset + one frame`. On a cleanly booted
/// device that is the aligned panel (verified). NOTE: after many xochitl restarts the
/// mapping order drifts and this can read garbage — use a fresh reboot for reliable
/// capture. See device_report.md for the offset derivation.
pub fn find_capture_address(pid: i32) -> Result<u64> {
    let maps = fs::read_to_string(format!("/proc/{pid}/maps")).context("reading /proc/pid/maps")?;
    let lines: Vec<&str> = maps.lines().collect();
    let fb0_idx = lines
        .iter()
        .position(|l| l.contains("/dev/fb0"))
        .context("no /dev/fb0 mapping found in xochitl maps — OS layout may have changed, see DESIGN.md re-discovery notes")?;

    let (bpp, pointer_offset) = firmware_bytes_per_pixel_and_offset()?;
    if bpp != 4 {
        bail!(
            "firmware uses {bpp} bytes/pixel — this capture path is only verified for the \
             firmware>=3.24 4bpp BGRA layout (device_report.md); re-run M0 recon on this OS version"
        );
    }
    let need = pointer_offset + FRAME_BYTES as u64;

    let parse = |line: &str| -> Option<(u64, u64, bool)> {
        let mut it = line.split_whitespace();
        let range = it.next()?;
        let perms = it.next()?;
        let (s, e) = range.split_once('-')?;
        let start = u64::from_str_radix(s, 16).ok()?;
        let end = u64::from_str_radix(e, 16).ok()?;
        let anon = line.split_whitespace().nth(5).is_none();
        Some((start, end.saturating_sub(start), anon && perms.starts_with("rw")))
    };

    for line in &lines[fb0_idx + 1..] {
        if let Some((start, size, usable)) = parse(line) {
            if usable && size >= need {
                return Ok(start + pointer_offset + 8);
            }
        }
    }
    bail!(
        "no anonymous writable mapping large enough for a frame found after /dev/fb0 — \
         the memory layout may have drifted; a fresh device reboot restores it"
    )
}

/// Read one frame and decode it as the verified portrait BGRA layout.
/// Content is grayscale in practice (B=G=R); we take the green channel.
pub fn capture_frame(pid: i32, addr: u64) -> Result<GrayImage> {
    let mut mem = fs::File::open(format!("/proc/{pid}/mem")).context("opening /proc/pid/mem")?;
    mem.seek(SeekFrom::Start(addr)).context("seeking to framebuffer address")?;
    let mut buf = vec![0u8; FRAME_BYTES];
    mem.read_exact(&mut buf).context("reading framebuffer bytes")?;

    let mut img = GrayImage::new(WIDTH, HEIGHT);
    for y in 0..HEIGHT as u64 {
        let row_off = (y * STRIDE) as usize;
        for x in 0..WIDTH as usize {
            let px_off = row_off + x * 4;
            // BGRA — green channel as the gray value.
            let gray = buf[px_off + 1];
            img.put_pixel(x as u32, y as u32, Luma([gray]));
        }
    }
    Ok(img)
}

pub fn capture_now() -> Result<GrayImage> {
    // Prefer the xovi grabWindow path (drift-immune); fall back to /proc/pid/mem.
    match capture_via_grab() {
        Ok(img) => Ok(img),
        Err(e) => {
            log::warn!("grab capture unavailable ({e}); falling back to /proc/pid/mem");
            let pid = find_xochitl_pid()?;
            let addr = find_capture_address(pid)?;
            capture_frame(pid, addr)
        }
    }
}
