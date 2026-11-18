//! An iOS simulator's screen and input, for the daemon's device panel
//! (ADR-0020).
//!
//! Simulator.app shows a device's screen and sends it touches through two of
//! Xcode's private frameworks: CoreSimulator gives the device's display as an
//! IOSurface with a callback for every change, and SimulatorKit makes the HID
//! messages (touches, buttons, keys) the device takes. The daemon uses them
//! the same way, but in a helper process of its own (`agentloftd simulator`,
//! [`run`]), so that a crash in a private framework ends the helper and not
//! the daemon. The daemon (`loft-core`'s `devices/ios.rs`) starts one helper
//! per attached device and talks to it over its stdin and stdout
//! ([`wire`]).
//!
//! What the helper needs to know that isn't Objective-C ([`geometry`]: how
//! a turned screen's pictures map to the device's own; [`keys`]: which HID
//! key makes which character) builds everywhere, so it is tested everywhere.

pub mod geometry;
pub mod keys;
pub mod wire;

#[cfg(target_os = "macos")]
mod helper;

#[cfg(target_os = "macos")]
pub use helper::{Options, run};
