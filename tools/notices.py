#!/usr/bin/env python3
"""Writes THIRD-PARTY-NOTICES.txt for the shipped Windows binaries: every
crate linked into flint.exe and flint-preview-host.exe, with its license
expression and the license/notice files it ships.

Usage: python3 tools/notices.py > THIRD-PARTY-NOTICES.txt
"""
import json, os, re, subprocess, sys

TARGET = "x86_64-pc-windows-gnu"
tree = subprocess.check_output(["cargo", "tree", "--locked", "--target", TARGET, "-e", "normal", "--prefix", "none", "-f", "{p}",
                                "-p", "flint-ui-win32", "-p", "flint-preview"], text=True)
shipped = set()
for line in tree.splitlines():
    m = re.match(r"^(\S+) v(\S+)", line.strip())
    if m:
        shipped.add((m.group(1), m.group(2)))
meta = json.loads(subprocess.check_output(["cargo", "metadata", "--locked", "--format-version", "1", "--filter-platform", TARGET], text=True))
out = ["Third-party software in Flint", "=" * 29, "",
       "Flint is licensed under MIT OR Apache-2.0 (LICENSE-MIT.txt, LICENSE-APACHE.txt).",
       "It includes the following open-source components, linked into flint.exe and",
       "flint-preview-host.exe. SQLite is compiled in from libsqlite3-sys (public domain).", ""]
pkgs = sorted((p for p in meta["packages"] if p["source"] and (p["name"], p["version"]) in shipped), key=lambda p: (p["name"], p["version"]))
for p in pkgs:
    out.append("-" * 78)
    out.append(f"{p['name']} {p['version']}  —  {p.get('license') or 'see files below'}")
    if p.get("repository"):
        out.append(p["repository"])
    d = os.path.dirname(p["manifest_path"])
    files = sorted(f for f in os.listdir(d) if re.match(r"^(LICEN[CS]E|COPYING|NOTICE|COPYRIGHT|UNLICENSE)", f, re.I) and os.path.isfile(os.path.join(d, f)))
    for f in files:
        text = open(os.path.join(d, f), encoding="utf-8", errors="replace").read().strip()
        out.append(f"\n[{f}]\n{text}")
    if not files:
        out.append("(no license file in the package; license expression above)")
    out.append("")
sys.stdout.write("\n".join(out) + "\n")
print(f"{len(pkgs)} components", file=sys.stderr)
