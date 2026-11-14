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

        def alpha():
            return next(s for s in json.loads(run("ls", "--json", env=env).stdout) if s["name"] == "alpha")

        def until(condition, what):
            for _ in range(80):
                if condition():
                    return
                drain(master)
                time.sleep(0.05)
            raise AssertionError(what)

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
        # Enter opens the session in the whole window, under a header line that says this is dpty. The browser stays
        # in charge: Ctrl-] or a click on the header goes back to the list.
        original_size = (alpha()["cols"], alpha()["rows"])
        send(master, b"\r")
        until(lambda: alpha()["attached"] == 1, "Enter did not open the session")
        assert (alpha()["cols"], alpha()["rows"]) == (cols, rows - 1), (alpha()["cols"], alpha()["rows"])
        assert browser.poll() is None
        send(master, b"hello\r")
        read_until(master, b"ECHO:hello")
        os.write(master, b"\x1d")
        until(lambda: alpha()["attached"] == 0, "Ctrl-] did not go back to the list")
        until(lambda: (alpha()["cols"], alpha()["rows"]) == original_size, "going back did not give the session its size back")
        send(master, b"\r")
        until(lambda: alpha()["attached"] == 1, "could not reopen the session")
        send(master, b"\x1b[<0;3;1M\x1b[<0;3;1m")  # a click on the header line
        until(lambda: alpha()["attached"] == 0, "clicking the header did not go back to the list")
        assert browser.poll() is None, "going back to the list ended the browser"

        # The side panel as a terminal: Tab gives it the keyboard, the session is resized to the panel, typing reaches the
        # program, and Ctrl-\\ gives the keyboard back to the list.
        drain(master)
        size_before = (alpha()["cols"], alpha()["rows"])
        send(master, b"\t")
        until(lambda: alpha()["attached"] == 1, "Tab did not connect the side panel to the session")
        list_width = round(cols * 0.4)
        assert (alpha()["cols"], alpha()["rows"]) == (cols - list_width - 2, rows - 2 - 6 - 2), (alpha()["cols"], alpha()["rows"])
        send(master, b"panel\r")
        read_until(master, b"ECHO:panel")
        assert alpha()["attached"] == 1, "typing in the panel left it"
        send(master, b"\x1b[<0;%d;20M\x1b[<0;%d;20m" % (list_width + 10, list_width + 10))  # a click inside the panel stays in it
        send(master, b"again\r")
        read_until(master, b"ECHO:again")
        os.write(master, b"\x1c")
        until(lambda: alpha()["attached"] == 0, "Ctrl-\\ did not leave the side panel")
        until(lambda: (alpha()["cols"], alpha()["rows"]) == size_before, "leaving the panel did not give the session its size back")
        assert browser.poll() is None, "leaving the panel ended the browser"
        send(master, b"\t")
        until(lambda: alpha()["attached"] == 1, "could not re-enter the side panel")
        send(master, b"\x1b[<0;5;5M\x1b[<0;5;5m")  # a click on the list leaves it
        until(lambda: alpha()["attached"] == 0, "clicking outside did not leave the side panel")

        # Dragging the line between the list and the right-hand side resizes them, and the sizes are remembered.
        send(master, b"\x1b[<0;%d;6M" % list_width)
        send(master, b"\x1b[<32;%d;6M" % (list_width + 12))
        send(master, b"\x1b[<0;%d;6m" % (list_width + 12))
        saved = pathlib.Path(home) / "browser.json"
        until(saved.exists, "dragging a divider did not save the layout")
        assert abs(json.loads(saved.read_text())["list"] - (list_width + 12) / cols) < 0.01, saved.read_text()
        drain(master)

        # Links and images. This script stands in for the terminal: it says it can draw images and how big a cell is,
        # then checks what the browser writes to it when a session prints a hyperlink and an image.
        def collect(seconds):
            got = bytearray()
            deadline = time.monotonic() + seconds
            while time.monotonic() < deadline:
                if select.select([master], [], [], 0.05)[0]:
                    got.extend(os.read(master, 65536))
            return bytes(got)

        run("new", "-d", "-n", "rich", "--", "sh", env=env)
        # Printed before the session is opened: links in recent output come back when it is.
        run("send", "rich", "printf '\\033]8;;https://example.com/early\\033\\\\EARLIER\\033]8;;\\033\\\\\\n'", env=env)
        run("wait", "rich", "EARLIER", "--timeout", "5", env=env)
        rich = lambda: next(s for s in json.loads(run("ls", "--json", env=env).stdout) if s["name"] == "rich")
        send(master, b"/\x15rich")
        send(master, b"\r")
        send(master, b"\r")
        asked = collect(1.2)
        assert rich()["attached"] == 1, "could not open the session for the link and image check"
        assert b"\x1b[16t" in asked and b"a=q" in asked, "the terminal was not asked about cell size and images"
        assert b"\x1b]8;;https://example.com/early\x1b\\" in asked, "a link printed before opening the session was not restored: %r" % asked[-800:]
        send(master, b"\x1b[6;18;9t\x1b_Gi=31;OK\x1b\\\\")
        send(master, b"printf '\\033]8;;https://example.com/docs\\033\\\\LINKTEXT\\033]8;;\\033\\\\\\n'\r")
        linked = collect(1.0)
        assert b"\x1b]8;;https://example.com/docs\x1b\\" in linked, "the link was not written to the terminal as a link: %r" % linked[-600:]
        assert b"\x1b]8;;\x1b\\" in linked, "the link was never closed"
        import base64
        png = base64.b64encode(b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR" + (90).to_bytes(4, "big") + (36).to_bytes(4, "big") + bytes([8, 6, 0, 0, 0, 0, 0, 0, 0]))
        send(master, b"printf '\\033_Ga=T,f=100,q=2;" + png + b"\\033\\\\'\r")
        pictured = collect(1.0)
        assert b"\x1b_Ga=t,i=" in pictured and b",f=100;" + png in pictured, "the image was not passed to the terminal: %r" % pictured[-600:]
        assert b"a=p,U=1,i=" in pictured and b"c=10,r=2" in pictured, "the image was not placed at 10x2 cells: %r" % pictured[-600:]
        assert "\U0010EEEE".encode() in pictured, "the image's cells were not drawn"
        os.write(master, b"\x1d")
        until(lambda: rich()["attached"] == 0, "could not leave the session")
        freed = collect(0.5)
        assert b"a=d,d=I,i=" in (pictured + freed), "the image was not freed when the view closed"
        run("rm", "rich", env=env)
        send(master, b"\x1b")
        send(master, b"/\x15alpha")
        send(master, b"\r")
        drain(master)

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
        read_until(master, b"NEWBORN")
        newborn = lambda: next(s for s in json.loads(run("ls", "--json", env=env).stdout) if "NEWBORN" in run("screen", s["id"], env=env).stdout)
        until(lambda: newborn()["attached"] == 1, "the new session was not opened")
        os.write(master, b"\x1b[93;5u")  # Ctrl-] as a terminal in an extended keyboard mode sends it
        until(lambda: newborn()["attached"] == 0, "the extended-keyboard form of Ctrl-] did not go back")

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
        until(lambda: alpha()["status"] == "running" and alpha()["attached"] == 1, "r did not restart and open the session")
        send(master, b"\x1c")  # Ctrl-\ still goes back when nothing outside takes it first
        until(lambda: alpha()["attached"] == 0, "Ctrl-\\ did not go back to the list")
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
        print("browser preview, full-window view with header, links and images, panel as terminal, mouse resize, content search, attach/detach, new/rename/delete, saved output, confirmed stop/restart and terminal restore OK")
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
