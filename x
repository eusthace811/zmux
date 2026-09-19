#!/usr/bin/env bash
# Build/test/run helper. The source lives in WSL (so git lives here), but the
# compiler must be the Windows MSVC toolchain.
#
# Two things make that work, both learned the hard way:
#   1. cargo.exe reads the source over the \\wsl.localhost UNC path fine.
#   2. It CANNOT put target/ there - 9p has no lock-file support, so the
#      incremental session lock fails. CARGO_TARGET_DIR must be Windows-local.
#   3. WSL does not forward env vars to Windows processes without WSLENV.
#
#   ./x test | build | release | run | clippy | fmt   (anything else -> cargo)
set -euo pipefail

CARGO=/mnt/c/Users/eusth/.cargo/bin/cargo.exe
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MANIFEST="$(wslpath -w "$HERE/Cargo.toml")"

export CARGO_TARGET_DIR='C:\Users\eusth\.zmux-target'
export CARGO_INCREMENTAL=0
export WSLENV=CARGO_TARGET_DIR:CARGO_INCREMENTAL

target_unix=/mnt/c/Users/eusth/.zmux-target

case "${1:-test}" in
  release) shift; "$CARGO" build --release --manifest-path "$MANIFEST" "$@"
           echo "binaries: $target_unix/release/{zmux,zmuxd}.exe" ;;
  run)     shift; "$CARGO" build --release --manifest-path "$MANIFEST"
           echo "Run this from a Git Bash window (it needs a console):"
           echo "  $(wslpath -w "$target_unix/release/zmux.exe")" ;;
  *)       # Insert --manifest-path right after the subcommand: appending it
           # would land after any `--`, where it reaches the test binary
           # instead of cargo.
           sub="$1"; shift
           "$CARGO" "$sub" --manifest-path "$MANIFEST" "$@" ;;
esac
