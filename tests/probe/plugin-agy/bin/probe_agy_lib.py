"""Shared process ancestry & logging helpers for AGY probe scripts (stdlib + ctypes)."""
import ctypes, json, os, subprocess, sys

ENV_KEYS = [
    "PROBE_LOG",
    "PLUGIN_ROOT",
    "PLUGIN_DATA",
    "TCAS_SESSION",
    "CLAUDE_CODE_SESSION_ID",
    "ANTIGRAVITY_CONVERSATION_ID",
    "ANTIGRAVITY_AGENT",
    "ANTIGRAVITY_LS_ADDRESS",
    "ANTIGRAVITY_LS_VERSION",
    "ANTIGRAVITY_PROJECT_ID",
    "ANTIGRAVITY_TRAJECTORY_ID",
    "ANTIGRAVITY_APP_DATA_DIR",
    "ANTIGRAVITY_AGENTAPI_EXE",
]


class _ProcBsdInfo(ctypes.Structure):
    _fields_ = [
        ("pbi_flags", ctypes.c_uint32),
        ("pbi_status", ctypes.c_uint32),
        ("pbi_xstatus", ctypes.c_uint32),
        ("pbi_pid", ctypes.c_uint32),
        ("pbi_ppid", ctypes.c_uint32),
        ("pbi_uid", ctypes.c_uint32),
        ("pbi_gid", ctypes.c_uint32),
        ("pbi_ruid", ctypes.c_uint32),
        ("pbi_rgid", ctypes.c_uint32),
        ("pbi_svuid", ctypes.c_uint32),
        ("pbi_svgid", ctypes.c_uint32),
        ("rfu_1", ctypes.c_uint32),
        ("pbi_comm", ctypes.c_char * 16),
        ("pbi_name", ctypes.c_char * 32),
        ("pbi_nfiles", ctypes.c_uint32),
        ("pbi_pgid", ctypes.c_uint32),
        ("pbi_pjobc", ctypes.c_uint32),
        ("e_tdev", ctypes.c_uint32),
        ("e_tpgid", ctypes.c_uint32),
        ("pbi_nice", ctypes.c_int32),
        ("pbi_start_tvsec", ctypes.c_uint64),
        ("pbi_start_tvusec", ctypes.c_uint64),
    ]


_LIBC = None
if sys.platform == "darwin":
    try:
        _LIBC = ctypes.CDLL("/usr/lib/libSystem.B.dylib")
    except Exception:
        _LIBC = None


def proc_info(pid):
    """Return dict {pid, ppid, started_at, name} or None."""
    if pid <= 0:
        return None
    if _LIBC is not None:
        bi = _ProcBsdInfo()
        size = ctypes.sizeof(_ProcBsdInfo)
        n = _LIBC.proc_pidinfo(int(pid), 3, ctypes.c_uint64(0), ctypes.byref(bi), size)
        if n == size:
            raw_name = bi.pbi_name or bi.pbi_comm
            name = raw_name.decode("utf-8", "replace")
            ms = int(bi.pbi_start_tvsec) * 1000 + int(bi.pbi_start_tvusec) // 1000
            return {"pid": int(pid), "ppid": int(bi.pbi_ppid), "started_at": ms, "name": name}
    try:
        out = subprocess.check_output(["ps", "-o", "ppid=,comm=", "-p", str(pid)], text=True).strip()
        if not out:
            return None
        parts = out.split(None, 1)
        return {"pid": int(pid), "ppid": int(parts[0]), "started_at": 0, "name": parts[1] if len(parts) > 1 else ""}
    except Exception:
        return None


def proc_alive(pid, started_at=None):
    info = proc_info(pid)
    if info is None:
        return False
    if started_at and info["started_at"]:
        return abs(info["started_at"] - started_at) <= 1
    return True


def ancestor_details():
    """Calling process's ancestors (nearest first, stopping before PID 1) as dicts."""
    out = []
    cur = proc_info(os.getpid())
    if not cur:
        return out
    pid = cur["ppid"]
    for _ in range(64):
        if pid <= 1:
            break
        info = proc_info(pid)
        if not info:
            break
        out.append(info)
        pid = info["ppid"]
    return out


def ancestors():
    return [d["pid"] for d in ancestor_details()]


def _cfg():
    here = os.path.dirname(os.path.realpath(__file__))
    p = os.path.join(here, "..", "probe_cfg.json")
    if os.path.exists(p):
        try:
            return json.load(open(p))
        except Exception:
            return {}
    return {}


def ctl_dir():
    return os.environ.get("PROBE_CTL") or _cfg().get("ctl") or "/nonexistent"


def log_path():
    return os.environ.get("PROBE_LOG") or _cfg().get("log")


def log(rec):
    p = log_path()
    if p:
        with open(p, "a") as f:
            f.write(json.dumps(rec) + "\n")
