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
            eprintln!(
                "[zmux] warning: could not restrict the DLL search path; \
                 a planted conpty.dll could be loaded"
            );
        }
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
