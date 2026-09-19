OUT=/c/Users/eusth/src/zmux-probes/probe-d-result.txt
{
  echo "### MSYS='$MSYS'  MSYSTEM=$MSYSTEM"
  /c/Users/eusth/src/zmux-probes/probe-d/target/release/probe-d.exe 2>&1
  echo
  echo "### same, but via winpty:"
  winpty /c/Users/eusth/src/zmux-probes/probe-d/target/release/probe-d.exe 2>&1
} > "$OUT" 2>&1
