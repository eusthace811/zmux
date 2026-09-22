//! The client's console input, read as the bytes the host terminal sent.
//!
//! crossterm reads a Windows console as key RECORDS, and a record cannot say
//! "this was pasted" - so a paste arrived as one keystroke per character and
//! no program in a pane ever saw a paste. The fix is herdr's
//! (`client/terminal_setup.rs`, `client/input/windows_vti.rs`): set
//! `ENABLE_VIRTUAL_TERMINAL_INPUT`, ask the host for bracketed paste, and read
//! the characters those records carry as the byte stream a terminal would
//! have sent. DESIGN.md §0.1 measured that mintty gives a native `.exe` a real
//! console where the bit can be set; Windows Terminal is the same ConPTY.
//!
//! crossterm still does everything else - raw mode, the alternate screen,
//! drawing. Only reading input moved here, and `input.rs` decodes it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;

use anyhow::{bail, Result};
use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Console::{
    GetConsoleMode, GetStdHandle, ReadConsoleInputW, SetConsoleMode, ENABLE_EXTENDED_FLAGS,
    ENABLE_MOUSE_INPUT, ENABLE_QUICK_EDIT_MODE, ENABLE_VIRTUAL_TERMINAL_INPUT,
    ENABLE_WINDOW_INPUT, INPUT_RECORD, KEY_EVENT, LEFT_ALT_PRESSED, LEFT_CTRL_PRESSED,
    MOUSE_EVENT, MOUSE_HWHEELED, MOUSE_MOVED, MOUSE_WHEELED, RIGHT_ALT_PRESSED,
    RIGHT_CTRL_PRESSED, SHIFT_PRESSED, STD_INPUT_HANDLE, WINDOW_BUFFER_SIZE_EVENT,
};
use windows_sys::Win32::System::Threading::WaitForSingleObject;

use crate::protocol::{MouseIn, MouseKind, MOUSE_ALT, MOUSE_CTRL, MOUSE_SHIFT};

/// One thing the console produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostInput {
    /// Characters, UTF-8, in the order typed. Escape sequences included.
    Bytes(Vec<u8>),
    /// A mouse event conhost delivered as a record rather than as an SGR
    /// report. Which of the two arrives depends on the console host, so both
    /// are handled - herdr handles both for the same reason.
    Mouse(MouseIn),
    /// The window changed size. Ask `terminal::size()` for the new one.
    Resize,
}

fn stdin_handle() -> Result<HANDLE> {
    let h = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    if h.is_null() || h == INVALID_HANDLE_VALUE {
        bail!("no console input handle");
    }
    Ok(h)
}

fn mode_of(h: HANDLE) -> Result<u32> {
    let mut mode = 0;
    if unsafe { GetConsoleMode(h, &mut mode) } == 0 {
        bail!("stdin is not a console: {}", std::io::Error::last_os_error());
    }
    Ok(mode)
}

fn set_mode(h: HANDLE, mode: u32) -> Result<()> {
    if unsafe { SetConsoleMode(h, mode) } == 0 {
        bail!("SetConsoleMode({mode:#x}): {}", std::io::Error::last_os_error());
    }
    Ok(())
}

/// VT input mode, for as long as this lives. Restores the previous mode on
/// drop, so it must be created AFTER crossterm's raw mode and dropped before
/// it is disabled - raw mode is part of what gets restored.
pub struct VtInput {
    saved: u32,
}

impl VtInput {
    pub fn enable() -> Result<Self> {
        let h = stdin_handle()?;
        let saved = mode_of(h)?;
        // WINDOW_INPUT: in VT mode a resize is still a record, and without it
        // there is no record at all.
        set_mode(h, saved | ENABLE_VIRTUAL_TERMINAL_INPUT | ENABLE_WINDOW_INPUT)?;
        // Read it back. herdr found the bit can be accepted and not stick, and
        // then the host's paste markers never arrive - the very bug this is for.
        if mode_of(h)? & ENABLE_VIRTUAL_TERMINAL_INPUT == 0 {
            let _ = set_mode(h, saved);
            bail!("this console does not support VT input (ENABLE_VIRTUAL_TERMINAL_INPUT)");
        }
        Ok(Self { saved })
    }
}

impl Drop for VtInput {
    fn drop(&mut self) {
        if let Ok(h) = stdin_handle() {
            let _ = set_mode(h, self.saved);
        }
    }
}

