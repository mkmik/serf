#!/usr/bin/env python3
"""Run serf under a real pty and type at it, to reproduce the TTY path."""
import os, pty, select, sys, time

cmd = sys.argv[1:] or ["./target/release/serf", "morphic.snap"]
script = eval(os.environ.get("SERF_SCRIPT", '[(8.0, "2\\n"), (14.0, "3 + 4\\n")]'))

pid, fd = pty.fork()
if pid == 0:
    err = os.environ.get("SERF_ERR_FILE")
    if err:
        fd2 = os.open(err, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
        os.dup2(fd2, 2)
    os.execvp(cmd[0], cmd)

start = time.time()
out = []
i = 0
while time.time() - start < float(os.environ.get('SERF_SECS', '30')):
    now = time.time() - start
    if i < len(script) and now >= script[i][0]:
        os.write(fd, script[i][1].encode())
        i += 1
    r, _, _ = select.select([fd], [], [], 0.2)
    if r:
        try:
            d = os.read(fd, 4096)
        except OSError:
            break
        if not d:
            break
        out.append(d)
os.kill(pid, 9)
sys.stdout.write(b"".join(out).decode("utf-8", "replace"))
