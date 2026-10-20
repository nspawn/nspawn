#!/usr/bin/env python3
"""Runs the cases of tests/compose/cases with an nspawn binary and compares with what
docker compose printed for them, as the conformance test of src/compose does in
process: here it is the binary, so what lies around nspawn compose's code is checked
too: the environment and directory it starts from, stdin, how it prints warnings and
errors, and its exit status.

    binary.py NSPAWN [CASES_DIR]

Output, error lines and status must be docker's. Warnings: docker's must all be
there, in its format; nspawn may print more, the notes of its support table, which
the conformance test checks one by one.
"""

import os
import re
import shutil
import subprocess
import sys
import tempfile


def warnings_and_error(stderr):
    warnings, error = [], []
    for line in stderr.splitlines():
        if line.startswith("time="):
            # The time is the moment of the run; the rest must be docker's.
            warnings.append(re.sub(r'^time="[^"]*" ', "", line))
        elif line.startswith('{"level":"warning"'):
            # The same under --progress json.
            warnings.append(re.sub(r',"time":"[^"]*"}$', "}", line))
        else:
            error.append(line)
    return warnings, error


def unordered(args, text):
    """docker compose prints these in the order of a Go map, which is random."""
    lines = text.split("\n")
    if "--variables" in args and "json" not in args and "yaml" not in args:
        lines[1:] = sorted(lines[1:])
    if any(flag in args for flag in ("--services", "--volumes", "--networks", "--models", "--images")):
        lines = sorted(lines)
    return "\n".join(lines)


def read(path, default=""):
    # docker compose prints bytes that are not UTF-8, and carriage returns, as they
    # are; so must nspawn.
    try:
        with open(path, encoding="utf-8", errors="surrogateescape", newline="") as f:
            return f.read()
    except FileNotFoundError:
        return default


def run(nspawn, case, real, n, args):
    name = os.path.basename(case)
    virtual = f"/compose/{name}"
    env = {"HOME": "/home/user", "PATH": "/usr/local/bin:/usr/bin:/bin"}
    for line in read(os.path.join(case, "env")).splitlines():
        if line and not line.startswith("#") and "=" in line:
            key, value = line.split("=", 1)
            env[key] = value
    stdin = os.path.join(case, "stdin")
    with open(stdin if os.path.exists(stdin) else os.devnull, "rb") as source:
        done = subprocess.run(
            [nspawn, "compose", *args.split()],
            cwd=real,
            env=env,
            stdin=source,
            capture_output=True,
        )
    local = lambda text: text.decode("utf-8", "surrogateescape").replace(real, virtual)
    stdout, stderr = local(done.stdout), local(done.stderr)
    expected = os.path.join(case, "expected")
    want_stdout = read(os.path.join(expected, f"{n}.stdout"))
    want_warnings, want_error = warnings_and_error(read(os.path.join(expected, f"{n}.stderr")))
    want_status = int(read(os.path.join(expected, f"{n}.status"), "0").strip() or 0)
    # Where nspawn refuses on purpose what docker compose takes, the case says so.
    refusal = read(os.path.join(case, f"nspawn-{n}.error"))
    if refusal:
        want_error, want_status, want_stdout = refusal.rstrip("\n").split("\n"), 1, ""
    warnings, error = warnings_and_error(stderr)
    problems = []
    if done.returncode != want_status:
        problems.append(f"status: docker {want_status}, nspawn {done.returncode}")
    if error != want_error:
        problems.append(f"error:\n  docker: {want_error!r}\n  nspawn: {error!r}")
    # What a failed run printed is docker compose's help, which nspawn's is not.
    if want_status == 0 and done.returncode == 0 and unordered(args, stdout) != unordered(args, want_stdout):
        problems.append(f"output:\n  docker: {want_stdout[:400]!r}\n  nspawn: {stdout[:400]!r}")
    missing = [w for w in want_warnings if w not in warnings]
    if missing:
        problems.append(f"warnings docker printed and nspawn did not: {missing!r}")
    return problems


def main():
    if len(sys.argv) not in (2, 3):
        sys.exit(__doc__.strip().split("\n\n")[1])
    nspawn = sys.argv[1]
    root = sys.argv[2] if len(sys.argv) == 3 else os.path.join(os.path.dirname(__file__), "cases")
    failures, count = [], 0
    with tempfile.TemporaryDirectory() as tmp:
        for name in sorted(os.listdir(root)):
            case = os.path.join(root, name)
            if not os.path.isdir(os.path.join(case, "expected")):
                continue
            # A copy, so that nothing written by a run lands in the source.
            real = os.path.join(tmp, name)
            shutil.copytree(case, real, symlinks=True, ignore=shutil.ignore_patterns("expected"))
            runs = [l for l in read(os.path.join(case, "runs"), "config").splitlines() if l.strip()]
            for n, args in enumerate(runs, 1):
                count += 1
                problems = run(nspawn, case, real, n, args)
                if problems:
                    failures.append(f"{name} run {n} ({args}):\n" + "\n".join(problems))
    for failure in failures:
        print(failure + "\n")
    print(f"{count - len(failures)} of {count} runs as docker compose")
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
