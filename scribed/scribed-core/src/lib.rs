//! Platform-independent core of the Magic Notebook Illustrator daemon.
//! No evdev/uinput/proc-fs access lives here — see the `scribed` binary
//! crate's `device` module for that. Kept separate so this crate builds
//! and its tests run on any machine (DESIGN.md M2 is explicitly "no device
//! needed", and the watcher/dissolve/config logic benefits the same way).

pub mod config;
pub mod dissolve;
pub mod geometry;
pub mod surface;
pub mod toggle;
pub mod vector;
pub mod watch;
