//! Screen capture, ported from `awwaiid/ghostwriter`'s address-finding
//! technique but with geometry corrected against this exact device/firmware
//! (see device_report.md — ghostwriter's own landscape 1872x1404 constants
//! are wrong for this OS build; ours is portrait 1404x1872, verified by
//! stride autocorrelation against real ink).

use anyhow::{bail, Context, Result};
use image::{GrayImage, Luma};
use std::fs;
use std::io::{Read, Seek, SeekFrom};

pub const WIDTH: u32 = 1404;
pub const HEIGHT: u32 = 1872;
const STRIDE: u64 = WIDTH as u64 * 4;
const FRAME_BYTES: usize = (WIDTH as u64 * HEIGHT as u64 * 4) as usize;

// NOTE: an in-xochitl capture path (inklingfb extension hooking the QImage ctor to
// hand us its live panel buffer) was tried and reverted — hooking that hot, threaded
// ctor destabilises xochitl's renderer (see xovi-ext/inklingfb/main.c). Capture stays
// here, reading the framebuffer out of xochitl's /proc/pid/mem.

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

/// Read a strip at a candidate address and return the fraction of pixels that are
/// grayscale (B≈G≈R) AND near-white — i.e. e-ink "paper". A real page (blank or inked)
/// is predominantly white, scoring high; an all-black/garbage heap region scores ~0
/// (crucially: near-black is NOT counted, or a zeroed region would look like all-ink).
/// Used only to validate/rank framebuffer candidates.
fn screen_score(mem: &mut fs::File, addr: u64) -> f64 {
    const SAMPLE_ROWS: usize = 300;
    if mem.seek(SeekFrom::Start(addr)).is_err() {
        return -1.0;
    }
    let mut buf = vec![0u8; SAMPLE_ROWS * STRIDE as usize];
    if mem.read_exact(&mut buf).is_err() {
        return -1.0;
    }
    let (mut white, mut ink, mut total) = (0u64, 0u64, 0u64);
    let (mut dup, mut dup_total) = (0u64, 0u64);
    let half = WIDTH as usize / 2;
    let mut y = 0;
    while y < SAMPLE_ROWS {
        let ro = y * STRIDE as usize;
        let mut x = 0;
        while x < WIDTH as usize {
            let o = ro + x * 4;
            let (b, g, r) = (buf[o] as i32, buf[o + 1] as i32, buf[o + 2] as i32);
            let gray = (b - g).abs() < 12 && (r - g).abs() < 12;
            if gray && g > 200 {
                white += 1;
            }
            if gray && g < 100 {
                ink += 1;
            }
            total += 1;
            // Left/right equality: a 2 bytes/px buffer misread at 4 bytes/px tiles each
            // row, so pixel x == pixel x+half everywhere.
            if x < half {
                let g2 = buf[ro + (x + half) * 4 + 1] as i32;
                if (g - g2).abs() < 4 {
                    dup += 1;
                }
                dup_total += 1;
            }
            x += 4;
        }
        y += 4;
    }
    if total == 0 {
        return 0.0;
    }
    let dup_frac = if dup_total == 0 { 0.0 } else { dup as f64 / dup_total as f64 };
    let ink_frac = ink as f64 / total as f64;
    // A tiled decoy has near-perfect left/right equality AND real content. A genuinely
    // blank page is also left/right-equal but has ~no ink, so don't reject that.
    if dup_frac > 0.9 && ink_frac > 0.02 {
        return 0.0;
    }
    white as f64 / total as f64
}

/// Find the framebuffer read address. The backing store is the first anonymous rw
/// mapping *after* `/dev/fb0` large enough to hold `pointer_offset + one frame`; on a
/// clean boot that's the aligned ~13 MB panel (verified). After *many* xochitl restarts
/// the mapping order can drift onto garbage, so we validate the pick with `screen_score`
/// and, if it doesn't look like a screen, scan the other large anon mappings for one
/// that does. A device reboot restores the clean aligned layout. See device_report.md.
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

    let mut mem = fs::File::open(format!("/proc/{pid}/mem")).context("opening /proc/pid/mem")?;

    // Primary: the first big anon mapping after fb0 (the aligned panel on a clean boot).
    let mut positional = None;
    for line in &lines[fb0_idx + 1..] {
        if let Some((start, size, usable)) = parse(line) {
            if usable && size >= need {
                positional = Some(start + pointer_offset + 8);
                break;
            }
        }
    }
    if let Some(addr) = positional {
        if screen_score(&mut mem, addr) >= 0.35 {
            return Ok(addr);
        }
    }

    // Fallback (drifted layout): validate any large anon mapping and take the best.
    let mut best = (-1.0f64, 0u64);
    for line in &lines {
        if let Some((start, size, usable)) = parse(line) {
            if usable && size >= need {
                let addr = start + pointer_offset + 8;
                let s = screen_score(&mut mem, addr);
                if s > best.0 {
                    best = (s, addr);
                }
            }
        }
    }
    if best.0 >= 0.35 {
        return Ok(best.1);
    }
    bail!(
        "no framebuffer-looking mapping found after /dev/fb0 (best screen-score {:.2}); \
         the memory layout has drifted — a device reboot restores the aligned framebuffer",
        best.0
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
    let pid = find_xochitl_pid()?;
    let addr = find_capture_address(pid)?;
    capture_frame(pid, addr)
}
