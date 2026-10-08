#!/usr/bin/env python3
"""Round-174 sweep: legs whose LABEL says "counted / measured / attributed / incremented" while the
predicate only asks whether something EXISTS (`any(...)`, `x in y`, `is not None`).

Same family as round 173's C2 fix (`any('provider="p1"' in l ...)` labelled "IS counted"): an existence
test cannot show that THIS event produced the number, and it cannot fail when the number was already
there. Crude on purpose — every hit must be read.
"""
import pathlib
import re
import sys

CLAIM = re.compile(r"\b(counted|counts|count|measured|attributed|increment|increase|increases|"
                   r"observed|recorded|records|metered|emitted)\b", re.I)
DISCRIMINATING = re.compile(r"(==|!=|<=|>=|<|>)|len\s*\(")


def balanced(text, start):
    depth = 0
    i = start
    quote = None
    while i < len(text):
        c = text[i]
        if quote:
            if c == "\\":
                i += 2
                continue
            if text.startswith(quote, i):
                i += len(quote)
                quote = None
                continue
            i += 1
            continue
        if c in "\"'":
            quote = c * 3 if text.startswith(c * 3, i) else c
            i += len(quote)
            continue
        if c == "(":
            depth += 1
        elif c == ")":
            depth -= 1
            if depth == 0:
                return text[start:i + 1]
        i += 1
    return text[start:]


def split_top(body):
    depth = 0
    quote = None
    for i, c in enumerate(body):
        if quote:
            if c == "\\":
                continue
            if body.startswith(quote, i):
                quote = None
            continue
        if c in "\"'":
            quote = c * 3 if body.startswith(c * 3, i) else c
            continue
        if c in "([{":
            depth += 1
        elif c in ")]}":
            depth -= 1
        elif c == "," and depth == 0:
            return body[:i], body[i + 1:]
    return body, ""


def main():
    hits = []
    for p in sorted(pathlib.Path("integration").glob("*.py")):
        text = p.read_text(encoding="utf-8", errors="replace")
        for m in re.finditer(r"\bcheck\(", text):
            body = balanced(text, m.start() + 5)[1:-1]
            label, pred = split_top(body)
            lab = " ".join(re.findall(r'"([^"]{6,})"', label))
            if not lab or not CLAIM.search(lab):
                continue
            first, _ = split_top(pred)
            if DISCRIMINATING.search(first):
                continue
            if not re.search(r"any\s*\(|\bin\b|is not None", first):
                continue
            line = text[:m.start()].count("\n") + 1
            hits.append((p.name, line, lab[:100], " ".join(first.split())[:90]))
    print(f"FLAGGED {len(hits)} existence-shaped 'counted/measured' leg(s)\n")
    for name, line, lab, pred in hits:
        print(f"{name}:{line}\n   label: {lab}\n   pred : {pred}\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
