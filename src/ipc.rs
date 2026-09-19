//! Named-pipe transport. Adapted from herdr's `src/ipc.rs` (Apache-2.0),
//! reduced to Windows-only and given explicit framing.
//!
//! herdr consumes `interprocess::local_socket` as a BYTE STREAM with
//! arbitrary-size reads, so message boundaries do not survive. Framing is
//! therefore mandatory, not optional (DESIGN.md §3).

use std::io::{self, Read, Write};

use anyhow::{bail, Context, Result};
use interprocess::local_socket::{
    prelude::*, GenericNamespaced, ListenerOptions, Stream,
};
use serde::{de::DeserializeOwned, Serialize};

use crate::protocol::MAX_FRAME_BYTES;

/// Per-user pipe name, keyed on the user's SID rather than `%USERNAME%`.
///
/// `USERNAME` is an ordinary environment variable that any process can set, so
/// deriving the name from it lets one local user predict - or collide with -
/// another's pipe. The SID is a real security principal and cannot be spoofed
/// by editing the environment.
///
/// The session component is sanitised because it reaches an OS object name.
pub fn pipe_name(session: &str) -> String {
    let user = current_user_key();
    let safe: String = session
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .take(64)
        .collect();
    format!("zmux-{user}-{safe}")
}

/// The current user's SID, or a clearly-marked fallback.
fn current_user_key() -> String {
    #[cfg(windows)]
    {
        if let Some(sid) = windows_user_sid() {
            return sid;
        }
    }
    // Falling back to USERNAME is weaker but still separates ordinary users;
    // the restrictive DACL remains the real protection either way.
    format!("u-{}", std::env::var("USERNAME").unwrap_or_else(|_| "unknown".into()))
}

#[cfg(windows)]
fn windows_user_sid() -> Option<String> {
    use std::os::windows::ffi::OsStringExt;

    type Handle = *mut core::ffi::c_void;
    extern "system" {
        fn GetCurrentProcess() -> Handle;
        fn OpenProcessToken(p: Handle, access: u32, token: *mut Handle) -> i32;
        fn GetTokenInformation(t: Handle, class: i32, info: *mut u8, len: u32, ret: *mut u32) -> i32;
        fn ConvertSidToStringSidW(sid: *mut core::ffi::c_void, out: *mut *mut u16) -> i32;
        fn CloseHandle(h: Handle) -> i32;
        fn LocalFree(p: *mut u16) -> *mut u16;
    }
    const TOKEN_QUERY: u32 = 0x0008;
    const TOKEN_USER: i32 = 1;

    unsafe {
        let mut token: Handle = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return None;
        }
        let mut need = 0u32;
        GetTokenInformation(token, TOKEN_USER, std::ptr::null_mut(), 0, &mut need);
        if need == 0 {
            CloseHandle(token);
            return None;
        }
        // Back the buffer with pointer-aligned storage. A Vec<u8> guarantees
        // only byte alignment, so reading a PSID out of it is undefined
        // behaviour even though it happens to work on x86.
        let words = (need as usize).div_ceil(std::mem::size_of::<usize>()).max(1);
        let mut buf: Vec<usize> = vec![0; words];
        let ok = GetTokenInformation(
            token,
            TOKEN_USER,
            buf.as_mut_ptr() as *mut u8,
            need,
            &mut need,
        ) != 0;
        CloseHandle(token);
        if !ok {
            return None;
        }
        // TOKEN_USER begins with SID_AND_ATTRIBUTES { PSID Sid; ... }
        let sid = *(buf.as_ptr() as *const *mut core::ffi::c_void);
        let mut raw: *mut u16 = std::ptr::null_mut();
        if ConvertSidToStringSidW(sid, &mut raw) == 0 || raw.is_null() {
            return None;
        }
        let mut len = 0usize;
        while *raw.add(len) != 0 {
            len += 1;
        }
        let text = std::ffi::OsString::from_wide(std::slice::from_raw_parts(raw, len));
        LocalFree(raw);
        // SIDs contain '-' and digits, both legal in a pipe name.
        text.into_string().ok()
    }
}

fn bincode_config() -> bincode::config::Configuration {
    bincode::config::standard()
}

