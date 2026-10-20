#!/usr/bin/env python3
"""Replaces what the recorded answers tell of the host the oracle ran on (its kernel,
CPUs, memory, names) with fixed values; the shapes are what the tests compare.

    sanitize.py GOLDEN_DIR
"""
import glob
import json
import re
import sys

for path in glob.glob(f"{sys.argv[1]}/*.body"):
    raw = open(path, "rb").read()
    try:
        text = raw.decode()
    except UnicodeDecodeError:
        continue
    new = re.sub(r'"KernelVersion":"[^"]*"', '"KernelVersion":"6.0.0"', text)
    if path.endswith("/info.body"):
        info = json.loads(text)
        info.update({"ID": "00000000-0000-0000-0000-000000000000", "Name": "oracle", "NCPU": 4,
                     "MemTotal": 8589934592, "KernelVersion": "6.0.0", "SystemTime": "2026-10-18T17:21:50Z"})
        new = json.dumps(info, separators=(",", ":")) + "\n"
    if new != text:
        open(path, "w").write(new)
