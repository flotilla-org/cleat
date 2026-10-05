#!/usr/bin/env python3
"""Count OSC 133 marks in decoded output, including fragmented PTY writes.

Run beside the preserved casts: python3 scan-casts.py
"""
import json
import pathlib
import re

pattern = re.compile(r"\x1b\]133;([^\x07\x1b]*)(?:\x07|\x1b\\)")
for path in sorted(pathlib.Path(__file__).parent.glob("*.cast")):
    events = [json.loads(line) for line in path.read_text().splitlines()[1:]]
    output = "".join(event[2] for event in events if event[1] == "o")
    print(path.name, [mark.group(1) for mark in pattern.finditer(output)])
