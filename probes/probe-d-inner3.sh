E=/c/Users/eusth/src/zmux-probes/probe-d/target/release/probe-d.exe
D=/c/Users/eusth/src/zmux-probes
"$E" "$D/r-direct.txt"
winpty "$E" "$D/r-winpty.txt" 2>/dev/null || echo "winpty failed" > "$D/r-winpty.txt"
