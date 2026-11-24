#!/usr/bin/env python3
"""AGY Probe hook: logs every lifecycle event to $PROBE_LOG and responds per $PROBE_CTL flags.

Control flags in $PROBE_CTL:
  inject      SessionStart, PreInvocation, and PostToolUse->PostInvocation inject PROBE-CTX-<event>
  allow_edit  PreToolUse emits {"decision": "allow"} for file tools (unless filename overrides below)
  allow_bash  PreToolUse emits {"decision": "allow"} for run_command containing "probe-"
  bg_rewrite  PreToolUse rewrites foreground probe-wait run_command to WaitMsBeforeAsync=500
  deny_edit   PreToolUse denies files whose basename starts with "deny-"

Special basenames on PreToolUse for edit/write tools:
  ok-empty*      outputs {}
  ok-ask*        outputs {"decision": "ask", "reason": "probe ask"}
  ok-allow*      outputs {"decision": "allow", "reason": "probe allow"}
  fail-exit1*    exits 1 without stdout
  fail-exit2*    writes stderr and exits 2
  fail-badjson*  writes invalid JSON and exits 0
  fail-emptyout* writes 0 bytes and exits 0
  fail-timeout*  sleeps 15s (past hooks.json timeout=4)
"""
import json, os, sys, time

HERE = os.path.dirname(os.path.realpath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "bin"))
import probe_agy_lib as lib


def flag(name):
    return os.path.exists(os.path.join(lib.ctl_dir(), name))


def ctl_path(name):
    return os.path.join(lib.ctl_dir(), name)


def out(obj):
    sys.stdout.write(json.dumps(obj))
    sys.stdout.flush()


def extract_target_path(args):
    if not isinstance(args, dict):
        return ""
    for k in ("Path", "TargetFile", "AbsolutePath", "file_path", "path", "DirectoryPath", "SearchPath"):
        v = args.get(k)
        if isinstance(v, str) and v:
            return v
    return ""


def inject_msg(text):
    return {"injectSteps": [{"systemMessage": {"systemMessage": text}}]}


def main():
    raw = sys.stdin.read()
    try:
        d = json.loads(raw)
    except Exception:
        d = {"_raw": raw}
    ev = d.get("hookEventName") or (sys.argv[1] if len(sys.argv) > 1 else "?")
    anc_details = lib.ancestor_details()
    lib.log({
        "event": ev,
        "ts": time.time(),
        "pid": os.getpid(),
        "ppid": os.getppid(),
        "hook_cwd": os.getcwd(),
        "ancestors": [x["pid"] for x in anc_details],
        "ancestor_details": anc_details,
        "env": {k: os.environ.get(k) for k in lib.ENV_KEYS},
        "input": d,
    })

    if ev == "SessionStart":
        if flag("inject"):
            out(inject_msg("PROBE-CTX-SessionStart"))
        else:
            out({})
        return

    if ev == "PreInvocation":
        if flag("inject"):
            msgs = ["PROBE-CTX-PreInvocation"]
            p = ctl_path("pending_post_ctx")
            if os.path.exists(p):
                try:
                    txt = open(p).read().strip()
                    os.unlink(p)
                    if txt:
                        msgs.append(txt)
                except Exception:
                    pass
            out(inject_msg(" ".join(msgs)))
        else:
            out({})
        return

    if ev == "PostToolUse":
        if flag("inject"):
            try:
                with open(ctl_path("pending_post_ctx"), "w") as f:
                    f.write("PROBE-CTX-PostInvocation")
            except Exception:
                pass
        out({})
        return

    if ev == "PostInvocation":
        p = ctl_path("pending_post_ctx")
        if flag("inject") and os.path.exists(p):
            try:
                txt = open(p).read().strip()
                os.unlink(p)
                if txt:
                    out(inject_msg(txt))
                    return
            except Exception:
                pass
        out({})
        return

    if ev == "Stop":
        out({})
        return

    if ev != "PreToolUse":
        out({})
        return

    tc = d.get("toolCall") or {}
    tool = tc.get("name") or ""
    args = tc.get("args") or {}

    if tool == "run_command" or "CommandLine" in args:
        cmd = args.get("CommandLine") or args.get("command") or ""
        wait_ms = args.get("WaitMsBeforeAsync")
        if "probe-wait" in cmd and flag("bg_rewrite"):
            if wait_ms is None or (isinstance(wait_ms, (int, float)) and wait_ms > 500):
                out({
                    "decision": "allow",
                    "reason": "probe: rewritten to background",
                    "overwrite": {"WaitMsBeforeAsync": 500},
                })
                return
            out({"decision": "allow", "reason": "probe: already background"})
            return
        if "probe-" in cmd and flag("allow_bash"):
            out({"decision": "allow", "reason": "probe: bash allowed"})
            return
        out({"decision": "allow", "reason": "probe: default allow bash"})
        return

    target = extract_target_path(args)
    base = os.path.basename(target)
    if base.startswith("fail-exit1"):
        sys.exit(1)
    if base.startswith("fail-exit2"):
        sys.stderr.write("probe: blocked by exit 2\n")
        sys.exit(2)
    if base.startswith("fail-badjson"):
        sys.stdout.write("{this is not json")
        sys.stdout.flush()
        return
    if base.startswith("fail-emptyout"):
        return
    if base.startswith("fail-timeout"):
        time.sleep(15)
        return
    if base.startswith("ok-empty"):
        out({})
        return
    if base.startswith("ok-ask"):
        out({"decision": "ask", "reason": "probe: ask fallback"})
        return
    if base.startswith("ok-allow"):
        out({"decision": "allow", "reason": "probe: explicit allow"})
        return
    if base.startswith("deny-") and flag("deny_edit"):
        out({"decision": "deny", "reason": "probe: deny-* files are protected"})
        return
    if flag("allow_edit"):
        out({"decision": "allow", "reason": "probe: edits allowed"})
        return
    out({"decision": "ask"})


if __name__ == "__main__":
    main()
