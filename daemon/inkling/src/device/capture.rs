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

/// Find the read address. The framebuffer backing store is the first anonymous
/// (unnamed) writable mapping *after* `/dev/fb0` in xochitl's maps that is large
/// enough to actually hold `pointer_offset + one frame`. The original code always
/// took the very next line, but after xochitl restarts the mapping order shifts and
/// that line can be a small (~3.6 MB) unrelated region — reading from it returns
/// garbage. Requiring the mapping to be big enough skips those and locks onto the
/// real ~13 MB framebuffer. See device_report.md for the offset derivation.
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
    let need = pointer_offset + FRAME_BYTES as u64; // bytes we must be able to read from base

    // Parse a maps line: "START-END perms offset dev inode [pathname]".
    let parse = |line: &str| -> Option<(u64, u64, bool)> {
        let mut it = line.split_whitespace();
        let range = it.next()?;
        let perms = it.next()?;
        let (s, e) = range.split_once('-')?;
        let start = u64::from_str_radix(s, 16).ok()?;
        let end = u64::from_str_radix(e, 16).ok()?;
        let anon = line.split_whitespace().nth(5).is_none(); // no pathname column
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
        "no anonymous writable mapping large enough for a frame ({} bytes) found after /dev/fb0 — \
         framebuffer layout may have changed (see device_report.md); a fresh xochitl restart usually re-aligns it",
        need
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
