"""Bounded fake-daemon regressions for wrapper scheduling and signal races."""

import json
import os
import signal
import socket
import subprocess
import sys
import tempfile
import time


def timed_out(_signal, _frame):
    raise TimeoutError("runtime lifecycle regression exceeded 20 seconds")


signal.signal(signal.SIGALRM, timed_out)
signal.alarm(20)
binary, case = sys.argv[1:]


def until(what, predicate):
    deadline = time.monotonic() + 4
    while time.monotonic() < deadline:
        result = predicate()
        if result:
            return result
        time.sleep(0.02)
    raise AssertionError("timed out waiting for " + what)


with tempfile.TemporaryDirectory(prefix="hr-runtime-") as home:
    listener = socket.socket(socket.AF_UNIX)
    listener.bind(home + "/headroomd.sock")
    listener.listen()
    listener.settimeout(4)
    marker = home + "/started"
    # A retry exits cleanly, while the initial attempt waits to be killed.
    child = (
        "import os,time; p=" + repr(marker) + "; "
        "again=os.path.exists(p); open(p,'w').write(str(os.getpid())); "
        "time.sleep(0 if again else 30)"
    )
    wrapper = subprocess.Popen(
        [binary, "run", "--", sys.executable, "-c", child],
        env={**os.environ, "HEADROOM_HOME": home},
        start_new_session=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    groups = {wrapper.pid}
    conn = None
    try:
        conn, _ = listener.accept()
        conn.settimeout(4)
        reader = conn.makefile("r")

        def receive(kind):
            message = json.loads(reader.readline())
            assert message["t"] == kind, message
            return message

        def send(message):
            conn.sendall(json.dumps(message).encode() + b"\n")

        receive("enqueue")
        if case in ("retry", "retry-legacy"):
            send({"t": "granted", "id": 1, "retry": 0, "low": False})
            pgid = receive("started")["pgid"]
            groups.add(pgid)
            until("child readiness", lambda: os.path.exists(marker))
            os.kill(wrapper.pid, signal.SIGSTOP)
            until("stopped wrapper", lambda: "T" in subprocess.check_output(
                ["/bin/ps", "-o", "stat=", "-p", str(wrapper.pid)], text=True))
            send({"t": "frozen", "by": "guard", "why": "memory pressure"})
            send({"t": "killed", "why": "memory pressure", "requeued": True})
            os.killpg(pgid, signal.SIGTERM)
            # The child must be waitable before any wrapper thread can read.
            until("child exit", lambda: not subprocess.check_output(
                ["/bin/ps", "-o", "stat=", "-p", str(pgid)], text=True).strip()
                or "Z" in subprocess.check_output(
                    ["/bin/ps", "-o", "stat=", "-p", str(pgid)], text=True))
            os.kill(wrapper.pid, signal.SIGCONT)
            receive("exited")
            # Synchronization must wait for this acknowledgment, not merely
            # drain whatever happened to reach the reader thread already.
            time.sleep(0.1)
            assert wrapper.poll() is None, "wrapper exited before disposition acknowledgment"
            if case == "retry":
                send({"t": "ok"})
            else:
                send({"t": "queued", "id": 1, "pos": 1, "of": 1,
                      "estimate": 1, "samples": 0, "reason": "retry"})
            send({"t": "granted", "id": 1, "retry": 1, "low": False})
            groups.add(receive("started")["pgid"])
            receive("exited")
            send({"t": "ok"})
            out, err = wrapper.communicate(timeout=4)
            assert wrapper.returncode == 0, (wrapper.returncode, out, err)
            assert "requeued, will run again" in err, err
        elif case in ("term", "int", "hup", "error", "inside"):
            if case in ("error", "inside"):
                send({"t": "error", "msg": "unavailable"} if case == "error"
                     else {"t": "inside", "id": 1})
            else:
                send({"t": "queued", "id": 1, "pos": 1, "of": 1,
                      "estimate": 1, "samples": 0, "reason": "held"})
                time.sleep(0.1)
                conn.shutdown(socket.SHUT_RDWR)
            until("fallback command", lambda: os.path.exists(marker))
            pgid = int(open(marker).read())
            groups.add(pgid)
            sig = {"int": signal.SIGINT, "hup": signal.SIGHUP}.get(case, signal.SIGTERM)
            os.kill(wrapper.pid, sig)
            out, err = wrapper.communicate(timeout=4)
            assert wrapper.returncode == 128 + sig, (wrapper.returncode, out, err)
        elif case == "legacy":
            send({"t": "granted", "id": 1, "retry": 0, "low": False})
            pgid = receive("started")["pgid"]
            groups.add(pgid)
            until("child readiness", lambda: os.path.exists(marker))
            os.killpg(pgid, signal.SIGTERM)
            receive("exited")
            # A daemon predating the acknowledgment leaves the connection open.
            out, err = wrapper.communicate(timeout=3)
            assert wrapper.returncode == 143, (wrapper.returncode, out, err)
        else:
            raise AssertionError(case)
    finally:
        for pgid in groups:
            try:
                os.killpg(pgid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        wrapper.wait(timeout=4)
        if conn:
            conn.close()
        listener.close()