/// Take conhost's mouse, or give it back.
///
/// The host terminal is told separately, with VT (`?1000h` and friends) -
/// that is what mintty and Windows Terminal listen to. This is the console's
/// half: records for the mouse, and QuickEdit off while zmux has it, since
/// with QuickEdit on conhost keeps the mouse for selecting text.
pub fn set_mouse_input(on: bool) -> Result<()> {
    let h = stdin_handle()?;
    let mode = mode_of(h)? | ENABLE_EXTENDED_FLAGS;
    let mode = if on {
        (mode | ENABLE_MOUSE_INPUT) & !ENABLE_QUICK_EDIT_MODE
    } else {
        (mode & !ENABLE_MOUSE_INPUT) | ENABLE_QUICK_EDIT_MODE
    };
    set_mode(h, mode)
}

/// Console input on its own thread, as a channel.
///
/// A thread because `ReadConsoleInputW` blocks and the client's loop also has
/// frames to draw. It waits with a timeout so it can notice being dropped.
pub struct Reader {
    pub rx: Receiver<HostInput>,
    stop: Arc<AtomicBool>,
}

impl Reader {
    pub fn spawn() -> Result<Self> {
        stdin_handle()?;
        let (tx, rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let quit = Arc::clone(&stop);
        std::thread::spawn(move || read_loop(&tx, &quit));
        Ok(Self { rx, stop })
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn read_loop(tx: &Sender<HostInput>, stop: &AtomicBool) {
    let Ok(h) = stdin_handle() else { return };
    let mut text = Utf16::default();
    let mut mouse = MouseRecords::default();
    let mut records = [INPUT_RECORD::default(); 128];
    while !stop.load(Ordering::Relaxed) {
        match unsafe { WaitForSingleObject(h, 50) } {
            WAIT_OBJECT_0 => {}
            WAIT_TIMEOUT => continue,
            _ => return,
        }
        let mut read = 0;
        let ok = unsafe {
            ReadConsoleInputW(h, records.as_mut_ptr(), records.len() as u32, &mut read)
        };
        if ok == 0 {
            return;
        }
        let mut out = Vec::new();
        for r in &records[..read as usize] {
            match r.EventType as u32 {
                KEY_EVENT => {
                    let k = unsafe { r.Event.KeyEvent };
                    // In VT mode the characters ride on key-DOWN records; a
                    // key-up carries the same character again.
                    if k.bKeyDown == 0 {
                        continue;
                    }
                    let unit = unsafe { k.uChar.UnicodeChar };
                    // Ctrl-Space is NUL, and NUL is also "no character".
                    let unit = match unit {
                        0 if k.wVirtualKeyCode == 0x20 => 0,
                        0 => continue,
                        u => u,
                    };
                    for _ in 0..k.wRepeatCount.max(1) {
                        text.push(unit, &mut out);
                    }
                }
                MOUSE_EVENT => {
                    let m = unsafe { r.Event.MouseEvent };
                    let rec = MouseRecord {
                        x: m.dwMousePosition.X.max(0) as u16,
                        y: m.dwMousePosition.Y.max(0) as u16,
                        buttons: m.dwButtonState,
                        flags: m.dwEventFlags,
                        keys: m.dwControlKeyState,
                    };
                    if let Some(ev) = mouse.translate(rec) {
                        // Keep the order: text typed before the click goes first.
                        if !out.is_empty() && tx.send(HostInput::Bytes(std::mem::take(&mut out))).is_err() {
                            return;
                        }
                        if tx.send(HostInput::Mouse(ev)).is_err() {
                            return;
                        }
                    }
                }
                WINDOW_BUFFER_SIZE_EVENT => {
                    if !out.is_empty() && tx.send(HostInput::Bytes(std::mem::take(&mut out))).is_err() {
                        return;
                    }
                    if tx.send(HostInput::Resize).is_err() {
                        return;
                    }
                }
                _ => {}
            }
        }
        if !out.is_empty() && tx.send(HostInput::Bytes(out)).is_err() {
            return;
        }
    }
}

/// UTF-16 code units to UTF-8, across records: an emoji is two records, one
/// surrogate each, and a read can end between them.
#[derive(Debug, Default)]
struct Utf16 {
    high: Option<u16>,
}

impl Utf16 {
    fn push(&mut self, unit: u16, out: &mut Vec<u8>) {
        let c = match (self.high.take(), unit) {
            (None, 0xd800..=0xdbff) => {
                self.high = Some(unit);
                return;
            }
            (Some(hi), 0xdc00..=0xdfff) => {
                char::from_u32(0x10000 + ((u32::from(hi) - 0xd800) << 10) + (u32::from(unit) - 0xdc00))
            }
            // An unpaired surrogate is not a character. Drop it, keep going.
            (Some(_), 0xd800..=0xdbff) => {
                self.high = Some(unit);
                return;
            }
            (_, u) => char::from_u32(u32::from(u)),
        };
        if let Some(c) = c {
            let mut b = [0; 4];
            out.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct MouseRecord {
    x: u16,
    y: u16,
    buttons: u32,
    flags: u32,
    keys: u32,
}

/// Mouse records to press/release/drag events.
///
/// A record carries the buttons held NOW, not which one changed, so the
/// previous state is kept to tell a press from a release.
#[derive(Debug, Default)]
struct MouseRecords {
    held: u32,
}

impl MouseRecords {
    fn translate(&mut self, r: MouseRecord) -> Option<MouseIn> {
        // FROM_LEFT_1ST, RIGHTMOST, FROM_LEFT_2ND -> xterm's 0, 2, 1.
        let xterm = |bit: u32| match bit {
            1 => 0,
            2 => 2,
            _ => 1,
        };
        let lowest = |bits: u32| xterm(bits & bits.wrapping_neg());
        let mut mods = 0;
        if r.keys & SHIFT_PRESSED != 0 {
            mods |= MOUSE_SHIFT;
        }
        if r.keys & (LEFT_ALT_PRESSED | RIGHT_ALT_PRESSED) != 0 {
            mods |= MOUSE_ALT;
        }
        if r.keys & (LEFT_CTRL_PRESSED | RIGHT_CTRL_PRESSED) != 0 {
            mods |= MOUSE_CTRL;
        }
        let now = r.buttons & 0b111;
        let kind = if r.flags & MOUSE_WHEELED != 0 {
            // The high word is the signed distance; positive is away from you.
            if (r.buttons as i32) >> 16 > 0 { MouseKind::WheelUp } else { MouseKind::WheelDown }
        } else if r.flags & MOUSE_HWHEELED != 0 {
            return None;
        } else if r.flags & MOUSE_MOVED != 0 {
            if now != 0 { MouseKind::Drag(lowest(now)) } else { MouseKind::Move }
        } else {
            let pressed = now & !self.held;
            let released = self.held & !now;
            self.held = now;
            if pressed != 0 {
                MouseKind::Press(lowest(pressed))
            } else if released != 0 {
                MouseKind::Release(lowest(released))
            } else {
                return None;
            }
        };
        Some(MouseIn { x: r.x, y: r.y, kind, mods })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf8(units: &[u16]) -> Vec<u8> {
        let mut t = Utf16::default();
        let mut out = Vec::new();
        for &u in units {
            t.push(u, &mut out);
        }
        out
    }

    #[test]
    fn surrogate_pairs_become_one_character() {
        let units: Vec<u16> = "a😀é".encode_utf16().collect();
        assert_eq!(utf8(&units), "a😀é".as_bytes());
    }

    #[test]
    fn a_lone_surrogate_is_dropped_not_fatal() {
        assert_eq!(utf8(&[0xd83d, b'x' as u16]), b"x");
        assert_eq!(utf8(&[0xde00, b'y' as u16]), b"y");
    }

    fn rec(buttons: u32, flags: u32) -> MouseRecord {
        MouseRecord { x: 3, y: 4, buttons, flags, keys: 0 }
    }

    #[test]
    fn a_click_is_a_press_then_a_release() {
        let mut m = MouseRecords::default();
        assert_eq!(m.translate(rec(1, 0)).unwrap().kind, MouseKind::Press(0));
        assert_eq!(m.translate(rec(1, MOUSE_MOVED)).unwrap().kind, MouseKind::Drag(0));
        assert_eq!(m.translate(rec(0, 0)).unwrap().kind, MouseKind::Release(0));
        assert_eq!(m.translate(rec(0, MOUSE_MOVED)).unwrap().kind, MouseKind::Move);
        assert_eq!(m.translate(rec(2, 0)).unwrap().kind, MouseKind::Press(2), "right is 2");
        assert_eq!(m.translate(rec(6, 0)).unwrap().kind, MouseKind::Press(1), "middle is 1");
    }

    #[test]
    fn the_wheel_direction_is_the_sign_of_the_high_word() {
        let mut m = MouseRecords::default();
        assert_eq!(m.translate(rec(120 << 16, MOUSE_WHEELED)).unwrap().kind, MouseKind::WheelUp);
        let down = ((-120i32) << 16) as u32;
        assert_eq!(m.translate(rec(down, MOUSE_WHEELED)).unwrap().kind, MouseKind::WheelDown);
        assert!(m.translate(rec(120 << 16, MOUSE_HWHEELED)).is_none());
    }
}
