#!/usr/bin/env python3
"""Headless hook probes (claude -p). Prints a results table, writes examples to $PROBE_TMP/examples.json.

Usage: python3 tests/probe/run.py [--only name,name]   (names: basic resume bg rewrite failmodes missing editfail bypass denyrule subagent)
Needs: claude on PATH, python3. ~10 model runs with --model haiku.
"""
import json, os, shutil, sys, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from probelib import *

T = Table()
EXAMPLES = {}


def collect_examples(run):
    for r in run.events():
        key = r["event"]
        if key in ("bin-anc", "bin-wait-start", "bin-wait-end"):
            continue
        tn = r["input"].get("tool_name")
        if key in ("PreToolUse", "PostToolUse", "PostToolUseFailure", "PermissionRequest") and tn:
            key = "%s[%s]" % (key, tn)
        EXAMPLES.setdefault(key, trim(r["input"]))


def hook_ppids(run):
    return {r["ppid"] for r in run.events() if r["event"] not in ("bin-anc", "bin-wait-start", "bin-wait-end")}


# ---------------------------------------------------------------- basic
def basic():
    run = Run("basic", files={"src/a.txt": "hello needle world\n", "src/b.txt": "nothing\n"})
    prompt = (
        "Do these steps in order, each with exactly the tool named, no extra commentary. "
        "1) Bash: probe-anc direct  "
        "2) Bash: env -u TCAS_SESSION -u CLAUDE_CODE_SESSION_ID probe-anc envu  "
        "3) Bash: bash -c 'probe-anc bashc'  "
        "4) Bash: probe-anc amp & wait  "
        "5) Bash: probe-fail  "
        "6) Write tool: create deny-1.txt with content x  "
        "7) Write tool: create ok-1.txt with content y  "
        "8) Read tool: src/a.txt  "
                "If a step is refused or fails, continue with the next step. Finally reply with the word DONE.")
    j = run.claude_headless(prompt, max_turns=25)
    collect_examples(run)
    sid = j.get("session_id")
    ev = run.events()
    names = {r["event"] for r in ev}
    want = {"SessionStart", "UserPromptSubmit", "PreToolUse", "PostToolUse", "PostToolUseFailure", "Stop", "SessionEnd"}
    T.add("hooks fire in headless mode", "all of " + ",".join(sorted(want)), sorted(names & want) if want - names else "all present",
          not (want - names))
    perm = run.events("PermissionRequest")
    T.add("PermissionRequest observed (info)", "-", "%d events" % len(perm), None)
    # deny
    denied = bool(tool_events(run, "PreToolUse", "Write")) and not os.path.exists(run.path("deny-1.txt"))
    T.add("PreToolUse JSON deny blocks edit", "deny-1.txt absent", "deny-1.txt %s" % ("absent" if not os.path.exists(run.path("deny-1.txt")) else "CREATED"), denied)
    T.add("allow lets edit through (control)", "ok-1.txt present", "present" if os.path.exists(run.path("ok-1.txt")) else "absent",
          os.path.exists(run.path("ok-1.txt")))
    post_after_deny = [r for r in run.events("PostToolUse", "PostToolUseFailure")
                       if (r["input"].get("tool_input") or {}).get("file_path", "").endswith("deny-1.txt")]
    T.add("denied call has no Post event", "0 Post events for deny-1.txt", len(post_after_deny), len(post_after_deny) == 0)
    # bin on PATH
    anc = {r["label"]: r for r in run.events("bin-anc")}
    T.add("plugin bin/ on Bash PATH", "probe-anc ran", sorted(anc), "direct" in anc)
    # env file
    ss = run.events("SessionStart")
    others = [r for r in ev if r["event"] not in ("SessionStart", "bin-anc", "bin-wait-start", "bin-wait-end")]
    T.add("CLAUDE_ENV_FILE only in SessionStart", "set in SessionStart, empty elsewhere",
          "SessionStart=%s; other events with value: %d" % (bool(ss and ss[0]["env"]["CLAUDE_ENV_FILE"]), sum(1 for r in others if r["env"]["CLAUDE_ENV_FILE"])),
          bool(ss and ss[0]["env"]["CLAUDE_ENV_FILE"]) and not any(r["env"]["CLAUDE_ENV_FILE"] for r in others))
    d = anc.get("direct")
    T.add("CLAUDE_ENV_FILE var reaches Bash", "TCAS_SESSION=probe-<sid>", d and d["env"]["TCAS_SESSION"],
          bool(d) and d["env"]["TCAS_SESSION"] == "probe-%s" % sid)
    T.add("CLAUDE_CODE_SESSION_ID == session_id", "equal", d and d["env"]["CLAUDE_CODE_SESSION_ID"], bool(d) and d["env"]["CLAUDE_CODE_SESSION_ID"] == sid)
    pp = hook_ppids(run)
    T.add("all hooks share one ppid (harness)", "1 distinct ppid", sorted(pp), len(pp) == 1)
    # SessionEnd
    se = run.events("SessionEnd")
    T.add("SessionEnd fires at end of -p turn", "1 event, reason other", [(r["input"].get("reason")) for r in se], len(se) == 1)
    # ancestors
    h = next(iter(pp)) if pp else None
    for lab, text in (("direct", "direct"), ("envu", "env -u"), ("bashc", "bash -c"), ("amp", "background '&'")):
        r = anc.get(lab)
        T.add("Bash ancestors include harness pid (%s)" % text, "harness pid %s in chain" % h,
              (r["ancestors"][:6] if r else "command not run"), bool(r) and h in r["ancestors"])
    e = anc.get("envu")
    if e:
        T.add("env -u removes both vars (so ancestors needed)", "both None", (e["env"]["TCAS_SESSION"], e["env"]["CLAUDE_CODE_SESSION_ID"]),
              e["env"]["TCAS_SESSION"] is None and e["env"]["CLAUDE_CODE_SESSION_ID"] is None)
    # failure
    f = bash_events(run, "PostToolUseFailure", "probe-fail")
    T.add("failing command -> PostToolUseFailure", "event with error", (f[0]["input"].get("error") if f else "none"), bool(f) and "error" in f[0]["input"])
    post_fail = bash_events(run, "PostToolUse", "probe-fail")
    T.add("failing command has no PostToolUse", "0", len(post_fail), len(post_fail) == 0)
    # read / grep
    rd = tool_events(run, "PostToolUse", "Read")
    T.add("Read input carries file_path", "file_path", rd and rd[0]["input"]["tool_input"].get("file_path"), bool(rd) and "file_path" in rd[0]["input"]["tool_input"])
    if rd:
        T.add("hook paths are symlink-resolved", "/private/tmp/...", rd[0]["input"]["tool_input"].get("file_path"),
              rd[0]["input"]["tool_input"].get("file_path", "").startswith("/private/tmp"))
    return run, sid


