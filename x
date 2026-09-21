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
#   ./x test               bounded parallelism (RUST_TEST_THREADS, default 4),
#                          because every pane is a console and MSYS bash stops
#                          starting at 128 of them system-wide. It says so when
#                          that is what went wrong, instead of leaving you with
#                          a scatter of unrelated assertion failures.
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
  export WSLENV=CARGO_TARGET_DIR:CARGO_INCREMENTAL:RUST_TEST_THREADS:ZMUX_TEST_LOG
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

  # Every integration test starts a daemon, and every pane it opens is a
  # console. MSYS bash refuses to start once the SYSTEM has 128 of them -
  # "console device allocation failure - too many consoles in use" - and the
  # symptom is not an error but a pane that quietly vanishes, which surfaces
  # as a handful of unrelated assertion failures in whichever tests were
  # unlucky. Measured on a machine already holding ~130 consoles: 6 threads
  # green 3/3 with no pane lost, 8 threads 0/3 with 6 lost, 12 threads 0/3
  # with 40 lost. The suite is process-bound, not CPU-bound, so a low count
  # costs little; set RUST_TEST_THREADS yourself to override.
  test)    # `./x` on its own means `./x test`, so there may be nothing to
           # shift off - and a failing `shift` under `set -e` would end the
           # script before it ran a thing.
           if [ "$#" -gt 0 ]; then shift; fi
           export RUST_TEST_THREADS="${RUST_TEST_THREADS:-4}"
           # Collect each daemon's stderr, which is the only place a lost pane
           # explains itself. Without this the cause is unreachable.
           # A directory the daemons can write to, and a unix path to read it
           # back through. When the caller supplied one, keep it - but keep
           # READING it too: an earlier version dropped the diagnosis
           # entirely in that case, which is exactly when someone is already
           # trying to work out what went wrong.
           if [ -z "${ZMUX_TEST_LOG:-}" ]; then
             logs="$(mktemp -d)"
             trap 'rm -rf "$logs"' EXIT
           elif [ "$HOST" = wsl ]; then
             logs="$(wslpath -u "$ZMUX_TEST_LOG")"
           else
             logs="$(cygpath -u "$ZMUX_TEST_LOG")"
           fi
           mkdir -p "$logs"
           if [ "$HOST" = wsl ]; then
             export ZMUX_TEST_LOG="$(wslpath -w "$logs")"
           else
             export ZMUX_TEST_LOG="$(cygpath -w "$logs")"
           fi
           status=0
           run_cargo test "$@" || status=$?
           # Say WHY, not just that. A run that failed because the machine ran
           # out of consoles is not a failing test suite, and reading it as
           # one costs an afternoon.
           if [ "$status" -ne 0 ]; then
             if grep -qs 'too many consoles' "$logs"/*.log; then
               echo "" >&2
               echo "x: the shells in these tests could not start - Windows is out of consoles." >&2
               echo "   MSYS bash gives up at 128 SYSTEM-WIDE, and this machine is already near it:" >&2
               echo "     conhost.exe running now: $("$TASKLIST" 2>/dev/null | grep -ci '^conhost' || echo '?')" >&2
               echo "   This is not a failing test. Close some terminals, or lower RUST_TEST_THREADS" >&2
               echo "   (currently $RUST_TEST_THREADS), and run again." >&2
             elif grep -qs 'exited with status' "$logs"/*.log; then
               echo "" >&2
               echo "x: panes went away during the run; their daemons said:" >&2
               grep -hs 'exited with status' "$logs"/*.log | sort | uniq -c | sed 's/^/     /' >&2
             fi
           fi
           exit "$status" ;;

  *)       sub="$1"; shift; run_cargo "$sub" "$@" ;;
esac
