import json, os, subprocess, time

def log(rec):
    p = os.environ.get("PROBE_LOG")
    if p:
        with open(p, "a") as f:
            f.write(json.dumps(rec) + "\n")

def ancestors():
    chain, pid = [], os.getpid()
    while pid > 1 and len(chain) < 40:
        try:
            ppid = int(subprocess.check_output(["ps", "-o", "ppid=", "-p", str(pid)]).strip())
        except Exception:
            break
        chain.append(ppid)
        pid = ppid
    return chain