def resume(sid):
    run = Run("basic-resume-dummy")  # not used; resume must share repo+log with basic
    shutil.rmtree(run.dir, ignore_errors=True)
    base = Run.__new__(Run)
    base.name, base.dir = "basic", os.path.join(ROOT, "basic")
    base.repo, base.ctl, base.log = base.dir + "/repo", base.dir + "/ctl", base.dir + "/log.jsonl"
    before = len(base.events())
    j = base.claude_headless("Use Bash to run: probe-anc resumed   Then reply DONE.", extra=["--resume", sid], max_turns=8)
    new = base.events()[before:]
    ss = [r for r in new if r["event"] == "SessionStart"]
    T.add("resume keeps session id", "same session_id, SessionStart source=resume",
          [(r["input"].get("source"), r["input"].get("session_id") == sid) for r in ss],
          bool(ss) and ss[0]["input"].get("source") == "resume" and ss[0]["input"].get("session_id") == sid)
    old_pp = {r["ppid"] for r in base.events()[:before] if r["event"] == "SessionStart"}
    T.add("resume has a new harness pid", "different ppid", (sorted(old_pp), ss and ss[0]["ppid"]), bool(ss) and ss[0]["ppid"] not in old_pp)
    a = [r for r in new if r["event"] == "bin-anc"]
    T.add("TCAS_SESSION present after resume", "probe-<sid>", a and a[0]["env"]["TCAS_SESSION"], bool(a) and a[0]["env"]["TCAS_SESSION"] == "probe-%s" % sid)
    T.add("SessionEnd per -p turn", "2 total (one per turn)", len(base.events("SessionEnd")), len(base.events("SessionEnd")) == 2)


