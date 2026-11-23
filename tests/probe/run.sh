#!/bin/sh
# Repeatable hook probes. Usage: tests/probe/run.sh [headless|interactive|all]   (default: all)
# Needs claude on PATH and python3 (stdlib only). Uses --model haiku; ~14 headless runs + ~5 interactive turns.
# Temp files go to $PROBE_TMP (default /tmp/tcas-probe); nothing global is touched.
set -e
here=$(cd "$(dirname "$0")" && pwd)
mode=${1:-all}
[ "$mode" = headless ] || [ "$mode" = all ] && python3 "$here/run.py"
[ "$mode" = interactive ] || [ "$mode" = all ] && python3 "$here/interactive.py"
exit 0
