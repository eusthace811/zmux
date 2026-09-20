//! Tracking a pane's working directory.
//!
//! There is no `/proc` on Windows and the process field you can inspect does
//! not follow a logical `cd`; herdr marks live cwd as *partial* on Windows for
//! exactly this reason (DESIGN.md §4). The working path is shell integration:
//! the shell emits OSC 7 on each prompt and we read it out of the vt100
//! callback stream.

/// Bash snippet injected via `--rcfile`.
///
/// Windows-specific details, none of them optional:
///
/// 1. A LOGIN shell ignores `--rcfile`, so we cannot use `-l`. Verified
///    directly: with `-l` the hook is undefined, without it the hook loads.
///    The file therefore performs login startup itself.
/// 2. `$PWD` is an MSYS path (`/c/work`) but `portable-pty` needs a Windows
///    one. We emit the MSYS form and convert on receipt rather than calling
///    `cygpath`, which would be a process spawn on every prompt.
/// 3. Spaces and non-ASCII must be percent-encoded to form a valid file URI.
/// 4. `PROMPT_COMMAND` is an array in bash 5.1+, so appending a string to it
///    is wrong; the type is tested first.
pub const RCFILE: &str = r#"
# zmux shell integration.
#
# Launched as `bash --rcfile <this>` WITHOUT -l, because a login shell ignores
# --rcfile entirely - verified: `bash --rcfile X -l -i` leaves the hook
# undefined, `bash --rcfile X -i` defines it. herdr uses `-l`, but herdr does
# not do OSC 7 for bash at all (its docs mark Windows cwd as partial), so there
# is no behaviour to match here.
#
# Because we dropped -l, this file must do the login shell's job itself or the
# user silently loses their environment.

if [ -z "$__ZMUX_INIT" ]; then
  __ZMUX_INIT=1

  # Login-equivalent startup.
  [ -f /etc/profile ] && . /etc/profile
  if [ -f "$HOME/.bash_profile" ]; then . "$HOME/.bash_profile"
  elif [ -f "$HOME/.bash_login" ]; then . "$HOME/.bash_login"
  elif [ -f "$HOME/.profile" ]; then . "$HOME/.profile"
  fi

  # Interactive startup, but ONLY if a profile above did not already pull it
  # in - double-sourcing re-runs profile side effects (PATH appends, ssh-agent
  # launches) a second time.
  if ! declare -F __zmux_bashrc_loaded >/dev/null 2>&1; then
    [ -f /etc/bash.bashrc ] && . /etc/bash.bashrc
    [ -f "$HOME/.bashrc" ] && . "$HOME/.bashrc"
  fi
fi