# ---------------------------------------------------------------- bg
def bg_checks(run, j, label, rewrite):
    ev = run.events()
    ws = run.events("bin-wait-start")
    we = run.events("bin-wait-end")
    post = bash_events(run, "PostToolUse", "probe-wait")
    pre = bash_events(run, "PreToolUse", "probe-wait")
    stops = run.events("Stop")
    ups = run.events("UserPromptSubmit")
    tn = [r for r in ups if str(r["input"].get("prompt", "")).lstrip().startswith("<task-notification>")]
    tid = post and (post[0]["input"].get("tool_response") or {}).get("backgroundTaskId")
    early = bool(post and we and post[0]["ts"] < we[0]["ts"] - 1.5)
    if rewrite:
        T.add("updatedInput rewrites fg Bash to background", "PreToolUse input has no run_in_background; PostToolUse early with backgroundTaskId",
              "pre.run_in_background=%s, backgroundTaskId=%s, post %.1fs before cmd end" % (pre and pre[0]["input"]["tool_input"].get("run_in_background"), tid, (we[0]["ts"] - post[0]["ts"]) if post and we else -1),
              bool(pre) and not pre[0]["input"]["tool_input"].get("run_in_background") and bool(tid) and early)
    else:
        T.add("background Bash PostToolUse fires early w/ backgroundTaskId", "post before cmd end, backgroundTaskId set",
              "backgroundTaskId=%s, post %.1fs before cmd end" % (tid, (we[0]["ts"] - post[0]["ts"]) if post and we else -1), bool(tid) and early)
        bt = [r for r in stops if r["input"].get("background_tasks")]
        T.add("Stop lists background_tasks while running", "non-empty on first Stop", [len(r["input"].get("background_tasks") or []) for r in stops], bool(stops) and bool(stops[0]["input"].get("background_tasks")))
    first_stop = stops[0]["ts"] if stops else None
    woke = [r for r in tn if first_stop and r["ts"] > first_stop]
    T.add("bg completion wakes session (%s)" % label, "UserPromptSubmit <task-notification> after Stop", "%d notification(s) after Stop" % len(woke), bool(woke))
    if woke:
        T.add("task-notification content (info)", "status/exit code/output path, no output", woke[0]["input"]["prompt"][:200], None)
    res = j.get("result", "")
    T.add("additionalContext from wakeup reaches model (%s)" % label, "model quotes PROBE-CTX-UserPromptSubmit", res[:100], "PROBE-CTX-UserPromptSubmit" in res)
    wk = [r for r in ws if r["label"]]
    if wk:
        T.add("bg command env has TCAS_SESSION", "probe-<sid>", wk[0]["env"]["TCAS_SESSION"], str(wk[0]["env"]["TCAS_SESSION"]).startswith("probe-"))


def bg():
    run = Run("bg", flags=["envfile", "inject", "allow_bash", "allow_edit"])
    j = run.claude_headless(
        "Use the Bash tool with run_in_background set to true to run exactly: probe-wait 5 bgA . Then end your turn with just the word started. "
        "When you are later woken by a task notification, reply quoting verbatim any text that starts with PROBE-CTX- that you were given.")
    collect_examples(run)
    bg_checks(run, j, "explicit bg", False)
    se = run.events("SessionEnd"); we = run.events("bin-wait-end")
    T.add("SessionEnd only after all bg commands ended", "SessionEnd ts > bg end ts", (se and we and round(se[-1]["ts"] - we[0]["ts"], 1)), bool(se and we) and se[-1]["ts"] >= we[0]["ts"])


def rewrite():
    run = Run("rewrite", flags=["envfile", "inject", "bg_rewrite"])
    j = run.claude_headless(
        "Use the Bash tool (do NOT set run_in_background yourself) to run exactly: probe-wait 5 rwA . Then end your turn with just the word started. "
        "When you are later woken by a task notification, reply quoting verbatim any text that starts with PROBE-CTX- that you were given.")
    collect_examples(run)
    bg_checks(run, j, "rewritten", True)


