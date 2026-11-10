"""Exercise the browser through a real PTY using an isolated dpty daemon."""

import fcntl
import json
import os
import pathlib
import pty
import select
import struct
import subprocess
import tempfile
import termios
import time

binary = os.environ.get("DPTY_TEST_BIN", str(pathlib.Path(__file__).resolve().parent.parent / "target/debug/dpty"))
command = os.environ.get("DPTY_TEST_COMMAND", "browse")
browser_args = [command] if command else []


def run(*args, env):
    return subprocess.run([binary, *args], env=env, check=True, capture_output=True, text=True)


def read_until(master, needle, timeout=8):
    output = bytearray()
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        ready, _, _ = select.select([master], [], [], 0.2)
        if ready:
            output.extend(os.read(master, 65536))
            if b"\x1b[6n" in output:
                os.write(master, b"\x1b[1;1R")
                output = output.replace(b"\x1b[6n", b"")
            if needle in output:
                return bytes(output)
    raise AssertionError(f"did not see {needle!r}: {bytes(output)[-4000:]!r}")


def read_until_after(master, first, then, timeout=8):
    """Waits for `then` to show up after `first`: the preview pane shows session text too, so text alone does not prove an attach."""
    output = bytearray()
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        ready, _, _ = select.select([master], [], [], 0.2)
        if ready:
            output.extend(os.read(master, 65536))
            if b"\x1b[6n" in output:
                os.write(master, b"\x1b[1;1R")
                output = output.replace(b"\x1b[6n", b"")
            at = output.find(first)
            if at >= 0 and then in output[at:]:
                return bytes(output)
    raise AssertionError(f"did not see {then!r} after {first!r}: {bytes(output)[-3000:]!r}")


LEFT_BROWSER = b"\x1b[?1049l"
TERMINAL_RESET = b"\x1b[?2004l\x1b[?1l\x1b>\x1b[<u"


def send(master, data):
    os.write(master, data)
    time.sleep(0.25)


def drain(master):
    while select.select([master], [], [], 0.05)[0]:
        data = os.read(master, 65536)
        if b"\x1b[6n" in data:
            os.write(master, b"\x1b[1;1R")


def wait_for_exit(process, master, timeout=8):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if process.poll() is not None:
            return process.returncode
        if select.select([master], [], [], 0.1)[0]:
            data = os.read(master, 65536)
            if b"\x1b[6n" in data:
                os.write(master, b"\x1b[1;1R")
    raise AssertionError("browser did not exit")