# Percent-encode per UTF-8 BYTE. Encoding code points instead means a
# non-ASCII directory never round-trips.
__zmux_urlencode() {
  local s="$1" o="" i c
  local LC_ALL=C
  for (( i=0; i<${#s}; i++ )); do
    c="${s:$i:1}"
    case "$c" in
      [-_.~a-zA-Z0-9/]) o+="$c" ;;
      *) o+=$(printf '%s' "$c" | od -An -tx1 | tr -d ' \n' | sed 's/../%&/g') ;;
    esac
  done
  printf '%s' "$o"
}

__zmux_osc7() {
  printf '\033]7;file://%s%s\033\\' "${HOSTNAME:-localhost}" "$(__zmux_urlencode "$PWD")"
}

# What is running, so the status bar can say so.
#
# OSC 133 is the FinalTerm/iTerm2 semantic-prompt convention (VS Code and
# WezTerm speak it too): `;C` means a command started, `;D;<status>` means it
# finished. The command TEXT is not part of OSC 133, so it rides in VS Code's
# `OSC 633;E;<line>` rather than in a sequence number we invented.
#
# Deliberately NOT a `trap ... DEBUG`. A DEBUG trap fires for every simple
# command, including the OTHER entries in PROMPT_COMMAND - and Git Bash
# installs one in git-prompt.sh - so the bar would sit showing somebody's
# prompt helper as the running command. PS0 is expanded exactly once per
# command line, after it is read and before it runs, and never for a
# PROMPT_COMMAND entry. Measured: the trap version also reported only `echo a`
# for `echo a; echo b`, where this reports the whole line.
__zmux_report() {
  printf '\033]133;C\033\\'
  local c
  c=$(HISTTIMEFORMAT= history 1)
  c=${c#"${c%%[![:space:]]*}"}   # drop leading spaces
  c=${c#* }                      # drop the history number
  c=${c#"${c%%[![:space:]]*}"}   # and the spaces after it
  # No history (HISTSIZE=0, or the line was suppressed by HISTCONTROL) just
  # means no text; the pane is still correctly reported as busy.
  [ -n "$c" ] && printf '\033]633;E;%s\033\\' "$c"
}

__zmux_precmd() {
  # FIRST line: this is the finished command's exit status.
  local e=$?
  # Unconditional, not only after a `;C`. Being at a prompt IS being idle, so
  # this re-asserts it even if a `;C` was lost, and an empty line costs one
  # sequence.
  printf '\033]133;D;%s\033\\' "$e"
  __zmux_osc7
}

# PROMPT_COMMAND is an array in bash 5.1+; appending a string would break it.
# PREPENDED in both forms, not appended: `$?` inside __zmux_precmd has to be
# the user's command's status, and anything running before us replaces it.
if [[ "$(declare -p PROMPT_COMMAND 2>/dev/null)" == "declare -a"* ]]; then
  PROMPT_COMMAND=(__zmux_precmd "${PROMPT_COMMAND[@]}")
else
  PROMPT_COMMAND="__zmux_precmd${PROMPT_COMMAND:+;$PROMPT_COMMAND}"
fi

# Keep whatever PS0 the user had; ours only prints escape sequences.
PS0='$(__zmux_report)'"${PS0:-}"

# Report immediately so a pane knows its directory before the first command.
__zmux_osc7
"#;

/// Decode an OSC 7 payload into a path.
///
/// Accepts `file://host/path` and a bare path. Returns the MSYS-style path as
/// the shell reported it; conversion to a Windows path is the caller's job.
pub fn parse_osc7(payload: &str) -> Option<String> {
    let rest = match payload.strip_prefix("file://") {
        Some(r) => {
            let i = r.find('/')?;
            &r[i..]
        },
        None if payload.starts_with('/') => payload,
        None => return None,
    };
    Some(percent_decode(rest))
}

fn percent_decode(s: &str) -> String {
    // Operate on BYTES throughout. The previous version sliced the &str by
    // byte offsets, so a payload like "%Aé" cut a multi-byte character in half
    // and panicked - inside the PTY reader thread, where any pane could
    // trigger it just by printing an escape sequence.
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    let hex = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Convert an MSYS path (`/c/work`, `/cygdrive/c/work`) to a Windows one.
///
/// Done here rather than by calling `cygpath` because that would be a process
/// spawn on every shell prompt.
pub fn msys_to_windows(p: &str) -> String {
    let rest = p.strip_prefix("/cygdrive").unwrap_or(p);
    let bytes = rest.as_bytes();
    // "/c/..." -> "C:\..."
    if bytes.len() >= 2 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() {
        let after = &rest[2..];
        if after.is_empty() || after.starts_with('/') {
            let drive = rest[1..2].to_ascii_uppercase();
            let tail = after.trim_start_matches('/').replace('/', "\\");
            return format!("{drive}:\\{tail}");
        }
    }
    rest.replace('/', "\\")
}

/// True only for a plain local drive path like `C:\work`.
///
/// ANY program in a pane can emit OSC 7, including a hostile one. A value such
/// as `file://h//server/share` becomes a UNC path, and merely calling
/// `is_dir()` on it makes Windows reach out over SMB and negotiate
/// credentials. Automatic cwd reuse is therefore restricted to local drives.
pub fn is_safe_local_dir(p: &str) -> bool {
    let b = p.as_bytes();
    if b.len() < 3 || !b[0].is_ascii_alphabetic() || b[1] != b':' {
        return false;
    }
    if b[2] != b'\\' && b[2] != b'/' {
        return false;
    }
    // Reject UNC-ish and relative escapes outright rather than reasoning about
    // how Windows will normalise them.
    !p.contains("\\\\") && !p.contains("..")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_file_uri() {
        assert_eq!(parse_osc7("file://host/c/work").as_deref(), Some("/c/work"));
    }

    #[test]
    fn parses_a_bare_path() {
        assert_eq!(parse_osc7("/c/work").as_deref(), Some("/c/work"));
    }

    #[test]
    fn decodes_percent_escapes() {
        assert_eq!(
            parse_osc7("file://h/c/my%20projects").as_deref(),
            Some("/c/my projects")
        );
    }

    #[test]
    fn decodes_non_ascii() {
        // "café" percent-encoded as UTF-8.
        assert_eq!(parse_osc7("file://h/c/caf%C3%A9").as_deref(), Some("/c/café"));
    }

    /// A pane can emit any OSC 7 it likes, and this ran on the PTY reader
    /// thread - a panic here took a pane's output with it.
    #[test]
    fn malformed_percent_escapes_do_not_panic() {
        for payload in ["file://h/c/%Aé", "file://h/c/%", "file://h/c/%ZZ", "file://h/c/%A"] {
            let _ = parse_osc7(payload);
        }
        assert_eq!(parse_osc7("file://h/c/a%").as_deref(), Some("/c/a%"));
    }

    #[test]
    fn rejects_nonsense() {
        assert!(parse_osc7("not-a-path").is_none());
        assert!(parse_osc7("file://host").is_none());
    }

    #[test]
    fn converts_msys_drive_paths() {
        assert_eq!(msys_to_windows("/c/work/x"), r"C:\work\x");
        assert_eq!(msys_to_windows("/c"), r"C:\");
        assert_eq!(msys_to_windows("/cygdrive/d/tmp"), r"D:\tmp");
    }

    #[test]
    fn only_local_drive_paths_are_reused() {
        assert!(is_safe_local_dir(r"C:\work"));
        assert!(is_safe_local_dir(r"D:\"));
        // A pane can emit any OSC 7 it likes; touching a UNC path would make
        // Windows negotiate SMB credentials with an attacker-chosen host.
        assert!(!is_safe_local_dir(r"\\server\share"));
        assert!(!is_safe_local_dir(r"C:\a\\\\server\share"));
        assert!(!is_safe_local_dir(r"C:\work\..\..\Windows"));
        assert!(!is_safe_local_dir("/usr/bin"));
        assert!(!is_safe_local_dir(""));
    }

    #[test]
    fn a_unc_osc7_payload_does_not_become_a_reusable_path() {
        let p = parse_osc7("file://h//server/share").expect("parses");
        assert!(!is_safe_local_dir(&msys_to_windows(&p)), "UNC path was accepted");
    }

    #[test]
    fn leaves_non_drive_paths_alone_apart_from_separators() {
        assert_eq!(msys_to_windows("/usr/bin"), r"\usr\bin");
    }
}
