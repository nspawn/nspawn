#!/usr/bin/python3
# Runs a command on a pseudo terminal the way a person would: what comes on standard
# input is typed only once the command has shown a prompt (a login flushes whatever
# arrived before it was ready), and the exit status is the command's.
import os
import pty
import re
import select
import sys
import time

PROMPT = re.compile(rb"[$#] (\x1b\[[0-9;?]*[a-zA-Z])*\s*$")


def main(argv):
    typed = b"" if sys.stdin.isatty() else sys.stdin.buffer.read()
    pid, master = pty.fork()
    if pid == 0:
        os.execvp(argv[0], argv)
    output = b""
    sent = not typed
    deadline = time.monotonic() + 30
    while True:
        try:
            ready, _, _ = select.select([master], [], [], 0.5)
        except InterruptedError:
            continue
        if ready:
            try:
                chunk = os.read(master, 4096)
            except OSError:
                break
            if not chunk:
                break
            os.write(1, chunk)
            output += chunk
        if not sent and (PROMPT.search(output[-200:]) or time.monotonic() > deadline):
            os.write(master, typed)
            sent = True
    _, status = os.waitpid(pid, 0)
    return os.waitstatus_to_exitcode(status)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
