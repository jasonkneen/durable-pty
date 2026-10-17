"""Drives a real `dpty attach` inside a pty, then checks the session saw the input and survives detach."""
import os, pty, subprocess, sys, time, select, tempfile, shutil
home = tempfile.mkdtemp(prefix="dpty-att.")
env = dict(os.environ, DPTY_HOME=home)
binp = os.path.join(os.path.dirname(__file__), "..", "target", "debug", "dpty")
subprocess.run([binp, "new", "-d", "-n", "att", "--", "sh", "-c", "echo ready; while read l; do echo ECHO:$l; done"], env=env, check=True, capture_output=True)
m, s = pty.openpty()
p = subprocess.Popen([binp, "attach", "att"], stdin=s, stdout=s, stderr=s, env=env, close_fds=True)
def read_until(token, secs=5):
    buf = b""; end = time.time() + secs
    while time.time() < end:
        r, _, _ = select.select([m], [], [], 0.2)
        if r:
            buf += os.read(m, 4096)
            if token in buf: return buf
    return buf
out = read_until(b"ready"); assert b"ready" in out, out
os.write(m, b"hello\n")
out = read_until(b"ECHO:hello"); assert b"ECHO:hello" in out, out
os.write(m, b"\x1c")  # Ctrl-\ detaches
p.wait(timeout=5)
screen = subprocess.run([binp, "screen", "att"], env=env, capture_output=True, text=True).stdout
assert "ECHO:hello" in screen, screen
subprocess.run([binp, "stop"], env=env, capture_output=True)
shutil.rmtree(home, ignore_errors=True)
print("attach/detach OK")
