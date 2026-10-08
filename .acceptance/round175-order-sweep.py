#!/usr/bin/env python3
"""Round-175 sweep (dimension a): legs whose LABEL is about TIME/ORDER (first / before / mid-request /
then / subsequently / initial / still) while the predicate compares only FINAL state.

This is the `test_usage_drop_accounting.py` D2 shape fixed in round 168 ("the first reason an operator
sees" was asserted with `any(...)` on the sample taken at the END). A predicate that shows precedence
either looks at an ordered slice (`[:1]`, `[:2]`, `codes[0]`), references a `_before`/`_after` pair, or
reads a timestamp/elapsed. Anything else cannot show order.

Crude on purpose — every hit must be read.
"""
import pathlib
import re
import sys

ORDER = re.compile(r"\b(first|before|mid-request|mid request|then|subsequently|initial|"
                   r"precedence|earlier|order|still)\b", re.I)
SHOWS_ORDER = re.compile(r"(_before|_after|\[:1\]|\[:2\]|\[0\]|\b0\]|elapsed|_t0|t_term|delta|"
                         r">\s*\w*_before|<|>|monotonic|prev)")


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
            if not lab or not ORDER.search(lab):
                continue
            first, _ = split_top(pred)
            if SHOWS_ORDER.search(first) or SHOWS_ORDER.search(pred[:400]):
                continue
            line = text[:m.start()].count("\n") + 1
            hits.append((p.name, line, lab[:100], " ".join(first.split())[:95]))
    print(f"FLAGGED {len(hits)} order-claiming leg(s) whose predicate looks order-blind\n")
    for name, line, lab, pred in hits:
        print(f"{name}:{line}\n   label: {lab}\n   pred : {pred}\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
