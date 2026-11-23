"""Shared helpers for the probe scripts (stdlib only)."""
import json, os, shutil, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
PLUGIN = os.path.join(HERE, "plugin")
ROOT = os.environ.get("PROBE_TMP", "/tmp/tcas-probe")
MODEL = os.environ.get("PROBE_MODEL", "haiku")
ALL_FLAGS = ["envfile", "inject", "allow_edit", "allow_bash", "bg_rewrite", "deny_edit"]


class Run:
    """One temp repo + log + control dir."""

    def __init__(self, name, flags=ALL_FLAGS, files=None, settings=None):
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
        if settings:
            os.makedirs(os.path.join(self.repo, ".claude"), exist_ok=True)
            json.dump(settings, open(os.path.join(self.repo, ".claude", "settings.json"), "w"))
        subprocess.run(["git", "init", "-q"], cwd=self.repo, check=True)
        subprocess.run(["git", "add", "-A"], cwd=self.repo)
        subprocess.run(["git", "-c", "user.name=p", "-c", "user.email=p@p", "commit", "-qm", "init", "--allow-empty"],
                       cwd=self.repo)

    def env(self):
        e = dict(os.environ)
        e.update(PROBE_LOG=self.log, PROBE_CTL=self.ctl)
        return e

    def events(self, *names):
        out = []
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

    def claude_headless(self, prompt, plugin=PLUGIN, max_turns=15, extra=(), timeout=240):
        cmd = ["claude", "-p", prompt, "--model", MODEL, "--permission-mode", "default",
               "--max-turns", str(max_turns), "--plugin-dir", plugin, "--output-format", "json"] + list(extra)
        t0 = time.time()
        try:
            p = subprocess.run(cmd, cwd=self.repo, env=self.env(), capture_output=True, text=True, timeout=timeout)
            out, err = p.stdout, p.stderr
        except subprocess.TimeoutExpired as e:
            out, err = (e.stdout or b"").decode() if isinstance(e.stdout, bytes) else (e.stdout or ""), "TIMEOUT"
        try:
            j = json.loads(out)
        except Exception:
            j = {"result": out, "session_id": None, "_stderr": err}
        j["_elapsed"] = time.time() - t0
        return j


def ev_session(r):
    return (r.get("input") or {}).get("session_id")


def tool_events(run, event, tool):
    return [r for r in run.events(event) if r["input"].get("tool_name") == tool]


def bash_events(run, event, needle):
    return [r for r in run.events(event)
            if r["input"].get("tool_name") == "Bash" and needle in (r["input"].get("tool_input") or {}).get("command", "")]


def trim(o, n=200):
    if isinstance(o, dict):
        return {k: trim(v, n) for k, v in o.items()}
    if isinstance(o, list):
        return [trim(v, n) for v in o[:6]]
    if isinstance(o, str) and len(o) > n:
        return o[:n] + "..."
    return o


class Table:
    def __init__(self):
        self.rows = []

    def add(self, conclusion, expected, observed, ok):
        status = "PASS" if ok is True else ("FAIL" if ok is False else "INFO")
        self.rows.append((conclusion, expected, str(observed).replace("\n", " ").replace("|", "/"), status))

    def render(self):
        lines = ["| conclusion | expected | observed | result |", "|---|---|---|---|"]
        for r in self.rows:
            lines.append("| %s | %s | %s | %s |" % r)
        return "\n".join(lines)
