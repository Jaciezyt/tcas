#!/usr/bin/env python3
"""Probe hook: logs every event as one JSON line to $PROBE_LOG, behaviour from files in $PROBE_CTL.

Control files (existence = on):
  envfile     SessionStart writes TCAS_SESSION=probe-<session_id> into CLAUDE_ENV_FILE
  inject      SessionStart/UserPromptSubmit/PostToolUse return additionalContext PROBE-CTX-<event>
  allow_edit  PreToolUse allows Write/Edit/NotebookEdit
  allow_bash  PreToolUse allows Bash commands containing "probe-"
  bg_rewrite  PreToolUse allows and rewrites Bash commands starting with "probe-wait" to run_in_background
  deny_edit   PreToolUse denies (JSON permissionDecision) edits of files named deny-*
Edits of files named fail-exit1* / fail-exit2* / fail-badjson* / fail-timeout* make the hook misbehave
(exit 1 / exit 2 with stderr / invalid JSON on stdout / sleep past the timeout).
"""
import json, os, sys, time

ENV_KEYS = ["CLAUDE_ENV_FILE", "CLAUDE_PROJECT_DIR", "TCAS_SESSION", "CLAUDE_CODE_SESSION_ID"]


def flag(name):
    return os.path.exists(os.path.join(os.environ.get("PROBE_CTL", "/nonexistent"), name))


def log(rec):
    p = os.environ.get("PROBE_LOG")
    if p:
        with open(p, "a") as f:
            f.write(json.dumps(rec) + "\n")


def out(obj):
    sys.stdout.write(json.dumps(obj))
    sys.stdout.flush()


def main():
    raw = sys.stdin.read()
    try:
        d = json.loads(raw)
    except Exception:
        d = {"_raw": raw}
    ev = d.get("hook_event_name") or (sys.argv[1] if len(sys.argv) > 1 else "?")
    log({"event": ev, "ts": time.time(), "pid": os.getpid(), "ppid": os.getppid(),
         "env": {k: os.environ.get(k) for k in ENV_KEYS}, "input": d})

    def hso(**kw):
        return {"hookSpecificOutput": dict(hookEventName=ev, **kw)}

    if ev == "SessionStart":
        ef = os.environ.get("CLAUDE_ENV_FILE")
        if flag("envfile") and ef:
            with open(ef, "a") as f:
                f.write("export TCAS_SESSION=probe-%s\n" % d.get("session_id"))
    if ev in ("SessionStart", "UserPromptSubmit", "PostToolUse") and flag("inject"):
        out(hso(additionalContext="PROBE-CTX-" + ev))
        return
    if ev != "PreToolUse":
        return

    tool = d.get("tool_name")
    ti = d.get("tool_input") or {}
    if tool in ("Write", "Edit", "NotebookEdit", "MultiEdit"):
        base = os.path.basename(ti.get("file_path") or ti.get("notebook_path") or "")
        if base.startswith("fail-exit1"):
            sys.exit(1)
        if base.startswith("fail-exit2"):
            sys.stderr.write("probe: blocked by exit 2\n")
            sys.exit(2)
        if base.startswith("fail-badjson"):
            sys.stdout.write("{this is not json")
            return
        if base.startswith("fail-timeout"):
            time.sleep(30)
            return
        if base.startswith("deny-") and flag("deny_edit"):
            out(hso(permissionDecision="deny", permissionDecisionReason="probe: deny-* files are protected"))
            return
        if flag("allow_edit"):
            out(hso(permissionDecision="allow", permissionDecisionReason="probe: edits allowed"))
        return
    if tool == "Bash":
        cmd = ti.get("command", "")
        if cmd.startswith("probe-wait") and flag("bg_rewrite"):
            new = dict(ti)
            new["run_in_background"] = True
            out(hso(permissionDecision="allow", permissionDecisionReason="probe: rewritten to background",
                    updatedInput=new))
        elif "probe-" in cmd and flag("allow_bash"):
            out(hso(permissionDecision="allow", permissionDecisionReason="probe: bash allowed"))


main()
