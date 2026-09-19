//! Probe D - does a native Windows exe get a REAL CONSOLE in this host?
//! Zero deps. Writes its own report file so that stdio stays attached to the
//! terminal: shell redirection would turn stdout into a pipe and invalidate
//! the whole measurement.
type Handle = *mut core::ffi::c_void;
extern "system" {
    fn GetConsoleMode(h: Handle, mode: *mut u32) -> i32;
    fn GetConsoleWindow() -> Handle;
    fn GetStdHandle(n: u32) -> Handle;
    fn GetConsoleScreenBufferInfo(h: Handle, i: *mut ScreenInfo) -> i32;
    fn GetLastError() -> u32;
}
#[repr(C)]
#[derive(Default)]
struct ScreenInfo { size: [i16;2], pos: [i16;2], attr: u16, win: [i16;4], max: [i16;2] }
const STD_INPUT: u32 = -10i32 as u32;
const STD_OUTPUT: u32 = -11i32 as u32;
const VT_INPUT: u32 = 0x0200;
const VT_OUTPUT: u32 = 0x0004;

fn main() {
    let mut r = String::new();
    macro_rules! out { ($($a:tt)*) => {{ let s=format!($($a)*); println!("{s}"); r.push_str(&s); r.push('\n'); }} }

    out!("MSYS={:?} TERM={:?}", std::env::var("MSYS").unwrap_or_default(), std::env::var("TERM").unwrap_or_default());
    out!("GetConsoleWindow      : {}", if unsafe{GetConsoleWindow()}.is_null() {"NULL"} else {"non-null"});

    let mut im=0u32; let ok_in = unsafe{ GetConsoleMode(GetStdHandle(STD_INPUT), &mut im) }!=0;
    if ok_in { out!("GetConsoleMode(stdin) : OK 0x{im:04x} VT_INPUT={}", im & VT_INPUT!=0); }
    else     { out!("GetConsoleMode(stdin) : FAIL err={}", unsafe{GetLastError()}); }

    let mut om=0u32; let ok_out = unsafe{ GetConsoleMode(GetStdHandle(STD_OUTPUT), &mut om) }!=0;
    if ok_out { out!("GetConsoleMode(stdout): OK 0x{om:04x} VT_OUTPUT={}", om & VT_OUTPUT!=0); }
    else      { out!("GetConsoleMode(stdout): FAIL err={}", unsafe{GetLastError()}); }

    let mut si=ScreenInfo::default();
    if unsafe{ GetConsoleScreenBufferInfo(GetStdHandle(STD_OUTPUT), &mut si) }!=0 {
        out!("console size          : {}x{}", si.win[2]-si.win[0]+1, si.win[3]-si.win[1]+1);
    } else { out!("console size          : unavailable"); }

    out!("");
    if ok_in && ok_out { out!(">> REAL CONSOLE - crossterm raw mode WILL work. Host is usable."); }
    else { out!(">> NO CONSOLE - crossterm enable_raw_mode() will FAIL here."); }

    if let Some(path) = std::env::args().nth(1) { let _ = std::fs::write(path, r); }
}
