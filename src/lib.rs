//! zmux - tmux-style panes for Windows with persistent, detachable sessions.
//!
//! Windows only by design (DESIGN.md §3): no `cfg(unix)` branches anywhere.

pub mod ipc;
pub mod protocol;
pub mod pty;
pub mod render;
pub mod term;
