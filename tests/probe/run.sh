#!/bin/sh
# Repeatable hook probes.
# Usage: tests/probe/run.sh [headless|interactive|claude|agy-headless|agy-interactive|agy|all]   (default: all)
# Needs claude and/or agy on PATH and python3 (stdlib only).
# Temp files go to $PROBE_TMP / $PROBE_AGY_TMP (/tmp/tcas-probe*); nothing global is touched.
set -e
here=$(cd "$(dirname "$0")" && pwd)
mode=${1:-all}
case "$mode" in
  headless)         python3 "$here/run.py" ;;
  interactive)      python3 "$here/interactive.py" ;;
  claude)           python3 "$here/run.py"; python3 "$here/interactive.py" ;;
  agy-headless)     python3 "$here/agy_run.py" ;;
  agy-interactive)  python3 "$here/agy_interactive.py" ;;
  agy)              python3 "$here/agy_run.py"; python3 "$here/agy_interactive.py" ;;
  all)
    python3 "$here/run.py"
    python3 "$here/interactive.py"
    python3 "$here/agy_run.py"
    python3 "$here/agy_interactive.py"
    ;;
  *)
    echo "usage: $0 [headless|interactive|claude|agy-headless|agy-interactive|agy|all]" >&2
    exit 2
    ;;
esac
exit 0
