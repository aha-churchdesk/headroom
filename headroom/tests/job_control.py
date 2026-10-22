"""Real controlling-terminal regression, launched by the Rust e2e fixture."""

import json
import os
import pty
import select
import shlex
import signal
import socket
import subprocess
import sys
import time


def timed_out(_signal, _frame):
    raise TimeoutError("terminal regression exceeded 30 seconds")


signal.signal(signal.SIGALRM, timed_out)
signal.alarm(30)
binary = sys.argv[1]
home = os.environ["HEADROOM_HOME"]
prompt = b"HEADROOM_TEST> "


def request(message):
    with socket.socket(socket.AF_UNIX) as conn:
        conn.settimeout(2)
        conn.connect(home + "/headroomd.sock")
        conn.sendall(json.dumps(message).encode() + b"\n")
        return json.loads(conn.makefile().readline())


def listing():
    return request({"t": "list"})


def until(what, predicate, seconds=5):
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        # Readline can fill the PTY with redisplay output before executing a
        # command, especially with TERM=dumb. Keep draining while polling the
        # daemon, and retain the bytes for the next terminal assertion.
        drain_terminal()
        result = predicate()
        if result:
            return result
        time.sleep(0.03)
    raise AssertionError("timed out waiting for " + what)


pid, terminal = pty.fork()
if pid == 0:
    os.environ["PS1"] = prompt.decode()
    os.execv("/bin/bash", ["bash", "--noprofile", "--norc", "-i"])

groups = set()
pending_terminal = bytearray()


def drain_terminal(timeout=0):
    if terminal is None:
        return
    ready, _, _ = select.select([terminal], [], [], timeout)
    if ready:
        pending_terminal.extend(os.read(terminal, 65536))


def read_for(seconds=0.15):
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        drain_terminal(max(0, end - time.monotonic()))
    output = bytes(pending_terminal)
    pending_terminal.clear()
    return output


def shell_prompt():
    output = b""

    def arrived():
        nonlocal output
        output += read_for(0.05)
        return prompt in output

    until("the shell prompt: " + repr(output), arrived)
    return output


def send(command):
    os.write(terminal, command.encode() + b"\n")


def running_job():
    jobs = listing()["running"]
    return jobs[0] if jobs and jobs[0]["pgid"] else None


def state(expected):
    job = running_job()
    return job and job["state"] == expected and job


def ps(field, process):
    return subprocess.run(["/bin/ps", "-o", field + "=", "-p", str(process)],
                          capture_output=True, text=True, timeout=2).stdout.strip()


