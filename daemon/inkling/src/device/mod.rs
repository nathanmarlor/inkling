//! Device I/O for the reMarkable 2. Linux-only (evdev pen/touch injection,
//! screen capture via the inklingfb extension) — everything here is cfg-gated
//! so the workspace still builds and tests on a dev machine; the CLI reports
//! "linux only" for device subcommands elsewhere.

#[cfg(target_os = "linux")]
pub mod capture;
#[cfg(target_os = "linux")]
pub mod touch;
#[cfg(target_os = "linux")]
pub mod uinput;
