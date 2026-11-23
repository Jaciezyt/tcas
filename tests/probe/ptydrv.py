"""Minimal pty driver for an interactive claude session (stdlib only)."""
import os, pty, re, select, signal, struct, fcntl, termios, time, subprocess

ANSI = re.compile(r"\x1b\[[0-9;?<>=]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(\x07|\x1b\\)|\x1b[()][A-Z0-9]|\x1b[=>78]")


class Pty:
    def __init__(self, argv, cwd, env, rows=50, cols=160):
        self.buf = ""
        self.raw_total = 0
        m, s = pty.openpty()
        fcntl.ioctl(s, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        env = dict(env, TERM="xterm-256color", COLUMNS=str(cols), LINES=str(rows))
        self.p = subprocess.Popen(argv, cwd=cwd, env=env, stdin=s, stdout=s, stderr=s, start_new_session=True, close_fds=True)
        os.close(s)
        self.m = m
        self.pid = self.p.pid

    def pump(self, secs=0.3):
        end = time.time() + secs
        while True:
            left = end - time.time()
            if left <= 0:
                break
            r, _, _ = select.select([self.m], [], [], left)
            if not r:
                break
            try:
                d = os.read(self.m, 65536)
            except OSError:
                break
            if not d:
                break
            self.raw_total += len(d)
            self.buf += ANSI.sub(lambda m: " " if m.group(0).endswith(("C", "H", "G", "d")) and m.group(0).startswith("\x1b[") else "", d.decode("utf8", "replace"))
            self.buf = self.buf[-60000:]

    def send(self, text):
        os.write(self.m, text.encode())

    def wait_for(self, pat, timeout=30, since=None):
        """wait until regex pat appears in buffer (after position since)."""
        end = time.time() + timeout
        while time.time() < end:
            self.pump(0.3)
            seg = self.buf if since is None else self.buf[since:]
            if re.search(pat, seg):
                return True
            if self.p.poll() is not None:
                self.pump(0.2)
                break
        return False

    def alive(self):
        return self.p.poll() is None

    def kill(self):
        try:
            os.killpg(self.pid, signal.SIGTERM)
        except Exception:
            pass
        t = time.time() + 3
        while self.p.poll() is None and time.time() < t:
            time.sleep(0.1)
        try:
            os.killpg(self.pid, signal.SIGKILL)
        except Exception:
            pass
        try:
            self.p.wait(timeout=3)
        except Exception:
            pass
        try:
            os.close(self.m)
        except Exception:
            pass
