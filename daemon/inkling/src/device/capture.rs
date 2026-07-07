//! Screen capture via the inklingfb xovi extension: it renders xochitl's live
//! window with QQuickWindow::grabWindow() and writes the frame to a file. This is
//! the ONLY capture path — the old /proc/pid/mem framebuffer read was removed for
//! production because its address drifts across xochitl restarts and a mis-read
//! once fed the image model a garbage frame.
//!
//! Contract with the extension (xovi-ext/inklingfb/main.c, grab_screen):
//!   touch /tmp/inkling_grab -> extension writes /tmp/inkling_frame:
//!     20-byte header: w, h, bytesPerLine, format, nbytes (int32 LE), then raw
//!     RGB32 pixels (B,G,R,A little-endian; content is grayscale — green taken).

use anyhow::{bail, Context, Result};
use image::{GrayImage, Luma};
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

pub const WIDTH: u32 = 1404;
pub const HEIGHT: u32 = 1872;

const GRAB_TRIGGER: &str = "/tmp/inkling_grab";
const GRAB_FRAME: &str = "/tmp/inkling_frame";

pub fn capture_now() -> Result<GrayImage> {
    let _ = fs::remove_file(GRAB_FRAME);
    fs::write(GRAB_TRIGGER, []).context("writing grab trigger")?;
    let deadline = Instant::now() + Duration::from_millis(1500);
    while !Path::new(GRAB_FRAME).exists() {
        if Instant::now() >= deadline {
            let _ = fs::remove_file(GRAB_TRIGGER);
            bail!("inklingfb grab produced no frame within 1.5s — is the extension loaded? (systemctl restart xochitl loads it)");
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
    let mut img = GrayImage::new(WIDTH, HEIGHT);
    for y in 0..h {
        let ro = y * bpl;
        for x in 0..w {
            img.put_pixel(x as u32, y as u32, Luma([px[ro + x * 4 + 1]]));
        }
    }
    Ok(img)
}
