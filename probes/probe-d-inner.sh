OUT=/c/Users/eusth/src/zmux-probes/probe-d-result.txt
{
  echo "=== MSYS env as seen by bash: '$MSYS' ==="
  echo "-- MSYSTEM=$MSYSTEM  TERM=$TERM"
  echo "-- native exe console check:"
  powershell.exe -NoProfile -Command "try{'CONSOLE_OK width=' + [Console]::WindowWidth}catch{'NO_CONSOLE: ' + \$_.Exception.GetType().Name}" 2>&1 | tr -d '\r'
  echo "-- is stdin a tty per MSYS? $( [ -t 0 ] && echo yes || echo no )"
  echo "-- stty size: $(stty size 2>&1)"
  echo "=== done ==="
} > "$OUT" 2>&1