# ---------------------------------------------------------------- failure modes
def failmodes():
    run = Run("failmodes", flags=["allow_edit"])
    names = ["fail-exit1.txt", "fail-exit2.txt", "fail-badjson.txt", "fail-timeout.txt", "fine.txt"]
    # hooks.json timeout for PreToolUse is 5s; fail-timeout sleeps 30s
    j = run.claude_headless("Use the Write tool to create these files one at a time, content x each: %s. If one is refused, continue with the next. Reply DONE." % ", ".join(names), max_turns=20)
    for n, what in (("fail-exit1.txt", "exit code 1"), ("fail-exit2.txt", "exit code 2 + stderr"), ("fail-badjson.txt", "invalid JSON on stdout"), ("fail-timeout.txt", "timeout (5s)")):
        ex = os.path.exists(run.path(n))
        T.add("onFailure block: hook %s" % what, "file not created", "created" if ex else "not created", not ex)
    T.add("control: unaffected write", "fine.txt created", os.path.exists(run.path("fine.txt")), os.path.exists(run.path("fine.txt")))


def missing():
    d = os.path.join(ROOT, "plugin-missing")
    shutil.rmtree(d, ignore_errors=True)
    shutil.copytree(PLUGIN, d)
    hj = json.load(open(d + "/hooks/hooks.json"))
    hj["hooks"]["PreToolUse"][0]["hooks"][0]["command"] = '"${CLAUDE_PLUGIN_ROOT}/bin/does-not-exist" PreToolUse'
    json.dump(hj, open(d + "/hooks/hooks.json", "w"), indent=2)
    run = Run("missing", flags=["allow_edit"])
    j = run.claude_headless("Use the Write tool to create missing-1.txt with content x. Report exactly what the tool said.", plugin=d, max_turns=6)
    ex = os.path.exists(run.path("missing-1.txt"))
    T.add("onFailure block: hook program missing", "file not created; reason mentions onFailure", "%s; reply: %s" % ("created" if ex else "not created", j.get("result", "")[:120]), not ex)


def editfail():
    run = Run("editfail", flags=["allow_edit", "allow_bash"], files={
        "src/a.txt": "hello needle world\n",
        "edit-me.txt": "alpha beta\n",
        "nb.ipynb": json.dumps({"cells": [{"cell_type": "code", "metadata": {}, "source": ["print(1)"], "outputs": [], "execution_count": None, "id": "c1"}],
                                "metadata": {}, "nbformat": 4, "nbformat_minor": 5})})
    j = run.claude_headless(
        "Perform these tool calls exactly, one each, in order, with no pre-checks: "
        "(1) Grep tool with pattern 'needle' and path 'src'. "
        "(2) Edit tool on edit-me.txt with old_string 'zzz-not-there' and new_string 'y' (this is a deliberate failure test: the text is absent, the call will error, do not retry). "
        "(3) Edit tool on never-read.txt with old_string 'a' and new_string 'b' (deliberate failure test, file was never read). "
        "(4) Read nb.ipynb then NotebookEdit on it: cell_id c1, new_source print(2). Then reply DONE.",
        max_turns=20, extra=["--disallowedTools", "Bash"])
    collect_examples(run)
    gr = tool_events(run, "PostToolUse", "Grep")
    T.add("Grep input carries path; result filenames", "tool_input.path + tool_response.filenames",
          gr and (gr[0]["input"]["tool_input"].get("path"), (gr[0]["input"].get("tool_response") or {}).get("filenames")),
          bool(gr) and "path" in gr[0]["input"]["tool_input"])
    # failed Edit: the call is rejected during input validation; check which hook events it produces
    r2 = Run("editfail2", flags=["allow_edit"], files={"edit-me.txt": "alpha beta\n"})
    j2 = r2.claude_headless("Call the Edit tool on edit-me.txt with old_string 'zzz-not-there' and new_string 'y'. I know the text is absent; I am testing the error message, so make the call anyway, without reading first. Then tell me the exact error.", max_turns=6)
    called = "not found" in j2.get("result", "")
    tool_ev = [(r["event"]) for r in r2.events() if r["event"] in ("PreToolUse", "PostToolUse", "PostToolUseFailure", "PermissionRequest")]
    T.add("failed Edit (old_string not found): hook events", "model got the error; record events (finding: none)", "error seen=%s; tool hook events=%s" % (called, tool_ev or "NONE"), None if called else False)
    T.add("failed Edit (old_string not found) -> PostToolUseFailure", "no PreToolUse/Post* at all (validation fails before hooks)", tool_ev or "NONE", bool(called) and not tool_ev)
    nb = [r for r in run.events("PreToolUse") if r["input"].get("tool_name") == "NotebookEdit"]
    keys = sorted(nb[0]["input"]["tool_input"]) if nb else "NotebookEdit not called"
    T.add("NotebookEdit path field name", "notebook_path", keys, bool(nb) and "notebook_path" in nb[0]["input"]["tool_input"])


