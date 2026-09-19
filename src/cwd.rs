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

  # Interactive startup, unless a profile above already pulled it in.
  [ -f /etc/bash.bashrc ] && . /etc/bash.bashrc
  [ -f "$HOME/.bashrc" ] && . "$HOME/.bashrc"
fi

__zmux_urlencode() {
  local s="$1" o="" i c
  for (( i=0; i<${#s}; i++ )); do
    c="${s:$i:1}"
    case "$c" in
      [-_.~a-zA-Z0-9/]) o+="$c" ;;
      *) printf -v c '%%%02X' "'$c"; o+="$c" ;;
    esac
  done
  printf '%s' "$o"
}

__zmux_osc7() {
  printf '\033]7;file://%s%s\033\\' "${HOSTNAME:-localhost}" "$(__zmux_urlencode "$PWD")"
}

# PROMPT_COMMAND is an array in bash 5.1+; appending a string would break it.
if [[ "$(declare -p PROMPT_COMMAND 2>/dev/null)" == "declare -a"* ]]; then
  PROMPT_COMMAND+=(__zmux_osc7)
else
  PROMPT_COMMAND="__zmux_osc7${PROMPT_COMMAND:+;$PROMPT_COMMAND}"
fi

# Report immediately so a pane knows its directory before the first command.
__zmux_osc7
"#;

/// Decode an OSC 7 payload into a path.
///
/// Accepts `file://host/path` and a bare path. Returns the MSYS-style path as
/// the shell reported it; conversion to a Windows path is the caller's job.
pub fn parse_osc7(payload: &str) -> Option<String> {
    let rest = match payload.strip_prefix("file://") {
        Some(r) => match r.find('/') {
            Some(i) => &r[i..], // skip the host component
            None => return None,
        },
        None if payload.starts_with('/') => payload,
        None => return None,
    };
    Some(percent_decode(rest))
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
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
    fn leaves_non_drive_paths_alone_apart_from_separators() {
        assert_eq!(msys_to_windows("/usr/bin"), r"\usr\bin");
    }
}
