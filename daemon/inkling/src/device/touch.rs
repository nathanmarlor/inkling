//! Touchscreen (finger) injection via `/dev/input/event2` — MT protocol type-B.
//! Needed to tap xochitl's own UI buttons (e.g. the selection toolbar's trash),
//! which ignore the pen/digitizer. Coordinate mapping (verified, "mode 1"):
//! touch_x = screen_x, touch_y = 1871 - screen_y, for the portrait 1404x1872 panel.

use anyhow::{Context, Result};
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::io::AsRawFd;

const TOUCH_NODE: &str = "/dev/input/event2";

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_ABS: u16 = 0x03;
const SYN_REPORT: u16 = 0x00;
const BTN_TOUCH: u16 = 0x14a;
const ABS_MT_SLOT: u16 = 0x2f;
const ABS_MT_POSITION_X: u16 = 0x35;
const ABS_MT_POSITION_Y: u16 = 0x36;
const ABS_MT_TRACKING_ID: u16 = 0x39;

const PANEL_H: i32 = 1871;

fn write_event(f: &mut std::fs::File, type_: u16, code: u16, value: i32) -> Result<()> {
    // 32-bit input_event: 8-byte timeval (zeroed) + u16 type + u16 code + i32 value.
    let mut buf = [0u8; 16];
    buf[8..10].copy_from_slice(&type_.to_ne_bytes());
    buf[10..12].copy_from_slice(&code.to_ne_bytes());
    buf[12..16].copy_from_slice(&value.to_ne_bytes());
    f.write_all(&buf).context("writing touch event")?;
    Ok(())
}

/// Tap the screen once at portrait framebuffer coords (screen_x, screen_y).
pub fn tap(screen_x: u32, screen_y: u32) -> Result<()> {
    let mut f = OpenOptions::new().write(true).open(TOUCH_NODE).with_context(|| format!("opening {TOUCH_NODE}"))?;
    let _ = f.as_raw_fd();
    let tx = screen_x as i32;
    let ty = (PANEL_H - screen_y as i32).clamp(0, PANEL_H);

    // Finger down.
    write_event(&mut f, EV_ABS, ABS_MT_SLOT, 0)?;
    write_event(&mut f, EV_ABS, ABS_MT_TRACKING_ID, 200)?;
    write_event(&mut f, EV_ABS, ABS_MT_POSITION_X, tx)?;
    write_event(&mut f, EV_ABS, ABS_MT_POSITION_Y, ty)?;
    write_event(&mut f, EV_KEY, BTN_TOUCH, 1)?;
    write_event(&mut f, EV_SYN, SYN_REPORT, 0)?;
    std::thread::sleep(std::time::Duration::from_millis(80));

    // Finger up.
    write_event(&mut f, EV_ABS, ABS_MT_SLOT, 0)?;
    write_event(&mut f, EV_ABS, ABS_MT_TRACKING_ID, -1)?;
    write_event(&mut f, EV_KEY, BTN_TOUCH, 0)?;
    write_event(&mut f, EV_SYN, SYN_REPORT, 0)?;
    Ok(())
}
