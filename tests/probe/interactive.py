#!/usr/bin/env python3
"""Interactive-mode probes: drives a real `claude` TUI through a pty. ~5 model turns with --model haiku.

Usage: python3 tests/probe/interactive.py [--extra-bypass]
Writes the table to stdout, raw timeline to $PROBE_TMP/ix/timeline.txt.
Trust dialog: answered with Down+Enter ("Yes, I trust"), written to ~/.claude.json by claude itself for the temp dir only.
Permissions: project .claude/settings.json allow-list Bash(probe-*) + --permission-mode default.
"""
import json, os, signal, sys, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from probelib import *
from ptydrv import Pty

T = Table()
TL = []


def note(s):
    TL.append("%.1f %s" % (time.time() - T0, s))
    sys.stderr.write(TL[-1] + "\n")


def clean_env(run):
    e = {k: v for k, v in run.env().items()
         if not (k.startswith("CLAUDE_CODE_") or k in ("CLAUDECODE", "CLAUDE_AGENT_SDK_VERSION", "CLAUDE_PID", "CLAUDE_EFFORT"))}
    e["DISABLE_AUTOUPDATER"] = "1"
    return e


def start(run, extra=()):
    argv = ["claude", "--model", MODEL, "--permission-mode", "default", "--plugin-dir", PLUGIN] + list(extra)
    p = Pty(argv, run.repo, clean_env(run))
    p.pump(3)
    if "trust this folder" in p.buf.lower() or "Yes, I trust" in p.buf:
        p.send("\x1b[B"); p.pump(0.5); p.send("\r"); note("trust dialog answered")
    ok = wait_until(p, lambda: run.events("SessionStart"), 40)
    p.pump(3)
    note("session started: %s" % ok)
    return p


def wait_until(p, cond, timeout, step=0.5):
    end = time.time() + timeout
    while time.time() < end:
        p.pump(step)
        v = cond()
        if v:
            return v
        if not p.alive():
            return cond()
    return cond()


def say(p, text):
    p.pump(1)
    p.send(text); p.pump(0.7); p.send("\r")
    note("sent: %s" % text[:70])


def stops(run):
    return len(run.events("Stop"))


def turn(p, run, text, timeout=90):
    n = stops(run)
    say(p, text)
    ok = wait_until(p, lambda: stops(run) > n, timeout)
    note("turn done=%s" % ok)
    return ok


def alive_pid(pid):
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False
    except PermissionError:
        return True


def survive_variant(run_dir_tag):
    """(d) second variant: choose 'Move to background and exit' and see whether the cmd survives."""
    run = Run(run_dir_tag, flags=["envfile", "inject", "bg_rewrite"], settings={"permissions": {"allow": ["Bash(probe-*)"]}})
    p = start(run)
    pid = None
    try:
        turn(p, run, "Use the Bash tool, WITHOUT setting run_in_background, to run exactly: probe-wait 40 ixbg2 . Then end your turn with just the word started.")
        sv = wait_until(p, lambda: [r for r in run.events("bin-wait-start") if r["label"] == "ixbg2"], 20)
        pid = sv[0]["pid"] if sv else None
        say(p, "/exit")
        wait_until(p, lambda: "Move to background" in p.buf[-3000:] or not p.alive(), 15)
        if p.alive():
            p.send("\x1b[B"); p.pump(0.5); p.send("\r")  # option 2
        gone = wait_until(p, lambda: not p.alive(), 20)
        if not gone:
            p.send("\x03"); p.pump(0.5); p.send("\x03"); wait_until(p, lambda: not p.alive(), 10)
        time.sleep(2)
        alive = bool(pid) and alive_pid(pid)
        T.add("(d) bg command after 'Move to background and exit'", "survives (measured)", "claude exited=%s; pid %s alive 2s later: %s" % (not p.alive(), pid, alive), None if pid else False)
        if alive:
            time.sleep(40)
            ended = [r for r in run.events("bin-wait-end") if r["label"] == "ixbg2"]
            T.add("(d) detached bg command ran to completion after claude exit", "bin-wait-end logged", bool(ended), None)
    finally:
        if pid and alive_pid(pid):
            os.kill(pid, signal.SIGKILL)
        p.kill()