def bypass2():
    run = Run("bypass", flags=["deny_edit", "allow_bash"])
    import subprocess
    cmd = ["claude", "-p", "Use the Write tool to create deny-bp.txt with content x, then use Bash to run: probe-anc bypass. If refused, continue. Reply DONE.",
           "--model", MODEL, "--permission-mode", "bypassPermissions", "--max-turns", "8", "--plugin-dir", PLUGIN, "--output-format", "json"]
    try:
        subprocess.run(cmd, cwd=run.repo, env=run.env(), capture_output=True, text=True, timeout=200)
    except subprocess.TimeoutExpired:
        pass
    pre = tool_events(run, "PreToolUse", "Write")
    ex = os.path.exists(run.path("deny-bp.txt"))
    T.add("bypassPermissions still runs PreToolUse", "PreToolUse logged, permission_mode=bypassPermissions", [r["input"].get("permission_mode") for r in pre], bool(pre))
    T.add("bypassPermissions: PreToolUse deny still blocks", "deny-bp.txt absent", "created" if ex else "absent", not ex)


def denyrule():
    run = Run("denyrule", flags=["envfile", "allow_bash"], settings={"permissions": {"deny": ["Bash(probe-anc:*)"]}})
    j = run.claude_headless("Use the Bash tool to run exactly: probe-anc denyrule . Then reply DONE.", max_turns=6)
    ran = [r for r in run.events("bin-anc") if r["label"] == "denyrule"]
    T.add("hook allow vs user settings deny rule", "deny rule wins: command not run", "command %s" % ("RAN (hook allow overrode deny rule)" if ran else "did not run"), not ran)


def subagent():
    run = Run("subagent", flags=["envfile", "allow_bash"])
    j = run.claude_headless("Use the Agent tool (general-purpose subagent) to run the Bash command 'probe-anc sub' and report its output. Then reply DONE.", max_turns=10)
    collect_examples(run)
    names = {r["event"] for r in run.events()}
    T.add("SubagentStart / SubagentStop fire", "both present", sorted(n for n in names if n.startswith("Subagent")), {"SubagentStart", "SubagentStop"} <= names)
    sub = [r for r in run.events("PreToolUse") if r["input"].get("agent_id")]
    sid = run.events("SessionStart")[0]["input"]["session_id"] if run.events("SessionStart") else None
    T.add("subagent tool hooks: same session_id + agent_id/agent_type", "session_id equal, agent_id present",
          sub and (sub[0]["input"]["session_id"] == sid, sub[0]["input"].get("agent_id"), sub[0]["input"].get("agent_type")),
          bool(sub) and sub[0]["input"]["session_id"] == sid)
    a = run.events("bin-anc")
    T.add("subagent Bash has TCAS_SESSION", "probe-<sid>", a and a[0]["env"]["TCAS_SESSION"], bool(a) and a[0]["env"]["TCAS_SESSION"] == "probe-%s" % sid)


def main():
    only = None
    if "--only" in sys.argv:
        only = sys.argv[sys.argv.index("--only") + 1].split(",")
    os.makedirs(ROOT, exist_ok=True)
    ver = os.popen("claude --version").read().strip()
    sid = None
    steps = [("basic", None), ("resume", None), ("bg", bg), ("rewrite", rewrite), ("failmodes", failmodes), ("missing", missing),
             ("editfail", editfail), ("bypass", bypass2), ("denyrule", denyrule), ("subagent", subagent)]
    for name, fn in steps:
        if only and name not in only:
            continue
        sys.stderr.write("[run] %s\n" % name)
        try:
            if name == "basic":
                _, sid = basic()
            elif name == "resume":
                if not sid:
                    ev = Run.__new__(Run); ev.log = ROOT + "/basic/log.jsonl"
                    try: sid = ev.events("SessionStart")[0]["input"]["session_id"]
                    except Exception: continue
                resume(sid)
            else:
                fn()
        except Exception as e:
            T.add(name + " (runner error)", "-", repr(e), False)
    print("Claude Code %s, %s\n" % (ver, time.strftime("%Y-%m-%d")))
    print(T.render())
    json.dump(EXAMPLES, open(ROOT + "/examples.json", "w"), indent=1)
    sys.stderr.write("examples -> %s/examples.json\n" % ROOT)


if __name__ == "__main__":
    main()
