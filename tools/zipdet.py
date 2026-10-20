#!/usr/bin/env python3
"""Deterministic zip: sorted entries, fixed timestamps (SOURCE_DATE_EPOCH),
fixed permissions, so the same inputs give byte-identical archives.

Usage: zipdet.py <out.zip> <root-dir> <prefix-in-archive>
"""
import os, sys, time, zipfile

out, root, prefix = sys.argv[1], sys.argv[2], sys.argv[3].strip("/")
epoch = int(os.environ.get("SOURCE_DATE_EPOCH", "315532800"))
stamp = time.gmtime(max(epoch, 315532800))[:6]
entries = []
for d, _, files in os.walk(root):
    for f in files:
        full = os.path.join(d, f)
        entries.append((os.path.relpath(full, root).replace(os.sep, "/"), full))
with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED, compresslevel=9) as z:
    for rel, full in sorted(entries):
        info = zipfile.ZipInfo(f"{prefix}/{rel}", date_time=stamp)
        info.external_attr = 0o644 << 16
        info.compress_type = zipfile.ZIP_DEFLATED
        info.create_system = 0
        with open(full, "rb") as fh:
            z.writestr(info, fh.read())