try:
    shell_prompt()
    send(shlex.quote(binary) + " run -- /bin/sleep 60")
    job = until("a foreground managed child", running_job)
    group = job["pgid"]
    groups.add(group)
    until("child terminal ownership", lambda: os.tcgetpgrp(terminal) == group)
    read_for()

    # A daemon SIGSTOP must keep the supervisor alive and the shell waiting.
    wrapper = int(ps("ppid", group))
    request({"t": "control", "op": "freeze", "id": job["id"]})
    until("daemon freeze", lambda: state("frozen"))
    assert prompt not in read_for(0.3), "daemon freeze suspended the wrapper"
    stat = ps("stat", wrapper)
    assert not stat.startswith("T"), "daemon freeze stopped the supervisor"
    request({"t": "control", "op": "resume", "id": job["id"]})
    until("daemon resume", lambda: state("running"))

    os.write(terminal, b"\x1a")
    shell_prompt()
    stopped = until("terminal user freeze", lambda: state("frozen"))
    assert stopped["frozen_by"] == "user", stopped
    send("fg")
    until("foreground resume", lambda: state("running"))
    until("foreground terminal restored", lambda: os.tcgetpgrp(terminal) == group)
    read_for()

    os.write(terminal, b"\x1a")
    shell_prompt()
    send("bg")
    shell_prompt()
    until("background resume", lambda: state("running"))
    assert os.tcgetpgrp(terminal) == pid, "bg stole the shell terminal"
    request({"t": "control", "op": "kill", "id": job["id"]})
    until("background job exit", lambda: not listing()["running"])
    assert os.tcgetpgrp(terminal) == pid, "background exit stole the shell terminal"
    send("echo BACKGROUND_EXIT_OK")
    assert b"BACKGROUND_EXIT_OK" in shell_prompt()

    # A background reader must stop on SIGTTIN, then read normally after fg.
    reader = "import os; data=os.read(0,4096); os.write(1,b'READER_OUTPUT:'+data)"
    send(shlex.quote(binary) + " run -- " + shlex.quote(sys.executable) +
         " -c " + shlex.quote(reader) + " &")
    shell_prompt()
    job = until("background reader", running_job)
    group = job["pgid"]
    groups.add(group)
    until("background reader user freeze", lambda: state("frozen"))
    wrapper = int(ps("ppid", group))
    until("background reader supervisor stop", lambda: ps("stat", wrapper).startswith("T"))
    read_for()
    send("fg")
    until("reader foreground resume", lambda: state("running"))
    until("reader terminal ownership", lambda: os.tcgetpgrp(terminal) == group)
    read_for()
    os.write(terminal, b"READER_RESUMED\n")
    assert b"READER_OUTPUT:READER_RESUMED" in shell_prompt()
    until("foreground reader exit", lambda: not listing()["running"])

    # Daemon controls must wake a shell-stopped supervisor so it can reap.
    for resume_first in (True, False):
        send(shlex.quote(binary) + " run -- /bin/sleep 60")
        job = until("a daemon-control child", running_job)
        group = job["pgid"]
        groups.add(group)
        wrapper = int(ps("ppid", group))
        until("daemon-control terminal ownership", lambda: os.tcgetpgrp(terminal) == group)
        read_for()
        os.write(terminal, b"\x1a")
        shell_prompt()
        until("daemon-control terminal pause", lambda: state("frozen"))
        if resume_first:
            request({"t": "control", "op": "resume", "id": job["id"]})
            until("supervisor resumed by daemon", lambda: not ps("stat", wrapper).startswith("T"))
            assert os.tcgetpgrp(terminal) == pid, "daemon resume stole terminal ownership"
        request({"t": "control", "op": "kill", "id": job["id"]})
        until("daemon kill reaped by supervisor", lambda: not listing()["running"])
        until("supervisor exit after daemon kill", lambda: not ps("stat", wrapper))

    # Ctrl-C after a sample also cancels remembered setsid descendants.
    pidfile = home + "/terminal-detached.pid"
    worker = ("import os,signal,time; signal.signal(signal.SIGINT,signal.SIG_IGN); "
              "open(" + repr(pidfile) + ",'w').write(str(os.getpid())); time.sleep(60)")
    command = ("import subprocess,sys,time; subprocess.Popen([sys.executable,'-c'," +
               repr(worker) + "],start_new_session=True); time.sleep(60)")
    send(shlex.quote(binary) + " run -- " + shlex.quote(sys.executable) +
         " -c " + shlex.quote(command))
    job = until("a terminal cancellation job", running_job)
    group = job["pgid"]
    groups.add(group)
    until("a detached terminal worker", lambda: os.path.exists(pidfile))
    with open(pidfile) as ready:
        detached = int(ready.read())
    groups.add(detached)
    time.sleep(1.2)
    read_for()
    os.write(terminal, b"\x1a")
    shell_prompt()
    until("detached worker terminal pause", lambda: ps("stat", detached).startswith("T"))
    send("fg")
    until("terminal worker resumed", lambda: not ps("stat", detached).startswith("T"))
    until("terminal cancellation foreground", lambda: os.tcgetpgrp(terminal) == group)
    read_for()
    os.write(terminal, b"\x03")
    shell_prompt()
    until("terminal cancellation job exit", lambda: not listing()["running"])
    until("detached worker cancellation after grace", lambda: not ps("stat", detached))
except BaseException:
    print("Pending terminal output:", repr(read_for()), file=sys.stderr)
    raise
finally:
    # Keep cleanup bounded even if an assertion fails with a stopped wrapper.
    for job in listing()["running"]:
        if job["pgid"]:
            groups.add(job["pgid"])
        if job.get("pid"):
            try:
                os.kill(job["pid"], signal.SIGKILL)
            except ProcessLookupError:
                pass
    for group in groups:
        try:
            os.killpg(group, signal.SIGKILL)
        except ProcessLookupError:
            pass
    try:
        os.kill(pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    os.close(terminal)
    terminal = None
    until("shell cleanup", lambda: os.waitpid(pid, os.WNOHANG)[0])
