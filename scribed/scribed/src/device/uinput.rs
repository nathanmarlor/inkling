//! Virtual pen via raw uinput ioctls (linux/uinput.h). Hand-rolled so every
//! ABI detail is visible and verifiable; ioctl request numbers are computed
//! from `size_of::<T>()`, so a wrong struct layout fails cleanly at ioctl
//! time instead of corrupting memory.
//!
//! ABI note (the y2038 trap): the kernel's `struct input_event` timestamp is
//! two `__kernel_ulong_t`s — 32-bit on armv7 — NOT userspace `timeval`.
//! musl 1.2+ has 64-bit time_t, so `libc::timeval` would be 16 bytes and
//! shear every event by 8 bytes. `c_ulong` matches the kernel on both
//! armv7 (u32) and 64-bit hosts (u64). Timestamps we write are ignored;
//! the kernel stamps events at injection time.
//!
//! Real digitizer capabilities, queried via evtest on-device (device_report.md):
//!   ABS_X        0..20966   (long axis: spans the 1872px display dimension)
//!   ABS_Y        0..15725   (short axis: spans the 1404px display dimension)
//!   ABS_PRESSURE 0..4095
//!   ABS_DISTANCE 0..255
//!   ABS_TILT_X/Y -9000..9000
//! Exact axis orientation/flips display<->pen come from the empirical
//! calibration pass (`scribed calibrate`), never assumed.
//!
//! Speed model for the "rapid draw" effect: events are batched (many
//! input_event structs per write() syscall) and paced by a points-per-second
//! budget. Pacing exists because evdev client buffers overflow if we write
//! faster than xochitl drains — the kernel then drops events and injects
//! SYN_DROPPED, mangling strokes. The right pps ceiling is found empirically
//! on-device; the CLI exposes --pps.

use anyhow::{bail, Context, Result};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::mem::size_of;
use std::os::unix::io::AsRawFd;
use std::thread::sleep;
use std::time::{Duration, Instant};

use scribed_core::geometry::{AffineTransform, PointPx, Stroke};

// --- input-event-codes.h (stable kernel ABI) ---
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_ABS: u16 = 0x03;
const SYN_REPORT: u16 = 0;

const BTN_TOOL_PEN: u16 = 0x140;
const BTN_TOOL_RUBBER: u16 = 0x141;
const BTN_TOUCH: u16 = 0x14a;

const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const ABS_PRESSURE: u16 = 0x18;
const ABS_DISTANCE: u16 = 0x19;
const ABS_TILT_X: u16 = 0x1a;
const ABS_TILT_Y: u16 = 0x1b;

pub const PEN_ABS_X: (i32, i32) = (0, 20966);
pub const PEN_ABS_Y: (i32, i32) = (0, 15725);
pub const PEN_ABS_PRESSURE: (i32, i32) = (0, 4095);
pub const PEN_ABS_DISTANCE: (i32, i32) = (0, 255);
pub const PEN_ABS_TILT: (i32, i32) = (-9000, 9000);

#[repr(C)]
#[derive(Clone, Copy)]
struct InputEvent {
    sec: libc::c_ulong,  // kernel __kernel_ulong_t — see ABI note above
    usec: libc::c_ulong,
    type_: u16,
    code: u16,
    value: i32,
}

impl InputEvent {
    fn new(type_: u16, code: u16, value: i32) -> Self {
        Self { sec: 0, usec: 0, type_, code, value }
    }
}

#[repr(C)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct InputAbsInfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

const UINPUT_MAX_NAME_SIZE: usize = 80;

#[repr(C)]
struct UinputSetup {
    id: InputId,
    name: [u8; UINPUT_MAX_NAME_SIZE],
    ff_effects_max: u32,
}

#[repr(C)]
struct UinputAbsSetup {
    code: u16,
    absinfo: InputAbsInfo,
}

