#!/bin/sh
# Phase 0 probes A, D, E - run this INSIDE a Git Bash (mintty) window.
echo "================ PROBE E: target Windows build ================"
cmd.exe /c ver 2>/dev/null | tr -d '\r'
echo "(Win11 24H2+ matters for ConPTY teardown - DESIGN.md 2.5)"
echo
echo "================ PROBE D: is mintty a real console? ================"
echo "-- D1: native exe console width, no pcon override:"
powershell.exe -NoProfile -Command "[Console]::WindowWidth" 2>&1 | tr -d '\r' | head -3
echo "-- D2: same, forcing MSYS ConPTY:"
MSYS=enable_pcon powershell.exe -NoProfile -Command "[Console]::WindowWidth" 2>&1 | tr -d '\r' | head -3
echo "   A NUMBER on either line = real console = mintty works as host (0.1)."
echo "   An exception on both     = fall back to Windows Terminal."
echo "-- D3: MSYS runtime has ConPTY support compiled in?"
strings /usr/bin/msys-2.0.dll 2>/dev/null | grep -c pcon | sed 's/^/   pcon symbols: /'
echo "-- D4: winpty fallback present?"
command -v winpty >/dev/null && echo "   winpty: yes" || echo "   winpty: NO"
echo
echo "================ PROBE A: crates.io / Artifactory ================"
echo "-- A1: what registry is configured?"
cat ~/.cargo/config.toml 2>/dev/null | grep -A3 -i "source\|registry" | head -20
echo "   (empty = plain crates.io; a [source.crates-io] replace-with = Artifactory)"
echo "-- A2: reachability (NOTE: crates.io 403s a default curl UA!)"
curl -sS -o /dev/null -w "   crates.io api: %{http_code}\n" \
  -A "zmux-probe" --max-time 20 https://crates.io/api/v1/crates/vt100 2>&1
echo "-- A3: real resolution test"
cd "$(mktemp -d)" && cargo.exe new --quiet probe-a >/dev/null 2>&1 && cd probe-a \
  && cargo.exe add portable-pty vt100 crossterm tokio ratatui interprocess bincode 2>&1 | tail -6
echo "   SUCCESS = caching remote, count is irrelevant."
echo "   BLOCKED = allowlist: use 'cargo vendor --versioned-dirs' (DESIGN.md 6.1)."
