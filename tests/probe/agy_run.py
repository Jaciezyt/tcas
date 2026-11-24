#!/usr/bin/env python3
"""Headless hook probes for AGY (`agy -p`), covering harness.md section 10 items 1-6.

Usage: python3 tests/probe/agy_run.py [--only name,name]
  (names: basic resume bg rewrite failmodes missing subagent modes)
Needs: agy on PATH, python3. Writes examples to $PROBE_AGY_TMP/examples.json.
Does NOT modify ~/.gemini/config (places workspace plugin in <repo>/.agents/plugins/tcas-probe).
"""
import json, os, shutil, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "plugin-agy", "bin"))
import probe_agy_lib as plib
from probelib import Table, trim

PLUGIN_SRC = os.path.join(HERE, "plugin-agy")
ROOT = os.environ.get("PROBE_AGY_TMP", "/tmp/tcas-probe-agy")
MODEL = os.environ.get("PROBE_AGY_MODEL", "gemini-3.8-flash-low")
ALL_FLAGS = ["inject", "allow_edit", "allow_bash", "bg_rewrite", "deny_edit"]

T = Table()
EXAMPLES = {}


class AgyRun:
    """One temp git repo + .agents/plugins.json + log + control dir."""

    def __init__(self, name, flags=ALL_FLAGS, files=None, plugin_src=PLUGIN_SRC):
        self.name = name
        self.dir = os.path.join(ROOT, name)
        shutil.rmtree(self.dir, ignore_errors=True)
        self.repo = os.path.join(self.dir, "repo")
        self.ctl = os.path.join(self.dir, "ctl")
        self.log = os.path.join(self.dir, "log.jsonl")
        os.makedirs(self.repo)
        os.makedirs(self.ctl)
        open(self.log, "w").close()
        for f in flags:
            open(os.path.join(self.ctl, f), "w").close()
        for rel, content in (files or {}).items():
            p = os.path.join(self.repo, rel)
            os.makedirs(os.path.dirname(p), exist_ok=True)
            open(p, "w").write(content)
        subprocess.run(["git", "init", "-q", "-b", "main"], cwd=self.repo, check=True)
        subprocess.run(["git", "add", "-A"], cwd=self.repo, check=True)
        subprocess.run(
            ["git", "-c", "user.name=p", "-c", "user.email=p@p", "commit", "-qm", "init", "--allow-empty"],
            cwd=self.repo,
            check=True,
        )
        # Place plugin directly at <repo>/.agents/plugins/tcas-probe AND write probe_cfg.json
        # so hooks find log/ctl even if AGY's language server scrubs custom env vars.
        self.plugins_parent = os.path.join(self.repo, ".agents", "plugins")
        self.plugin_dir = os.path.join(self.plugins_parent, "tcas-probe")
        os.makedirs(self.plugins_parent, exist_ok=True)
        shutil.copytree(plugin_src, self.plugin_dir)
        with open(os.path.join(self.plugin_dir, "probe_cfg.json"), "w") as f:
            json.dump({"log": self.log, "ctl": self.ctl, "env_was_passed": True}, f)
        self.last_spawn_pid = None

    def env(self):
        e = {}
        for k, v in os.environ.items():
            if k.startswith("ANTIGRAVITY_") or k.startswith("CLAUDE") or k == "TCAS_SESSION":
                continue
            e[k] = v
        e["PROBE_LOG"] = self.log
        e["PROBE_CTL"] = self.ctl
        return e

    def events(self, *names):
        out = []
        if not os.path.exists(self.log):
            return out
        for l in open(self.log):
            try:
                r = json.loads(l)
            except Exception:
                continue
            if not names or r["event"] in names:
                out.append(r)
        return out

    def path(self, rel):
        return os.path.join(self.repo, rel)

    def agy_headless(self, prompt, mode=None, extra=(), timeout=240):
        cmd = [
            "agy",
            "-p",
            prompt,
            "--model",
            MODEL,
            "--disable-slash-commands",
            "--output-format",
            "json",
        ]
        if mode:
            cmd += ["--mode", mode]
        cmd += list(extra)
        t0 = time.time()
        proc = subprocess.Popen(
            cmd,
            cwd=self.repo,
            env=self.env(),
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        self.last_spawn_pid = proc.pid
        try:
            out, err = proc.communicate(timeout=timeout)
            rc = proc.returncode
        except subprocess.TimeoutExpired:
            proc.kill()
            out, err = proc.communicate()
            rc = -9
        try:
            j = json.loads(out)
            if not isinstance(j, dict):
                j = {"result": out, "_parsed": j}
        except Exception:
            j = {"result": out}
        j["_stderr"] = err
        j["_rc"] = rc
        j["_elapsed"] = time.time() - t0
        j["_spawn_pid"] = proc.pid
        json.dump(j, open(os.path.join(self.dir, "result.json"), "w"), indent=2)
        return j


def collect_examples(run):
    for r in run.events():
        key = r["event"]
        if key in ("bin-anc", "bin-wait-start", "bin-wait-end"):
            continue
        tc = (r.get("input") or {}).get("toolCall") or {}
        tn = tc.get("name")
        if key in ("PreToolUse", "PostToolUse") and tn:
            key = "%s[%s]" % (key, tn)
        EXAMPLES.setdefault(key, trim(r.get("input")))


def hook_events_only(run):
    return [r for r in run.events() if r["event"] not in ("bin-anc", "bin-wait-start", "bin-wait-end")]


def tool_events(run, event, tool=None):
    out = []
    for r in run.events(event):
        tc = (r.get("input") or {}).get("toolCall") or {}
        if tool is None or tc.get("name") == tool:
            out.append(r)
    return out


# ---------------------------------------------------------------- 1. basic
def basic():
    run = AgyRun(
        "basic",
        flags=["allow_edit", "allow_bash", "deny_edit"],
        files={"src/a.txt": "hello needle world\n", "src/b.txt": "before line\n"},
    )
    bin_dir = os.path.realpath(os.path.join(run.plugin_dir, "bin"))
    prompt = (
        "Execute these steps in exact order, one tool call per step, with no commentary. "
        "If any tool call fails or is denied, do NOT retry or work around it—immediately move to the next step:\n"
        "1) Call read_file on '%s'\n"
        "2) Call edit_file on '%s' replacing 'before line' with 'after line'\n"
        "3) Call write_file on '%s' with Content 'allow'\n"
        "4) Call write_file on '%s' with Content 'empty'\n"
        "5) Call write_file on '%s' with Content 'ask'\n"
        "6) Call write_file on '%s' with Content 'denied'\n"
        "7) Call run_command with CommandLine: probe-anc bare\n"
        "8) Call run_command with CommandLine: python3 '%s/probe-anc' direct\n"
        "9) Call run_command with CommandLine: env -u TCAS_SESSION -u ANTIGRAVITY_CONVERSATION_ID python3 '%s/probe-anc' envu\n"
        "10) Call run_command with CommandLine: python3 '%s/probe-fail'\n"
        "Finally reply with the single word DONE."
    ) % (
        run.path("src/a.txt"),
        run.path("src/b.txt"),
        run.path("ok-allow.txt"),
        run.path("ok-empty.txt"),
        run.path("ok-ask.txt"),
        run.path("deny-1.txt"),
        bin_dir,
        bin_dir,
        bin_dir,
    )
    j = run.agy_headless(prompt)
    collect_examples(run)
    hev = hook_events_only(run)
    names = {r["event"] for r in hev}
    want = {"SessionStart", "PreInvocation", "PreToolUse", "PostToolUse", "PostInvocation", "Stop"}
    T.add(
        "hooks fire in headless mode (agy -p)",
        "all of " + ",".join(sorted(want)),
        "all present" if not (want - names) else sorted(names),
        not (want - names),
    )
    cid = hev[0]["input"].get("conversationId") if hev else None
    T.add("conversationId present on all hooks", "same non-empty UUID", cid, bool(cid) and all(r["input"].get("conversationId") == cid for r in hev))

    # Item 1: tool names and arg keys
    seen_tools = {}
    for r in tool_events(run, "PreToolUse"):
        tc = r["input"].get("toolCall") or {}
        seen_tools.setdefault(tc.get("name"), sorted((tc.get("args") or {}).keys()))
    T.add("Item 1: toolCall.name -> arg keys (info)", "read/edit/write/cmd names & keys", seen_tools, bool(seen_tools))

    # Check hook cwd vs input.cwd and PLUGIN_ROOT
    if hev:
        hcwd = os.path.realpath(hev[0]["hook_cwd"])
        icwd = os.path.realpath(hev[0]["input"].get("cwd") or "")
        proot = os.path.realpath(hev[0]["env"].get("PLUGIN_ROOT") or "")
        pdir = os.path.realpath(run.plugin_dir)
        repo_real = os.path.realpath(run.repo)
        T.add(
            "hook runs in plugin root w/ PLUGIN_ROOT; input.cwd is repo",
            "hook_cwd==PLUGIN_ROOT==plugin_dir, input.cwd==repo",
            (hcwd == pdir, proot == pdir, icwd == repo_real),
            hcwd == pdir and proot == pdir and icwd == repo_real,
        )

    # Decisions: allow, {}, ask, deny
    T.add("PreToolUse decision=allow permits write", "ok-allow.txt present", os.path.exists(run.path("ok-allow.txt")), os.path.exists(run.path("ok-allow.txt")))
    T.add("PreToolUse empty {} behavior (info)", "record whether ok-empty.txt created", "created=%s" % os.path.exists(run.path("ok-empty.txt")), None)
    T.add("PreToolUse decision=ask in accept-edits mode (info)", "record whether ok-ask.txt created", "created=%s" % os.path.exists(run.path("ok-ask.txt")), None)
    denied_absent = not os.path.exists(run.path("deny-1.txt"))
    T.add("PreToolUse decision=deny blocks write", "deny-1.txt absent", "absent" if denied_absent else "CREATED", denied_absent)
    post_deny = [
        r for r in tool_events(run, "PostToolUse")
        if str(((r["input"].get("toolCall") or {}).get("args") or {})).find("deny-1.txt") >= 0
    ]
    T.add("denied call has no PostToolUse", "0 PostToolUse for deny-1.txt", len(post_deny), len(post_deny) == 0)

    # Item 6a: plugin bin/ on PATH?
    anc = {r["label"]: r for r in run.events("bin-anc")}
    T.add("Item 6: plugin bin/ on run_command PATH (info)", "check if 'bare' ran", "bare_ran=%s (labels=%s)" % ("bare" in anc, sorted(anc)), None)

    # Env var identity
    d = anc.get("direct")
    T.add(
        "ANTIGRAVITY_CONVERSATION_ID == conversationId",
        "equal",
        d and d["env"].get("ANTIGRAVITY_CONVERSATION_ID"),
        bool(d) and d["env"].get("ANTIGRAVITY_CONVERSATION_ID") == cid,
    )

    # Item 3: hook ppid, ancestor chain, and process lifetime after exit!
    time.sleep(1.0)
    ppids = {r["ppid"] for r in hev}
    first_anc = hev[0]["ancestor_details"] if hev else []
    anc_summary = [(a["pid"], a["name"], plib.proc_alive(a["pid"], a["started_at"])) for a in first_anc[:5]]
    T.add("Item 3: hook ancestor chain [(pid,name,alive_after_exit)] (info)", "identify session-scoped pid", anc_summary, None)
    T.add("Item 3: spawned agy CLI pid alive after -p exit", "False (exited)", plib.proc_alive(j["_spawn_pid"]), not plib.proc_alive(j["_spawn_pid"]))
    if d:
        cmd_anc_pids = set(d["ancestors"])
        hook_anc_pids = {a["pid"] for a in first_anc}
        shared = sorted(cmd_anc_pids & hook_anc_pids)
        shared_names = [(a["pid"], a["name"], plib.proc_alive(a["pid"], a["started_at"])) for a in first_anc if a["pid"] in cmd_anc_pids]
        T.add(
            "Item 3: run_command & hook share session-scoped ancestor",
            "shared ancestor with agy CLI/LS that exits with session",
            shared_names[:4],
            any(a["name"].startswith("agy") and not plib.proc_alive(a["pid"], a["started_at"]) for a in first_anc if a["pid"] in cmd_anc_pids),
        )
    e = anc.get("envu")
    if e and first_anc:
        hook_agy_pids = {a["pid"] for a in first_anc if a["name"].startswith("agy")}
        T.add(
            "Item 3: env -u strips vars, ancestor still matches agy pid",
            "ANTIGRAVITY_CONVERSATION_ID is None, agy pid in ancestors",
            (e["env"].get("ANTIGRAVITY_CONVERSATION_ID"), sorted(set(e["ancestors"]) & hook_agy_pids)),
            e["env"].get("ANTIGRAVITY_CONVERSATION_ID") is None and bool(set(e["ancestors"]) & hook_agy_pids),
        )

    # Failing command -> PostToolUse with error?
    post_cmds = [
        r for r in tool_events(run, "PostToolUse", "run_command")
        if "probe-fail" in (((r["input"].get("toolCall") or {}).get("args") or {}).get("CommandLine") or "")
    ]
    T.add(
        "failing run_command -> PostToolUse error field (info)",
        "inspect PostToolUse keys/error on non-zero exit",
        [(sorted(r["input"].keys()), r["input"].get("error")) for r in post_cmds],
        None,
    )
    return run, cid


# ---------------------------------------------------------------- 2. resume
def resume(cid):
    base = AgyRun.__new__(AgyRun)
    base.name = "basic"
    base.dir = os.path.join(ROOT, "basic")
    base.repo = os.path.join(base.dir, "repo")
    base.ctl = os.path.join(base.dir, "ctl")
    base.log = os.path.join(base.dir, "log.jsonl")
    base.plugins_parent = os.path.join(base.repo, ".agents", "plugins")
    base.plugin_dir = os.path.join(base.plugins_parent, "tcas-probe")
    open(os.path.join(base.ctl, "inject"), "w").close()
    before = len(base.events())
    bin_dir = os.path.realpath(os.path.join(base.plugin_dir, "bin"))
    j = base.agy_headless(
        "Run command: python3 '%s/probe-anc' resumed . Then reply quoting verbatim every PROBE-CTX-* token you see." % bin_dir,
        extra=["-r", cid],
    )
    new = base.events()[before:]
    hev = [r for r in new if r["event"] not in ("bin-anc", "bin-wait-start", "bin-wait-end")]
    ev_names = [r["event"] for r in hev]
    cids = {r["input"].get("conversationId") for r in hev}
    T.add("resume (-r) keeps conversationId", cid, sorted(cids), cids == {cid})
    T.add("resume (-r) hook events fired (info)", "check if SessionStart or PreInvocation fires", ev_names, None)


# ---------------------------------------------------------------- 3. bg & rewrite (Item 5)
def bg_checks(run, j, label, rewritten):
    ws = run.events("bin-wait-start")
    we = run.events("bin-wait-end")
    pre = [
        r for r in tool_events(run, "PreToolUse", "run_command")
        if "probe-wait" in (((r["input"].get("toolCall") or {}).get("args") or {}).get("CommandLine") or "")
    ]
    post = [
        r for r in tool_events(run, "PostToolUse", "run_command")
        if "probe-wait" in (((r["input"].get("toolCall") or {}).get("args") or {}).get("CommandLine") or "")
    ]
    stops = run.events("Stop")
    pre_invs = run.events("PreInvocation")
    post_invs = run.events("PostInvocation")

    # In AGY, backgrounding returns control to the model ~500ms after PreToolUse
    # (emitting PostInvocation + next PreInvocation while the command runs), and
    # PostToolUse fires only when the background command actually finishes!
    mid_invs = [r for r in post_invs if ws and we and ws[0]["ts"] < r["ts"] < we[0]["ts"] - 1.0]
    post_at_end = bool(post and we and abs(post[0]["ts"] - we[0]["ts"]) < 1.5)
    if rewritten:
        orig_wait = pre and ((pre[0]["input"].get("toolCall") or {}).get("args") or {}).get("WaitMsBeforeAsync")
        post_wait = post and ((post[0]["input"].get("toolCall") or {}).get("args") or {}).get("WaitMsBeforeAsync")
        T.add(
            "Item 5: overwrite WaitMsBeforeAsync=500 backgrounds fg command",
            "orig=5000, post=500, turn continues while running, PostToolUse at cmd end",
            "orig_wait=%s post_wait=%s mid_post_invs=%d post_at_end=%s" % (orig_wait, post_wait, len(mid_invs), post_at_end),
            orig_wait == 5000 and post_wait == 500 and bool(mid_invs) and post_at_end,
        )
    else:
        T.add(
            "Item 5: explicit WaitMsBeforeAsync=500 backgrounds command",
            "turn continues while running, PostToolUse fires when cmd ends",
            "mid_post_invs=%d post_at_end=%s" % (len(mid_invs), post_at_end),
            bool(mid_invs) and post_at_end,
        )

    stop_idle_seq = [(r["input"].get("terminationReason"), r["input"].get("fullyIdle")) for r in stops]
    T.add("Item 5: Stop sequence & fullyIdle (%s) (info)" % label, "Stop(fullyIdle=False) if idle before cmd end, final Stop(fullyIdle=True)", stop_idle_seq, None)
    if stops and we:
        after_end_pre = [r for r in pre_invs if r["ts"] >= we[0]["ts"]]
        T.add(
            "Item 5: bg completion triggers model wakeup PreInvocation (%s)" % label,
            "PreInvocation fires after bin-wait-end, final Stop fullyIdle=True",
            "after_end_PreInvocations=%d, final_fullyIdle=%s" % (len(after_end_pre), stops[-1]["input"].get("fullyIdle")),
            bool(after_end_pre) and stops[-1]["input"].get("fullyIdle") is True,
        )

    txt = json.dumps(j)
    for marker in ("PROBE-CTX-SessionStart", "PROBE-CTX-PreInvocation", "PROBE-CTX-PostInvocation"):
        T.add(
            "Item 5: %s reaches model (%s)" % (marker, label),
            "quoted in model response",
            marker in txt,
            marker in txt,
        )


def bg():
    run = AgyRun("bg", flags=["inject", "allow_edit", "allow_bash"])
    bin_dir = os.path.realpath(os.path.join(run.plugin_dir, "bin"))
    j = run.agy_headless(
        "Use run_command with WaitMsBeforeAsync set to 500 to execute: python3 '%s/probe-wait' 4 bgA . "
        "Do NOT poll or sleep; immediately end your turn saying 'started'. "
        "When the background command finishes and wakes you up, reply quoting verbatim all PROBE-CTX-* strings you have seen in this conversation (including SessionStart, PreInvocation, PostInvocation)." % bin_dir
    )
    collect_examples(run)
    bg_checks(run, j, "explicit-bg", False)


def rewrite():
    run = AgyRun("rewrite", flags=["inject", "allow_edit", "allow_bash", "bg_rewrite"])
    bin_dir = os.path.realpath(os.path.join(run.plugin_dir, "bin"))
    j = run.agy_headless(
        "Use run_command with WaitMsBeforeAsync set to 5000 (foreground) to execute: python3 '%s/probe-wait' 4 rwA . "
        "If it moves to background, do NOT poll; immediately end your turn saying 'started'. "
        "When woken up by completion, reply quoting verbatim all PROBE-CTX-* strings you have seen (SessionStart, PreInvocation, PostInvocation)." % bin_dir
    )
    collect_examples(run)
    bg_checks(run, j, "rewritten-bg", True)


# ---------------------------------------------------------------- 4. failmodes & missing (Item 2)
def failmodes():
    run = AgyRun("failmodes", flags=["allow_edit"])
    names = ["fail-exit1.txt", "fail-exit2.txt", "fail-badjson.txt", "fail-emptyout.txt", "fail-timeout.txt", "fine.txt"]
    paths = [run.path(n) for n in names]
    j = run.agy_headless(
        "Call write_file one at a time (in order) to create each of these files with Content 'x': %s . "
        "If any call errors or is blocked, do NOT retry—continue to the next file. Reply DONE." % ", ".join(paths)
    )
    for n, desc in (
        ("fail-exit1.txt", "exit code 1"),
        ("fail-exit2.txt", "exit code 2 + stderr"),
        ("fail-badjson.txt", "invalid JSON on stdout"),
        ("fail-emptyout.txt", "empty stdout (0 bytes)"),
        ("fail-timeout.txt", "timeout (>4s)"),
    ):
        ex = os.path.exists(run.path(n))
        T.add("Item 2: PreToolUse hook %s (info)" % desc, "measure default block vs allow", "CREATED (fail-open)" if ex else "BLOCKED (fail-closed)", None)
    T.add("Item 2 control: valid allow write", "fine.txt created", os.path.exists(run.path("fine.txt")), os.path.exists(run.path("fine.txt")))


def missing():
    bad_plugin = os.path.join(ROOT, "plugin-missing-src")
    shutil.rmtree(bad_plugin, ignore_errors=True)
    shutil.copytree(PLUGIN_SRC, bad_plugin)
    hj = json.load(open(os.path.join(bad_plugin, "hooks.json")))
    hj["tcas-probe"]["PreToolUse"][0]["hooks"][0]["command"] = "./bin/does-not-exist"
    json.dump(hj, open(os.path.join(bad_plugin, "hooks.json"), "w"), indent=2)
    run = AgyRun("missing", flags=["allow_edit"], plugin_src=bad_plugin)
    j = run.agy_headless("Call write_file to create '%s' with Content 'x'. Report what happened." % run.path("missing-1.txt"))
    ex = os.path.exists(run.path("missing-1.txt"))
    T.add("Item 2: missing hook binary (info)", "measure default block vs allow", "CREATED (fail-open)" if ex else "BLOCKED (fail-closed)", None)


# ---------------------------------------------------------------- 5. subagent (Item 4)
def subagent():
    run = AgyRun("subagent", flags=["allow_edit", "allow_bash"], files={"sub.txt": "hello sub\n"})
    bin_dir = os.path.realpath(os.path.join(run.plugin_dir, "bin"))
    j = run.agy_headless(
        "Call invoke_subagent once with TypeName='self', Model='flash', Role='Probe Subagent', "
        "Prompt=\"Call run_command with CommandLine: python3 '%s/probe-anc' sub   and then reply DONE.\" "
        "Wait for the subagent to finish, then reply DONE." % bin_dir
    )
    collect_examples(run)
    hev = hook_events_only(run)
    ss = run.events("SessionStart")
    parent_cid = ss[0]["input"].get("conversationId") if ss else None
    all_cids = []
    for r in hev:
        c = r["input"].get("conversationId")
        if c not in all_cids:
            all_cids.append(c)
    sub_anc = [r for r in run.events("bin-anc") if r["label"] == "sub"]
    sub_hooks = [r for r in hev if r["input"].get("conversationId") != parent_cid]
    T.add(
        "Item 4: subagent conversationId vs parent (info)",
        "inspect cids & fields",
        "parent=%s all_cids=%s sub_hook_count=%d" % (parent_cid, all_cids, len(sub_hooks)),
        None,
    )
    if sub_hooks:
        T.add(
            "Item 4: subagent hook input keys & transcriptPath (info)",
            "check parent link",
            (sorted(sub_hooks[0]["input"].keys()), sub_hooks[0]["input"].get("transcriptPath")),
            None,
        )
    if ss and sub_anc:
        parent_agy = {a["pid"] for a in ss[0]["ancestor_details"] if a["name"].startswith("agy")}
        sub_agy = set(sub_anc[0]["ancestors"]) & parent_agy
        T.add(
            "Item 4: subagent shares parent agy process in ancestors",
            "non-empty intersection",
            sorted(sub_agy),
            bool(sub_agy),
        )


# ---------------------------------------------------------------- 6. modes (Item 6)
def modes():
    # --dangerously-skip-permissions
    r1 = AgyRun("mode-skip-perms", flags=["allow_edit", "deny_edit"])
    r1.agy_headless(
        "Call write_file to create '%s' with Content 'x' and '%s' with Content 'y'. If denied, do not retry; reply DONE."
        % (r1.path("deny-skip.txt"), r1.path("ok-allow.txt")),
        extra=["--dangerously-skip-permissions"],
    )
    pre1 = tool_events(r1, "PreToolUse")
    T.add(
        "Item 6: --dangerously-skip-permissions runs PreToolUse & enforces deny",
        "PreToolUse fired, deny-skip.txt absent, ok-allow.txt present",
        (bool(pre1), not os.path.exists(r1.path("deny-skip.txt")), os.path.exists(r1.path("ok-allow.txt"))),
        bool(pre1) and not os.path.exists(r1.path("deny-skip.txt")) and os.path.exists(r1.path("ok-allow.txt")),
    )

    # --sandbox
    r2 = AgyRun("mode-sandbox", flags=["allow_edit", "allow_bash", "deny_edit"])
    bin_dir = os.path.realpath(os.path.join(r2.plugin_dir, "bin"))
    r2.agy_headless(
        "1) Call write_file to create '%s' with Content 'x'. "
        "2) Call write_file to create '%s' with Content 'y'. "
        "3) Call run_command with CommandLine: python3 '%s/probe-anc' sandboxed . "
        "If a step is denied, continue. Reply DONE."
        % (r2.path("deny-sb.txt"), r2.path("ok-allow.txt"), bin_dir),
        extra=["--sandbox"],
    )
    pre2 = tool_events(r2, "PreToolUse")
    T.add(
        "Item 6: --sandbox runs PreToolUse & enforces deny",
        "PreToolUse fired, deny-sb.txt absent, ok-allow.txt present",
        (bool(pre2), not os.path.exists(r2.path("deny-sb.txt")), os.path.exists(r2.path("ok-allow.txt"))),
        bool(pre2) and not os.path.exists(r2.path("deny-sb.txt")) and os.path.exists(r2.path("ok-allow.txt")),
    )


def main():
    only = None
    if "--only" in sys.argv:
        only = sys.argv[sys.argv.index("--only") + 1].split(",")
    os.makedirs(ROOT, exist_ok=True)
    ver = subprocess.check_output(["agy", "--version"], text=True).strip()
    cid = None
    steps = [
        ("basic", None),
        ("resume", None),
        ("bg", bg),
        ("rewrite", rewrite),
        ("failmodes", failmodes),
        ("missing", missing),
        ("subagent", subagent),
        ("modes", modes),
    ]
    for name, fn in steps:
        if only and name not in only:
            continue
        sys.stderr.write("[agy-probe] %s\n" % name)
        sys.stderr.flush()
        try:
            if name == "basic":
                _, cid = basic()
            elif name == "resume":
                if not cid:
                    ev = AgyRun.__new__(AgyRun)
                    ev.log = os.path.join(ROOT, "basic", "log.jsonl")
                    try:
                        cid = hook_events_only(ev)[0]["input"]["conversationId"]
                    except Exception:
                        continue
                resume(cid)
            else:
                fn()
        except Exception as e:
            import traceback
            traceback.print_exc()
            T.add(name + " (runner error)", "-", repr(e), False)

    print("AGY %s (model %s), %s\n" % (ver, MODEL, time.strftime("%Y-%m-%d")))
    print(T.render())
    ex_path = os.path.join(ROOT, "examples.json")
    json.dump(EXAMPLES, open(ex_path, "w"), indent=2)
    sys.stderr.write("examples -> %s\n" % ex_path)


if __name__ == "__main__":
    main()