// --- asm-generic/ioctl.h request encoding (ARM uses the generic layout) ---
const IOC_NRSHIFT: u32 = 0;
const IOC_TYPESHIFT: u32 = 8;
const IOC_SIZESHIFT: u32 = 16;
const IOC_DIRSHIFT: u32 = 30;
const IOC_NONE: u32 = 0;
const IOC_WRITE: u32 = 1;
const UINPUT_IOCTL_BASE: u32 = b'U' as u32;

const fn ioc(dir: u32, ty: u32, nr: u32, size: u32) -> u32 {
    (dir << IOC_DIRSHIFT) | (ty << IOC_TYPESHIFT) | (nr << IOC_NRSHIFT) | (size << IOC_SIZESHIFT)
}
const fn io(nr: u32) -> u32 {
    ioc(IOC_NONE, UINPUT_IOCTL_BASE, nr, 0)
}
fn iow<T>(nr: u32) -> u32 {
    ioc(IOC_WRITE, UINPUT_IOCTL_BASE, nr, size_of::<T>() as u32)
}

const UI_DEV_CREATE: u32 = io(1);
const UI_DEV_DESTROY: u32 = io(2);

// `libc::ioctl` request type differs per target (c_int on 32-bit musl,
// c_ulong on 64-bit glibc) — take u32 and cast with `as _` at the call.
unsafe fn ioctl_int(fd: i32, req: u32, val: libc::c_int) -> Result<()> {
    if libc::ioctl(fd, req as _, val) < 0 {
        bail!("ioctl({req:#x}, {val}) failed: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

unsafe fn ioctl_ptr<T>(fd: i32, req: u32, val: *const T) -> Result<()> {
    if libc::ioctl(fd, req as _, val) < 0 {
        bail!("ioctl({req:#x}) failed: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Pen,
    Rubber,
}

impl Tool {
    fn code(self) -> u16 {
        match self {
            Tool::Pen => BTN_TOOL_PEN,
            Tool::Rubber => BTN_TOOL_RUBBER,
        }
    }
}

pub struct VirtualPen {
    file: File,
    /// Currently-held tool (pen stays "in proximity" across a whole drawing
    /// session — cheaper and more realistic than tool-in/out per stroke).
    active_tool: Option<Tool>,
    events_written: u64,
    /// True if we created a uinput device (must UI_DEV_DESTROY on drop);
    /// false when injecting into an existing node.
    owns_uinput: bool,
}

impl VirtualPen {
    /// Inject events directly into an EXISTING evdev device node (e.g. the
    /// real digitizer /dev/input/event1). The kernel's evdev write handler
    /// passes written input_events through input_inject_event(), so they are
    /// delivered to every reader of that device exactly as if the hardware
    /// had produced them. No uinput registration, no hotplug — works even if
    /// xochitl only listens to the real digitizer node.
    pub fn open_existing(path: &str) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("opening {path} for event injection (need root)"))?;
        Ok(Self { file, active_tool: None, events_written: 0, owns_uinput: false })
    }

    pub fn new() -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/uinput")
            .context("opening /dev/uinput (need root)")?;
        let fd = file.as_raw_fd();

        unsafe {
            ioctl_int(fd, iow::<libc::c_int>(100), EV_KEY as libc::c_int)?; // UI_SET_EVBIT
            for code in [BTN_TOOL_PEN, BTN_TOOL_RUBBER, BTN_TOUCH] {
                ioctl_int(fd, iow::<libc::c_int>(101), code as libc::c_int)?; // UI_SET_KEYBIT
            }
            ioctl_int(fd, iow::<libc::c_int>(100), EV_ABS as libc::c_int)?;
            for code in [ABS_X, ABS_Y, ABS_PRESSURE, ABS_DISTANCE, ABS_TILT_X, ABS_TILT_Y] {
                ioctl_int(fd, iow::<libc::c_int>(103), code as libc::c_int)?; // UI_SET_ABSBIT
            }

            let mut name = [0u8; UINPUT_MAX_NAME_SIZE];
            let n = b"scribed-virtual-pen";
            name[..n.len()].copy_from_slice(n);
            let setup = UinputSetup {
                id: InputId { bustype: 0x18 /* BUS_I2C, same as the real digitizer */, vendor: 0x2d1f, product: 0x0095, version: 1 },
                name,
                ff_effects_max: 0,
            };
            ioctl_ptr(fd, iow::<UinputSetup>(3), &setup as *const UinputSetup)?; // UI_DEV_SETUP

            let axes: [(u16, (i32, i32), i32); 6] = [
                (ABS_X, PEN_ABS_X, 100),
                (ABS_Y, PEN_ABS_Y, 100),
                (ABS_PRESSURE, PEN_ABS_PRESSURE, 0),
                (ABS_DISTANCE, PEN_ABS_DISTANCE, 0),
                (ABS_TILT_X, PEN_ABS_TILT, 0),
                (ABS_TILT_Y, PEN_ABS_TILT, 0),
            ];
            for (code, (min, max), resolution) in axes {
                let abs = UinputAbsSetup {
                    code,
                    absinfo: InputAbsInfo { value: 0, minimum: min, maximum: max, fuzz: 0, flat: 0, resolution },
                };
                ioctl_ptr(fd, iow::<UinputAbsSetup>(4), &abs as *const UinputAbsSetup)?; // UI_ABS_SETUP
            }

            if libc::ioctl(fd, UI_DEV_CREATE as _, 0) < 0 {
                bail!("UI_DEV_CREATE failed: {}", std::io::Error::last_os_error());
            }
        }

        // Let udev create the node and xochitl enumerate the new device.
        sleep(Duration::from_millis(500));
        Ok(Self { file, active_tool: None, events_written: 0, owns_uinput: true })
    }

    pub fn events_written(&self) -> u64 {
        self.events_written
    }

    fn write_events(&mut self, events: &[InputEvent]) -> Result<()> {
        let bytes = unsafe {
            std::slice::from_raw_parts(events.as_ptr() as *const u8, events.len() * size_of::<InputEvent>())
        };
        self.file.write_all(bytes).context("writing input_events to /dev/uinput")?;
        self.events_written += events.len() as u64;
        Ok(())
    }

    /// Bring the tool into proximity (start of a drawing/erasing session).
    pub fn tool_in(&mut self, tool: Tool) -> Result<()> {
        self.write_events(&[
            InputEvent::new(EV_KEY, tool.code(), 1),
            InputEvent::new(EV_ABS, ABS_DISTANCE, 20),
            InputEvent::new(EV_SYN, SYN_REPORT, 0),
        ])?;
        self.active_tool = Some(tool);
        Ok(())
    }

    /// Take the tool out of proximity (end of session).
    pub fn tool_out(&mut self) -> Result<()> {
        if let Some(tool) = self.active_tool.take() {
            // Defensive nib release first: if a stroke aborted mid-way (a write
            // failed after BTN_TOUCH-down), the nib is still "down" and xochitl
            // would draw a stray line into the next session. Lifting it here is
            // harmless when the nib was already up.
            let _ = self.write_events(&[
                InputEvent::new(EV_ABS, ABS_PRESSURE, 0),
                InputEvent::new(EV_ABS, ABS_DISTANCE, 40),
                InputEvent::new(EV_KEY, BTN_TOUCH, 0),
                InputEvent::new(EV_SYN, SYN_REPORT, 0),
            ]);
            self.write_events(&[
                InputEvent::new(EV_KEY, tool.code(), 0),
                InputEvent::new(EV_SYN, SYN_REPORT, 0),
            ])?;
        }
        Ok(())
    }

    /// Inject one stroke. `pps` = points per second pacing budget.
    /// Requires tool_in() first. Points are already in pen units here —
    /// callers apply the calibrated display->pen transform.
    pub fn stroke_pen_units(&mut self, points: &[(i32, i32, i32)], pps: f64) -> Result<()> {
        if points.is_empty() {
            return Ok(());
        }
        // Hover-move to the start point before touching down, so xochitl's
        // stroke begins where we mean it to rather than interpolating from
        // the previous stroke's end.
        let (x0, y0, _) = points[0];
        self.write_events(&[
            InputEvent::new(EV_ABS, ABS_X, x0),
            InputEvent::new(EV_ABS, ABS_Y, y0),
            InputEvent::new(EV_ABS, ABS_DISTANCE, 20),
            InputEvent::new(EV_SYN, SYN_REPORT, 0),
            InputEvent::new(EV_ABS, ABS_DISTANCE, 0),
            InputEvent::new(EV_KEY, BTN_TOUCH, 1),
            InputEvent::new(EV_SYN, SYN_REPORT, 0),
        ])?;

        // Batch points into chunks; pace by pps across chunk boundaries.
        const CHUNK_POINTS: usize = 32;
        let per_point = if pps > 0.0 { Duration::from_secs_f64(1.0 / pps) } else { Duration::ZERO };
        let started = Instant::now();
        let mut sent: usize = 0;
        let mut batch: Vec<InputEvent> = Vec::with_capacity(CHUNK_POINTS * 4);
        for chunk in points.chunks(CHUNK_POINTS) {
            batch.clear();
            for &(x, y, pressure) in chunk {
                batch.push(InputEvent::new(EV_ABS, ABS_X, x));
                batch.push(InputEvent::new(EV_ABS, ABS_Y, y));
                batch.push(InputEvent::new(EV_ABS, ABS_PRESSURE, pressure));
                batch.push(InputEvent::new(EV_SYN, SYN_REPORT, 0));
            }
            self.write_events(&batch)?;
            sent += chunk.len();
            // Sleep only as much as the pps budget says we're ahead.
            let target = per_point * sent as u32;
            let elapsed = started.elapsed();
            if target > elapsed {
                sleep(target - elapsed);
            }
        }

        // Pen-up, hardened against the "stray connecting line" artifact: if
        // a single BTN_TOUCH-up frame is dropped/coalesced, the pen stays
        // "down" and xochitl draws a straight line from here to the next
        // stroke's start. So we lift pressure to 0, raise the tool well out
        // of contact range (ABS_DISTANCE), send BTN_TOUCH 0 twice across
        // separate SYN frames, and settle briefly — redundant enough that no
        // single dropped event leaves the nib touching.
        self.write_events(&[
            InputEvent::new(EV_ABS, ABS_PRESSURE, 0),
            InputEvent::new(EV_KEY, BTN_TOUCH, 0),
            InputEvent::new(EV_SYN, SYN_REPORT, 0),
        ])?;
        self.write_events(&[
            InputEvent::new(EV_ABS, ABS_DISTANCE, 40),
            InputEvent::new(EV_KEY, BTN_TOUCH, 0),
            InputEvent::new(EV_SYN, SYN_REPORT, 0),
        ])?;
        Ok(())
    }

    /// Convenience: inject a display-space stroke through a calibrated
    /// display->pen transform with a normalized pressure.
    pub fn stroke_display(&mut self, stroke: &Stroke, transform: &AffineTransform, pps: f64) -> Result<()> {
        let pts: Vec<(i32, i32, i32)> = stroke
            .points
            .iter()
            .map(|p| {
                let pen = transform.apply(PointPx::new(p.x, p.y));
                let pressure = (p.pressure.clamp(0.0, 1.0) * PEN_ABS_PRESSURE.1 as f32) as i32;
                (
                    pen.x.clamp(PEN_ABS_X.0, PEN_ABS_X.1),
                    pen.y.clamp(PEN_ABS_Y.0, PEN_ABS_Y.1),
                    pressure,
                )
            })
            .collect();
        self.stroke_pen_units(&pts, pps)
    }
}

impl Drop for VirtualPen {
    fn drop(&mut self) {
        let _ = self.tool_out();
        if self.owns_uinput {
            unsafe {
                let _ = libc::ioctl(self.file.as_raw_fd(), UI_DEV_DESTROY as _, 0);
            }
        }
    }
}