def main():
    global T0
    T0 = time.time()
    os.makedirs(ROOT + "/ix", exist_ok=True)
    run = Run("ix", flags=["envfile", "inject", "bg_rewrite"], settings={"permissions": {"allow": ["Bash(probe-*)"]}})
    ver = os.popen("claude --version").read().strip()
    p = start(run)
    survivor = None
    try:
        # ---- (a) + (e): plain foreground Bash
        turn(p, run, "Use the Bash tool to run exactly this command and nothing else: probe-anc ixdirect . Then reply with the single word ok.")
        ev = run.events()
        names = {r["event"] for r in ev}
        sess = run.events("SessionStart")
        ss = sess[0] if sess else None
        sid = ss["input"]["session_id"] if ss else None
        want = {"SessionStart", "UserPromptSubmit", "PreToolUse", "PostToolUse", "Stop"}
        T.add("(a) hooks fire in interactive mode", "SessionStart,UserPromptSubmit,PreToolUse,PostToolUse,Stop", sorted(names & want), want <= names)
        T.add("(a) SessionStart source in interactive", "startup", ss and ss["input"].get("source"), bool(ss) and ss["input"].get("source") == "startup")
        T.add("(a) PermissionRequest fired for allow-listed/hook-approved cmd (info)", "-", len(run.events("PermissionRequest")), None)
        T.add("(a) CLAUDE_ENV_FILE only in SessionStart", "set only in SessionStart",
              [r["event"] for r in ev if (r.get("env") or {}).get("CLAUDE_ENV_FILE")], bool(ss) and bool(ss["env"]["CLAUDE_ENV_FILE"]) and [r["event"] for r in ev if (r.get("env") or {}).get("CLAUDE_ENV_FILE")] == ["SessionStart"])
        hp = {r["ppid"] for r in ev if "input" in r}
        T.add("(a) hook ppid is one harness pid", "1 distinct ppid == pty child pid or its descendant", "ppids=%s, spawned pid=%s" % (sorted(hp), p.pid), len(hp) == 1)
        anc = [r for r in run.events("bin-anc") if r["label"] == "ixdirect"]
        T.add("(e) Bash ancestors include harness pid", "hook ppid in chain", anc and (anc[0]["ancestors"][:6], sorted(hp)), bool(anc) and bool(hp) and next(iter(hp)) in anc[0]["ancestors"])
        T.add("(a) TCAS_SESSION / CLAUDE_CODE_SESSION_ID in Bash", "probe-<sid> / <sid>", anc and (anc[0]["env"]["TCAS_SESSION"], anc[0]["env"]["CLAUDE_CODE_SESSION_ID"]),
              bool(anc) and anc[0]["env"]["TCAS_SESSION"] == "probe-%s" % sid and anc[0]["env"]["CLAUDE_CODE_SESSION_ID"] == sid)
        T.add("(a) hook sees CLAUDE_CODE_SESSION_ID too", "== session_id", ss and ss["env"]["CLAUDE_CODE_SESSION_ID"], bool(ss) and ss["env"]["CLAUDE_CODE_SESSION_ID"] == sid)

        # ---- (b) + (c): foreground command rewritten to background, turn ends, completion wakes idle session
        n_up = len(run.events("UserPromptSubmit"))
        turn(p, run, "Use the Bash tool, WITHOUT setting run_in_background, to run exactly: probe-wait 12 ixrw . Then end your turn immediately with just the word started. "
                     "If you are later woken by a task notification, reply quoting verbatim any text beginning with PROBE-CTX- that was given to you.")
        pre = bash_events(run, "PreToolUse", "probe-wait 12 ixrw")
        post = bash_events(run, "PostToolUse", "probe-wait 12 ixrw")
        ws = [r for r in run.events("bin-wait-start") if r["label"] == "ixrw"]
        tid = post and (post[0]["input"].get("tool_response") or {}).get("backgroundTaskId")
        lat = (post[0]["ts"] - pre[0]["ts"]) if pre and post else None
        T.add("(b) updatedInput fg->bg works interactively", "PostToolUse early w/ backgroundTaskId, cmd not finished",
              "backgroundTaskId=%s, pre->post %.2fs, pre.run_in_background=%s" % (tid, lat if lat is not None else -1, pre and pre[0]["input"]["tool_input"].get("run_in_background")),
              bool(tid) and lat is not None and lat < 6)
        stop_t = run.events("Stop")[-1]["ts"]
        we = lambda: [r for r in run.events("bin-wait-end") if r["label"] == "ixrw"]
        T.add("(c) Stop fired before bg cmd ended", "Stop ts < cmd end", "ended yet=%s" % bool(we()), not we())
        wait_until(p, lambda: we(), 40)
        woke = wait_until(p, lambda: [r for r in run.events("UserPromptSubmit")[n_up:] if str(r["input"].get("prompt", "")).lstrip().startswith("<task-notification>")], 40)
        T.add("(c) idle session woken by bg completion (no keypress)", "UserPromptSubmit <task-notification> after Stop",
              "%d notification(s); delay after cmd end %s s" % (len(woke), woke and we() and round(woke[0]["ts"] - we()[0]["ts"], 1)), bool(woke))
        if woke:
            T.add("(c) task-notification prompt (info)", "-", woke[0]["input"]["prompt"][:160], None)
            wait_until(p, lambda: stops(run) >= 3 and run.events("Stop")[-1]["ts"] > woke[0]["ts"], 60)
            p.pump(1)
            T.add("(c) additionalContext from wakeup reaches model", "screen shows PROBE-CTX-UserPromptSubmit in model reply",
                  "on screen: %s" % ("PROBE-CTX-UserPromptSubmit" in p.buf.split("<task-notification>")[-1] if "<task-notification>" in p.buf else "PROBE-CTX-UserPromptSubmit" in p.buf[-4000:]),
                  "PROBE-CTX-UserPromptSubmit" in p.buf[-6000:])
            T.add("(c) hook ctx on wakeup UserPromptSubmit event", "UserPromptSubmit logged with TCAS_SESSION env intact", woke[0]["env"]["CLAUDE_CODE_SESSION_ID"] == sid, woke[0]["input"]["session_id"] == sid)

        # ---- (f) /clear
        n_ss = len(run.events("SessionStart"))
        n_se = len(run.events("SessionEnd"))
        say(p, "/clear")
        wait_until(p, lambda: len(run.events("SessionStart")) > n_ss, 20)
        p.pump(1)
        ss2 = run.events("SessionStart")[n_ss:]
        se2 = run.events("SessionEnd")[n_se:]
        T.add("(f) /clear fires SessionStart source=clear", "source clear", [r["input"].get("source") for r in ss2], bool(ss2) and ss2[0]["input"].get("source") == "clear")
        new_sid = ss2[0]["input"]["session_id"] if ss2 else None
        T.add("(f) /clear changes session_id", "new id (rule in harness.md s2)", "old=%s new=%s" % (sid, new_sid), None if new_sid is None else new_sid != sid)
        T.add("(f) /clear fires SessionEnd for old session", "SessionEnd reason clear", [(r["input"].get("reason"), r["input"].get("session_id") == sid) for r in se2], bool(se2))
        T.add("(f) /clear: same harness ppid", "same ppid", ss2 and (ss2[0]["ppid"], ss and ss["ppid"]), bool(ss2) and bool(ss) and ss2[0]["ppid"] == ss["ppid"])
        turn(p, run, "Use the Bash tool to run exactly: probe-anc afterclear . Then reply ok.")
        ac = [r for r in run.events("bin-anc") if r["label"] == "afterclear"]
        T.add("(f) after /clear Bash env: TCAS_SESSION / CLAUDE_CODE_SESSION_ID", "new session's values (probe-<new>)",
              ac and (ac[0]["env"]["TCAS_SESSION"], ac[0]["env"]["CLAUDE_CODE_SESSION_ID"], "new_sid=%s" % new_sid),
              bool(ac) and bool(new_sid) and ac[0]["env"]["CLAUDE_CODE_SESSION_ID"] == new_sid and ac[0]["env"]["TCAS_SESSION"] == "probe-%s" % new_sid)

        # ---- (d) background command vs session exit
        turn(p, run, "Use the Bash tool, WITHOUT setting run_in_background, to run exactly: probe-wait 45 ixsurvive . Then end your turn with just the word started.")
        sv = wait_until(p, lambda: [r for r in run.events("bin-wait-start") if r["label"] == "ixsurvive"], 20)
        survivor = sv[0]["pid"] if sv else None
        note("survivor pid %s" % survivor)
        say(p, "/exit")
        dlg = wait_until(p, lambda: "Exit and stop tasks" in p.buf[-3000:] or not p.alive(), 15)
        note("exit dialog shown: %s" % ("Exit and stop tasks" in p.buf[-3000:]))
        T.add("(d) /exit with bg cmd running shows a confirmation dialog", "dialog 'Exit and stop tasks / Move to background and exit / Stay'", "Exit and stop tasks" in p.buf[-3000:], None)
        if p.alive():
            p.send("\r")  # option 1: Exit and stop tasks
        gone = wait_until(p, lambda: not p.alive(), 20)
        if not gone:
            note("process did not exit; ctrl-c x2")
            p.send("\x03"); p.pump(0.5); p.send("\x03"); wait_until(p, lambda: not p.alive(), 10)
        p.pump(1)
        se_last = run.events("SessionEnd")[-1:]
        T.add("(d) interactive claude exits after choosing 'Exit and stop tasks'", "process exits", "exited=%s; SessionEnd reason=%s" % (not p.alive(), se_last and se_last[0]["input"].get("reason")), not p.alive())
        time.sleep(2)
        alive_after = bool(survivor) and alive_pid(survivor)
        T.add("(d) bg command after 'Exit and stop tasks'", "killed (measured)", "pid %s alive 2s after exit: %s" % (survivor, alive_after), None if survivor else False)
        if survivor and alive_after:
            os.kill(survivor, signal.SIGKILL)
        survive_variant(run_dir_tag="ix2")
    finally:
        if survivor and alive_pid(survivor):
            try:
                os.kill(survivor, signal.SIGKILL)
            except Exception:
                pass
        p.kill()
        open(ROOT + "/ix/timeline.txt", "w").write("\n".join(TL) + "\n\n--- final screen tail ---\n" + p.buf[-3000:])
    print("Claude Code %s interactive (pty), %s\n" % (ver, time.strftime("%Y-%m-%d")))
    print(T.render())


if __name__ == "__main__":
    main()
