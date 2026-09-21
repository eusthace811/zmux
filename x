#!/usr/bin/env bash
# Build / test / install helper, usable from either host:
#
#   WSL       The source lives here (so git does too), but the compiler must
#             be the Windows MSVC toolchain. Three things make that work, all
#             learned the hard way:
#               1. cargo.exe reads the source over \\wsl.localhost fine.
#               2. It CANNOT put target/ there - 9p has no lock files, so the
#                  incremental session lock fails. CARGO_TARGET_DIR must be
#                  Windows-local.
#               3. WSL does not forward env vars to Windows processes without
#                  WSLENV.
#   Git Bash  Native cargo. Two cases:
#               - repo on a Windows disk (the work laptop): nothing to do.
#               - repo on the WSL share: same target/ rule as above, and for
#                 a second reason. A //wsl.localhost/... path reaches link.exe,
#                 which reads the leading / as the start of an option, ignores
#                 every .o as an "unrecognized option" (LNK4044) and then dies
#                 with LNK1561, entry point must be defined.
#
#   ./x test | build | release | clippy | fmt | run   (anything else -> cargo)
#   ./x install [dir]      build --release and copy both binaries to dir
#                          (default ~/bin, which Git Bash already has on PATH)
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

case "$(uname -s)" in
  MINGW*|MSYS*) HOST=win ;;
  *)            HOST=wsl ;;
esac

if [ "$HOST" = wsl ]; then
  # Ask Windows where home is rather than hardcoding a name: this has to run
  # on a laptop that is not this one.
  WINHOME_W="${ZMUX_WINHOME:-$(cd /mnt/c && cmd.exe /c 'echo %USERPROFILE%' 2>/dev/null | tr -d '\r\n')}"
  if [ -z "$WINHOME_W" ]; then
    echo "x: cannot find the Windows home directory; set ZMUX_WINHOME='C:\\Users\\you'" >&2
    exit 1
  fi
  WINHOME_U="$(wslpath -u "$WINHOME_W")"
  CARGO="$WINHOME_U/.cargo/bin/cargo.exe"
  MANIFEST="$(wslpath -w "$HERE/Cargo.toml")"
  export CARGO_TARGET_DIR="${WINHOME_W}\\.zmux-target"
  export CARGO_INCREMENTAL=0
  export WSLENV=CARGO_TARGET_DIR:CARGO_INCREMENTAL
  RELEASE_DIR="$WINHOME_U/.zmux-target/release"
  BIN_DEFAULT="$WINHOME_U/bin"
  TASKLIST=/mnt/c/Windows/System32/tasklist.exe
else
  CARGO=cargo
  # Hand cargo a real Windows path: cygpath turns //wsl.localhost/... into
  # \\wsl.localhost\..., which link.exe cannot mistake for an option.
  MANIFEST="$(cygpath -w "$HERE/Cargo.toml")"
  if [ -n "${CARGO_TARGET_DIR:-}" ]; then
    TARGET_U="$(cygpath -u "$CARGO_TARGET_DIR")"
  else
    case "$HERE" in
      //*) # Repo on the WSL share. target/ has to go somewhere Windows-local,
           # for the linker reason above and the 9p lock reason. Same
           # directory the WSL branch uses, so the two share one build cache.
           TARGET_U="$HOME/.zmux-target"
           export CARGO_TARGET_DIR="$(cygpath -w "$TARGET_U")" ;;
      *)   TARGET_U="$HERE/target" ;;
    esac
  fi
  RELEASE_DIR="$TARGET_U/release"
  BIN_DEFAULT="$HOME/bin"
  TASKLIST=tasklist
fi

# Insert --manifest-path right after the subcommand: appending it would land
# after any `--`, where it reaches the test binary instead of cargo.
run_cargo() { local sub="$1"; shift; "$CARGO" "$sub" --manifest-path "$MANIFEST" "$@"; }

# Windows refuses to replace a running .exe, so say WHY before the copy fails
# with "Access is denied".
refuse_if_running() {
  command -v "$TASKLIST" >/dev/null 2>&1 || return 0
  local live
  live="$("$TASKLIST" 2>/dev/null | grep -ci '^zmux' || true)"
  [ "${live:-0}" -eq 0 ] && return 0
  echo "x: zmux is still running, and Windows will not replace a running .exe." >&2
  echo "   Detach with C-b d if you are attached, then end the sessions:" >&2
  local names=""
  [ -x "$RELEASE_DIR/zmux.exe" ] &&
    names="$("$RELEASE_DIR/zmux.exe" ls 2>/dev/null | grep -v '^no live sessions$' || true)"
  if [ -n "$names" ]; then
    while read -r s; do echo "     zmux kill $s" >&2; done <<< "$names"
  else
    echo "     zmux ls      # then: zmux kill <session>" >&2
  fi
  exit 1
}

case "${1:-test}" in
  release) shift; run_cargo build --release "$@"
           echo "binaries: $RELEASE_DIR/{zmux,zmuxd}.exe" ;;

  install) shift
           dest="${1:-$BIN_DEFAULT}"
           refuse_if_running
           run_cargo build --release
           mkdir -p "$dest"
           # BOTH, always: the client starts the daemon by looking next to
           # itself, so a half-installed pair fails at the first attach.
           cp -f "$RELEASE_DIR/zmux.exe" "$RELEASE_DIR/zmuxd.exe" "$dest/"
           echo "installed zmux.exe and zmuxd.exe to $dest"
           if [ "$dest" = "$BIN_DEFAULT" ]; then
             # Git Bash exports $HOME/bin:$PATH unconditionally, in
             # /etc/profile.d/env.sh - so there is nothing to add, only a
             # stale command cache to clear.
             echo "~/bin is already on Git Bash's PATH: run 'hash -r' there, then: zmux"
           else
             case ":$PATH:" in
               *":$dest:"*) echo "run 'hash -r', then: zmux" ;;
               *) echo "note: $dest is not on PATH. Add:"
                  echo "        export PATH=\"$dest:\$PATH\"" ;;
             esac
           fi ;;

  run)     shift; run_cargo build --release
           echo "Run this from a Git Bash window (it needs a console):"
           if [ "$HOST" = wsl ]; then
             echo "  $(wslpath -w "$RELEASE_DIR/zmux.exe")"
           else
             echo "  $RELEASE_DIR/zmux.exe"
           fi ;;

  *)       sub="$1"; shift; run_cargo "$sub" "$@" ;;
esac
