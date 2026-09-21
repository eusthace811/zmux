//! Process hardening applied before anything else runs.

/// Remove the current directory, `PATH`, and the application directory from
/// the DLL search order, leaving only `System32`.
///
/// This is not theoretical. `portable-pty` 0.9.0 deliberately probes for a
/// side-loaded ConPTY by bare name:
///
/// ```text
/// portable-pty-0.9.0/src/win/psuedocon.rs:55
///     if let Ok(sideloaded) = ConPtyFuncs::open(Path::new("conpty.dll")) {
/// ```
///
/// `kernel32.dll` on the line above is safe because it is a Windows KnownDLL
/// and always resolves from `System32`. `conpty.dll` is NOT a KnownDLL, so a
/// file of that name planted next to our executable would be loaded and
/// executed inside both the client and the daemon. On a managed laptop that is
/// exactly the kind of thing that must not be possible.
///
/// herdr solves this by vendoring a patched `portable-pty` that hash-verifies
/// an app-local ConPTY (`DESIGN.md` §2.3). We take the smaller route: deny the
/// search path itself, which neutralises the vector without forking a
/// dependency. The cost is that a legitimate side-loaded ConPTY is also
/// ignored - which is what we want, since we do not ship one.
///
/// Must be called before any code that might trigger a `LoadLibrary`.
/// Requires Windows 8 or later; a failure is reported and is not fatal.
pub fn restrict_dll_search_path() {
    #[cfg(windows)]
    {
        // LOAD_LIBRARY_SEARCH_SYSTEM32
        const SYSTEM32_ONLY: u32 = 0x0000_0800;
        extern "system" {
            fn SetDefaultDllDirectories(flags: u32) -> i32;
        }
        // SAFETY: a documented Win32 call taking a flags word.
        if unsafe { SetDefaultDllDirectories(SYSTEM32_ONLY) } == 0 {
            // FAIL CLOSED. This control exists to prevent arbitrary code
            // execution; continuing without it would walk straight into the
            // hijack path it was added to close.
            eprintln!(
                "[zmux] fatal: could not restrict the DLL search path \
                 (error {}); refusing to start.",
                std::io::Error::last_os_error()
            );
            std::process::exit(1);
        }
    }
}

/// Whether the job object this process belongs to kills its members when the
/// last handle to it closes. `None` means the question could not be answered.
///
/// This is the shipped half of Probe C, and the reason it is worth shipping is
/// that Probe C has never been run on the machine zmux is FOR
/// (`probes/RUNBOOK.md`, `DESIGN.md` §8.3). Windows console hosts put their
/// children in a job object, and `DETACHED_PROCESS` severs the console without
/// leaving the job - so if that job carries
/// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, the daemon dies with the terminal
/// that started it and every "persistent" session goes with it. Group policy
/// is the usual source, so this is most likely to be true exactly where it has
/// not been tested.
///
/// herdr asks the same question at runtime
/// (`platform/windows.rs:current_job_kills_processes_on_close`) and, when the
/// answer is yes, spawns via WMI `Win32_Process.Create` instead. That rung is
/// closed to us: WMI process creation is MITRE ATT&CK T1047 and a high-signal
/// EDR detection on a managed laptop (`DESIGN.md` §0.2). So we report rather
/// than route around it - a session that will not survive is still worth
/// having, as long as nobody is told otherwise.
///
/// ONE-SIDED. `QueryInformationJobObject(NULL, ...)` describes the job this
/// process is DIRECTLY in. Jobs nest, an ancestor's limits apply too, and they
/// cannot be read from inside. `Some(true)` is therefore trustworthy;
/// `Some(false)` only means "not the immediate job".
pub fn job_kills_members_on_close() -> Option<bool> {
    #[cfg(not(windows))]
    {
        None
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::JobObjects::{
            IsProcessInJob, QueryInformationJobObject, JobObjectExtendedLimitInformation,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;

        let mut in_job = 0i32;
        // SAFETY: a pseudo-handle for this process, a null job meaning "any",
        // and a valid writable BOOL.
        if unsafe { IsProcessInJob(GetCurrentProcess(), std::ptr::null_mut(), &mut in_job) } == 0 {
            return None;
        }
        if in_job == 0 {
            return Some(false);
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is writable and its exact size is passed. A null job
        // handle means the job this process is in.
        let ok = unsafe {
            QueryInformationJobObject(
                std::ptr::null_mut(),
                JobObjectExtendedLimitInformation,
                std::ptr::from_mut(&mut info).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                std::ptr::null_mut(),
            )
        } != 0;
        if !ok {
            return None;
        }
        Some(info.BasicLimitInformation.LimitFlags & JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE != 0)
    }
}

#[cfg(all(test, windows))]
mod tests {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    extern "system" {
        fn LoadLibraryW(name: *const u16) -> *mut core::ffi::c_void;
        fn FreeLibrary(h: *mut core::ffi::c_void) -> i32;
    }

    fn try_load(name: &str) -> bool {
        let wide: Vec<u16> = OsStr::new(name).encode_wide().chain(Some(0)).collect();
        // SAFETY: a null-terminated UTF-16 name.
        let h = unsafe { LoadLibraryW(wide.as_ptr()) };
        if h.is_null() {
            false
        } else {
            unsafe { FreeLibrary(h) };
            true
        }
    }

    /// The two syscalls must actually work, with the struct size and info
    /// class we pass. A `None` here means we would ship a check that can never
    /// warn - worse than no check, because its silence reads as an all-clear.
    #[test]
    fn the_job_question_gets_an_answer_rather_than_a_shrug() {
        assert!(
            super::job_kills_members_on_close().is_some(),
            "could not read this process's job limits; the Probe C warning would never fire"
        );
    }

    /// Proves the mitigation rather than assuming it.
    ///
    /// A real, loadable DLL is placed in the current directory under a name
    /// Windows has never heard of. Before hardening it loads by bare name;
    /// after hardening it must not, because only System32 is searched. That is
    /// precisely the difference between `portable-pty` finding a planted
    /// `conpty.dll` and not finding one.
    ///
    /// Two separate copies are used because a loaded module stays resident and
    /// the second call would otherwise just return the cached handle.
    #[test]
    fn hardening_removes_the_local_directory_from_the_dll_search_path() {
        let sys = std::path::PathBuf::from(std::env::var("SystemRoot").unwrap_or("C:\\Windows".into()))
            .join("System32")
            .join("version.dll"); // small, dependency-light, always present
        if !sys.is_file() {
            eprintln!("skipping: no System32\\version.dll to copy");
            return;
        }
        let dir = std::env::temp_dir().join(format!("zmux-dll-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let before = dir.join("zmux_probe_before.dll");
        let after = dir.join("zmux_probe_after.dll");
        std::fs::copy(&sys, &before).expect("copy");
        std::fs::copy(&sys, &after).expect("copy");

        let prev = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(&dir).expect("chdir");

        let loaded_before = try_load("zmux_probe_before.dll");
        super::restrict_dll_search_path();
        let loaded_after = try_load("zmux_probe_after.dll");

        std::env::set_current_dir(prev).ok();
        std::fs::remove_dir_all(&dir).ok();

        assert!(
            loaded_before,
            "the probe DLL was not loadable even before hardening; the test proves nothing"
        );
        assert!(
            !loaded_after,
            "a DLL in the working directory was STILL loadable by bare name - \
             a planted conpty.dll would be executed in this process"
        );
    }
}