with tempfile.TemporaryDirectory(prefix="dpty-browse-test-") as home:
    env = dict(os.environ, DPTY_HOME=home, TERM="xterm-256color")
    daemon = subprocess.Popen([binary, "serve", "--no-http"], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    browser = None
    master = slave = None
    try:
        for _ in range(80):
            if subprocess.run([binary, "status"], env=env, capture_output=True).returncode == 0:
                break
            time.sleep(0.05)
        else:
            raise AssertionError("isolated daemon did not start")

        run("new", "-d", "-n", "alpha", "--", "sh", "-c",
            'echo READY_ALPHA; while IFS= read -r line; do echo "ECHO:$line"; done', env=env)
        run("new", "-d", "-n", "beta", "--", "sh", "-c", "echo EXITED_MARKER", env=env)
        for _ in range(80):
            sessions = json.loads(run("ls", "--json", env=env).stdout)
            if any(s["name"] == "beta" and s["status"] == "exited" for s in sessions):
                break
            time.sleep(0.05)
        else:
            raise AssertionError("beta did not exit")

        no_tty = subprocess.run([binary, *browser_args], env=env, capture_output=True, text=True)
        assert no_tty.returncode != 0 and "interactive terminal" in no_tty.stderr

        master, slave = pty.openpty()
        rows = int(os.environ.get("DPTY_TEST_ROWS", "32"))
        cols = int(os.environ.get("DPTY_TEST_COLS", "120"))
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        original_mode = termios.tcgetattr(slave)
        browser = subprocess.Popen([binary, *browser_args], stdin=slave, stdout=slave, stderr=slave, env=env, close_fds=True)
        read_until(master, b"alpha")
        send(master, b"/alpha")
        send(master, b"\r")
        # The preview pane shows the selected session's screen without attaching to it.
        read_until(master, b"READY_ALPHA")
        assert next(s for s in json.loads(run("ls", "--json", env=env).stdout) if s["name"] == "alpha")["attached"] == 0
        send(master, b"\r")
        read_until_after(master, LEFT_BROWSER, b"READY_ALPHA")
        send(master, b"hello\r")
        read_until(master, b"ECHO:hello")
        os.write(master, b"\x1c")
        detached = read_until(master, b"Search:")
        assert TERMINAL_RESET in detached, "detach did not put the terminal modes back"

        # Search reaches into what a session printed, not just its name.
        send(master, b"/\x15ECHO:hel")
        read_until(master, b"output: ECHO:hello")
        send(master, b"\r")

        # Rename, then a new session started from the browser.
        send(master, b"/\x15beta")
        send(master, b"\r")
        send(master, b"R")
        send(master, b"-old\r")
        for _ in range(60):
            if any(s["name"] == "beta-old" for s in json.loads(run("ls", "--json", env=env).stdout)):
                break
            drain(master)  # an unread terminal fills up and stalls the browser
            time.sleep(0.05)
        else:
            raise AssertionError("rename did not reach the session")
        send(master, b"\x1b")
        send(master, b"n")
        send(master, b"sh -c 'echo NEW''BORN; sleep 60'\r")
        read_until_after(master, LEFT_BROWSER, b"NEWBORN")
        os.write(master, b"\x1b[92;5u")  # the detach key as a terminal in an extended keyboard mode sends it
        read_until(master, b"sessions")

        current = json.loads(run("ls", "--json", env=env).stdout)
        assert any(s["name"] == "beta-old" for s in current), [(s["name"], s["status"]) for s in current]
        send(master, b"/\x15beta")
        send(master, b"\r")
        send(master, b"\r")
        read_until(master, b"EXITED_MARKER")
        send(master, b"q")
        send(master, b"/\x15alpha")
        send(master, b"\r")
        send(master, b"s")
        read_until(master, b"Stop")
        send(master, b"n")
        sessions = json.loads(run("ls", "--json", env=env).stdout)
        assert next(s for s in sessions if s["name"] == "alpha")["status"] == "running"
        send(master, b"s")
        read_until(master, b"Stop")
        send(master, b"y")
        for _ in range(80):
            sessions = json.loads(run("ls", "--json", env=env).stdout)
            if next(s for s in sessions if s["name"] == "alpha")["status"] == "exited":
                break
            drain(master)
            time.sleep(0.05)
        else:
            raise AssertionError("confirmed stop did not end alpha")
        assert "ECHO:hello" in run("logs", "alpha", env=env).stdout
        time.sleep(3.5)
        drain(master)
        send(master, b"r")
        read_until_after(master, LEFT_BROWSER, b"READY_ALPHA")
        drain(master)
        send(master, b"\x1c")
        returned = read_until(master, b"\x1b[?1049h")
        if b"Search:" not in returned:
            read_until(master, b"Search:")
        # Delete asks first, then removes the session and its history.
        send(master, b"/\x15beta-old")
        send(master, b"\r")
        send(master, b"d")
        read_until(master, b"Delete")
        send(master, b"y")
        for _ in range(60):
            if not any(s["name"] == "beta-old" for s in json.loads(run("ls", "--json", env=env).stdout)):
                break
            drain(master)
            time.sleep(0.05)
        else:
            raise AssertionError("confirmed delete did not remove the session")
        send(master, b"q")
        assert wait_for_exit(browser, master) == 0
        assert termios.tcgetattr(slave) == original_mode, "terminal mode was not restored"
        sessions = json.loads(run("ls", "--json", env=env).stdout)
        assert next(s for s in sessions if s["name"] == "alpha")["status"] == "running"
        assert "ECHO:hello" in run("screen", "alpha", env=env).stdout
        print("browser preview, content search, attach/detach, new/rename/delete, saved output, confirmed stop/restart and terminal restore OK")
    finally:
        if browser and browser.poll() is None:
            browser.terminate()
            browser.wait(timeout=5)
        subprocess.run([binary, "stop", "--sessions"], env=env, capture_output=True, timeout=8)
        try:
            daemon.wait(timeout=8)
        except subprocess.TimeoutExpired:
            daemon.terminate()
            daemon.wait(timeout=5)
        if master is not None:
            os.close(master)
        if slave is not None:
            os.close(slave)
