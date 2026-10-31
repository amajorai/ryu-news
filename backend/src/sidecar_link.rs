//! Let a second process reach the store through the running sidecar.
//!
//! The handoff file, its route gate and the loopback client are shared by
//! every Rust app satellite; see [`ryu_sidecar_runtime::handoff`].

pub use ryu_sidecar_runtime::handoff::*;