/// Length-prefixed write: u32 big-endian length, then the payload.
pub fn write_msg<W: Write, T: Serialize>(w: &mut W, msg: &T) -> Result<()> {
    // NOTE: this checks AFTER encoding, so a pathological message is still
    // allocated once. Callers bound their inputs upstream (protocol::MAX_*);
    // this is the backstop, not the primary defence.
    let bytes = bincode::serde::encode_to_vec(msg, bincode_config())
        .context("encode message")?;
    if bytes.len() as u64 > MAX_FRAME_BYTES as u64 {
        bail!("outgoing frame {} exceeds MAX_FRAME_BYTES", bytes.len());
    }
    w.write_all(&(bytes.len() as u32).to_be_bytes())?;
    w.write_all(&bytes)?;
    w.flush()?;
    Ok(())
}

/// Length-prefixed read. Returns `Ok(None)` on clean EOF.
///
/// The length is validated BEFORE allocating, so a corrupt or hostile peer
/// cannot make us reserve an arbitrary buffer.
pub fn read_msg<R: Read, T: DeserializeOwned>(r: &mut R) -> Result<Option<T>> {
    let mut len_buf = [0u8; 4];
    if !read_exact_or_eof(r, &mut len_buf)? { return Ok(None) }
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME_BYTES {
        bail!("incoming frame {len} exceeds MAX_FRAME_BYTES {MAX_FRAME_BYTES}");
    }
    let mut buf = vec![0u8; len as usize];
    if !read_exact_or_eof(r, &mut buf)? {
        bail!("truncated frame: wanted {len} bytes");
    }
    let (msg, _) = bincode::serde::decode_from_slice(&buf, bincode_config())
        .context("decode message")?;
    Ok(Some(msg))
}

/// Reads exactly `buf.len()` bytes. `false` means clean EOF before any byte.
fn read_exact_or_eof<R: Read>(r: &mut R, buf: &mut [u8]) -> Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => return Ok(filled != 0 && { bail!("eof mid-frame") }),
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(true)
}

pub fn connect(session: &str) -> Result<Stream> {
    let name = pipe_name(session);
    let ns = name.as_str().to_ns_name::<GenericNamespaced>()?;
    Ok(Stream::connect(ns)?)
}

/// True if a daemon is already listening for this session.
pub fn is_running(session: &str) -> bool {
    connect(session).is_ok()
}

/// List live sessions by enumerating the named-pipe namespace.
///
/// Windows exposes `\\.\pipe\` as a readable directory, so this needs no
/// registry of our own - a pipe exists exactly while its daemon does, which
/// makes stale entries structurally impossible.
pub fn list_sessions() -> Vec<String> {
    // Must use the same key as pipe_name, or listing silently finds nothing.
    let prefix = format!("zmux-{}-", current_user_key());
    let Ok(dir) = std::fs::read_dir(r"\\.\pipe\") else {
        return Vec::new();
    };
    let mut out: Vec<String> = dir
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter_map(|n| n.strip_prefix(&prefix).map(str::to_string))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Restrict the pipe to SYSTEM and the creating user.
///
/// `D:P(A;;GA;;;SY)(A;;GA;;;OW)` - a protected DACL (P, so it does not inherit
/// anything permissive) granting all access to SYSTEM and to the object's
/// OWNER, which is whoever created it. Same descriptor herdr uses
/// (`ipc.rs:149`). The pipe carries keystrokes, so the default is not good
/// enough on a shared or managed machine.
const PIPE_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;OW)";

pub fn listen(session: &str) -> Result<interprocess::local_socket::Listener> {
    use interprocess::os::windows::local_socket::ListenerOptionsExt as _;
    use interprocess::os::windows::security_descriptor::SecurityDescriptor;
    use widestring::U16CString;

    let name = pipe_name(session);
    let ns = name.as_str().to_ns_name::<GenericNamespaced>()?;

    let sddl = U16CString::from_str(PIPE_SDDL)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let sd = SecurityDescriptor::deserialize(&sddl).context("build pipe security descriptor")?;

    Ok(ListenerOptions::new().name(ns).security_descriptor(sd).create_sync()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_name_cannot_inject_into_the_pipe_name() {
        // The session string reaches an OS object name, so anything exotic
        // must be flattened rather than passed through.
        let n = pipe_name("../../evil\\path");
        assert!(!n.contains('/') && !n.contains('\\') && !n.contains(".."), "{n}");
    }

    #[test]
    fn different_sessions_get_different_pipes() {
        assert_ne!(pipe_name("a"), pipe_name("b"));
    }

    #[cfg(windows)]
    #[test]
    fn the_pipe_name_is_keyed_on_the_user_sid() {
        // USERNAME is writable by any process; the SID is not.
        let key = super::current_user_key();
        assert!(
            key.starts_with("S-1-"),
            "expected a SID-derived key, got {key:?} - the name would be spoofable"
        );
    }
}
