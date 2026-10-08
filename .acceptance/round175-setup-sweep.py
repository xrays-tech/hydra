#!/usr/bin/env python3
"""Round-175 sweep: setup writes that are only STATUS-checked, with no READ-BACK before the legs that
depend on them.

Rounds 171/172 fixed two instances of this shape by hand (`test_snapshot_stale.py` S1's premise,
`test_model_catalog.py` C6's sub-tenant precondition). The mechanical rule here:

  For every `must(...)` call (and every inline `if st not in (200, 201)` guard) in a drill, look ahead
  30 lines for a READ-BACK of running state — an admin/call `GET`, a `sql(` query, or a `metric(`
  read. Flag the ones with none.

Crude on purpose (a `must` for a write whose effect is asserted behaviourally later needs no read-back);
every hit must be read.
"""
import pathlib
import re
import sys

MUST = re.compile(r'\bmust\(\s*"([^"]{6,})"')
INLINE = re.compile(r"if st\w* not in \(200, 201\)")
READBACK = re.compile(r'(admin\(\s*"GET"|call\(\s*"GET"|\bsql\(|\bmetric\(|\bread_back|\bGET", )')


def main():
    hits = []
    total = 0
    for p in sorted(pathlib.Path("integration").glob("*.py")):
        lines = p.read_text(encoding="utf-8", errors="replace").split("\n")
        for i, line in enumerate(lines):
            m = MUST.search(line) or INLINE.search(line)
            if not m:
                continue
            total += 1
            window = "\n".join(lines[i:i + 30])
            if READBACK.search(window):
                continue
            label = m.group(1) if m.re.groups else line.strip()[:70]
            hits.append((p.name, i + 1, label[:90]))
    print(f"{total} status-checked setup write(s); {len(hits)} with NO read-back within 30 lines\n")
    for name, line, label in hits:
        print(f"{name}:{line}  {label}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
