#!/usr/bin/env python3
"""Interactive PTY probe for AGY (`agy` TUI), verifying harness.md section 10 item 5:
- Interactive SessionStart / PreInvocation / PreToolUse / PostToolUse / PostInvocation / Stop
- WaitMsBeforeAsync=500 rewrite + idle Stop(fullyIdle=False) + automatic wakeup + Stop(fullyIdle=True)
- SessionStart / PreInvocation / PostInvocation injectSteps reaching model in interactive TUI
- Interactive process ancestry (TUI -> child language server -> hook / run_command) and clean exit when TUI closes
"""
import json, os, shutil, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
sys.path.insert(0, os.path.join(HERE, "plugin-agy", "bin"))
import probe_agy_lib as plib
from agy_run import AgyRun, MODEL, ROOT, hook_events_only
from probelib import Table
from ptydrv import Pty

T = Table()


def wait_until(p, cond, timeout, step=0.4):
    end = time.time() + timeout
    while time.time() < end:
        p.pump(step)
        v = cond()
        if v:
            return v
        if not p.alive():
            return cond()
    return cond()


def start_tui(run, prompt):
    p = Pty(["agy", "-i", prompt, "--model", MODEL], run.repo, run.env())
    # Handle folder trust prompt if shown on a fresh temp dir
    if p.wait_for(r"Do you trust the contents of this project|Auto mode:", timeout=20):
        if "Do you trust the contents of this project" in p.buf:
            p.send("\r")
            p.pump(2.0)
            # Check if SessionStart fired right after trusting; if workspace plugins
            # are only scanned when opening an already-trusted workspace, relaunch once.
            if not wait_until(p, lambda: run.events("SessionStart"), timeout=6):
                p.send("\x04\x04")
                wait_until(p, lambda: not p.alive(), timeout=6)
                p.kill()
                for fn in ("deny-ix.txt", "fail-emptyout-ix.txt"):
                    if os.path.exists(run.path(fn)):
                        os.unlink(run.path(fn))
                open(run.log, "w").close()
                p = Pty(["agy", "-i", prompt, "--model", MODEL], run.repo, run.env())
    return p


def main():
    os.makedirs(ROOT, exist_ok=True)
    run = AgyRun("ix", flags=["inject", "allow_edit", "allow_bash", "bg_rewrite", "deny_edit"])
    bin_dir = os.path.realpath(os.path.join(run.plugin_dir, "bin"))
    ver = subprocess.check_output(["agy", "--version"], text=True).strip()

    prompt = (
        "1) Call write_file on '%s' with Content 'x' (if denied, continue). "
        "2) Call write_file on '%s' with Content 'y'. "
        "3) Call run_command with WaitMsBeforeAsync=5000 (foreground) and CommandLine: python3 '%s/probe-wait' 6 ixrw . "
        "Immediately end your turn with the word STARTED without polling. "
        "When the background command completes and wakes you up, reply quoting verbatim all PROBE-CTX-* tokens you received."
    ) % (run.path("deny-ix.txt"), run.path("fail-emptyout-ix.txt"), bin_dir)

    p = start_tui(run, prompt)
    try:
        ok = wait_until(
            p,
            lambda: (
                run.events("bin-wait-end")
                and any(
                    s["ts"] > run.events("bin-wait-end")[0]["ts"] and s["input"].get("fullyIdle") is True
                    for s in run.events("Stop")
                )
            ),
            timeout=90,
        )
        p.pump(1.5)
        p.send("\x04\x04")
        wait_until(p, lambda: not p.alive(), timeout=10)
    finally:
        p.kill()
        open(os.path.join(run.dir, "screen.txt"), "w").write(p.buf[-12000:])

    time.sleep(1.0)
    hev = hook_events_only(run)
    names = {r["event"] for r in hev}
    want = {"SessionStart", "PreInvocation", "PreToolUse", "PostToolUse", "PostInvocation", "Stop"}
    T.add("interactive (-i) hooks fire", sorted(want), sorted(names & want), want <= names)

    T.add(
        "interactive PreToolUse deny & 0-byte pass-through",
        "deny-ix.txt absent, fail-emptyout-ix.txt present",
        (not os.path.exists(run.path("deny-ix.txt")), os.path.exists(run.path("fail-emptyout-ix.txt"))),
        not os.path.exists(run.path("deny-ix.txt")) and os.path.exists(run.path("fail-emptyout-ix.txt")),
    )

    stops = run.events("Stop")
    we = run.events("bin-wait-end")
    stop_seq = [(s["input"].get("terminationReason"), s["input"].get("fullyIdle")) for s in stops]
    T.add("interactive Stop(fullyIdle) sequence (info)", "[(..., False), (..., True)]", stop_seq, None)
    woke = bool(we and any(r["ts"] > we[0]["ts"] for r in run.events("PreInvocation")))
    T.add("interactive bg completion wakes idle TUI", "PreInvocation after bin-wait-end & final Stop fullyIdle=True", (woke, bool(ok)), woke and bool(ok))

    for marker in ("PROBE-CTX-SessionStart", "PROBE-CTX-PreInvocation", "PROBE-CTX-PostInvocation"):
        seen = marker in p.buf
        T.add("interactive %s reaches model on screen" % marker, "present in TUI output", seen, seen)

    ppids = {r["ppid"] for r in hev}
    ppid = next(iter(ppids)) if ppids else 0
    ws = run.events("bin-wait-start")
    cmd_has_ppid = bool(ws and ppid in ws[0]["ancestors"])
    T.add(
        "interactive hook ppid (child LS) shared w/ run_command & exits with TUI",
        "1 distinct ppid, in run_command ancestors, alive=False after exit",
        (sorted(ppids), cmd_has_ppid, plib.proc_alive(ppid) if ppid else None),
        len(ppids) == 1 and cmd_has_ppid and not plib.proc_alive(ppid),
    )

    print("AGY %s interactive (pty), %s\n" % (ver, time.strftime("%Y-%m-%d")))
    print(T.render())


if __name__ == "__main__":
    main()
