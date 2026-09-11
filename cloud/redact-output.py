#!/usr/bin/env python3
"""Remove this node's configured credentials before runbook output leaves the node."""
import pathlib
import shlex
import sys
import urllib.parse

values = set()
for line in pathlib.Path('/opt/broccoli/.env').read_text().splitlines():
    if '=' not in line or line.lstrip().startswith('#'):
        continue
    name, raw = line.split('=', 1)
    parts = shlex.split(raw)
    if not parts:
        continue
    value = parts[0]
    if any(marker in name.lower() for marker in ('password', 'secret', 'access_key', 'token')):
        if len(value) >= 8:
            values.add(value)
    if 'url' in name.lower():
        password = urllib.parse.urlsplit(value).password
        if password:
            values.add(password)
            values.add(urllib.parse.unquote(password))
text = sys.stdin.read()
for value in sorted(values, key=len, reverse=True):
    text = text.replace(value, '[REDACTED]')
sys.stdout.write(text)
