//! zmux - tmux-style panes for Windows with persistent, detachable sessions.
//!
//! Windows only by design (DESIGN.md §3): no `cfg(unix)` branches anywhere.

pub mod config;
pub mod cwd;
pub mod ipc;
pub mod layout;
pub mod protocol;
pub mod persist;
pub mod pty;
pub mod render;
pub mod session;
pub mod term;
