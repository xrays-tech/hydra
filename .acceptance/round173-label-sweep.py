#!/usr/bin/env python3
"""Round-173 sweep: `check("<label>", <predicate>)` pairs whose LABEL claims more than the
predicate can show.

The class this looks for produced a tautology in `test_model_catalog.py` (round 172): the label said
"narrowed to its route's provider" while the predicate was `"m1" in ids` — true for the anonymous
listing too. The mechanical rule here is deliberately crude and states its own crudeness:

  FLAG when the label contains a strong claim word (exactly / always / never / only / every /
  identical / unchanged / narrowed / same / none / zero / cannot / while) AND the predicate uses NO
  comparison operator and no `all(`/`set(` — i.e. it is membership/truthiness only.

Being crude, it OVER-reports; each hit must be read before it means anything.
"""
import pathlib
import re
import sys

STRONG = re.compile(r"\b(exactly|always|never|only|every|identical|unchanged|narrowed|same|none|"
                    r"zero|cannot|while|still)\b", re.I)
# a predicate that can actually discriminate a set/count/value
DISCRIMINATING = re.compile(r"(==|!=|<=|>=|<|>)|\ball\s*\(|\bset\s*\(|\bany\s*\(|\bsorted\s*\(|\blen\s*\(")


def balanced(text, start):
    """The substring from `start` to the matching close paren (string-aware enough for these files)."""
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


def main():
    hits = []
    for p in sorted(pathlib.Path("integration").glob("*.py")):
        text = p.read_text(encoding="utf-8", errors="replace")
        for m in re.finditer(r"\bcheck\(", text):
            call = balanced(text, m.start() + len("check"))
            body = call[1:-1]
            # split the first two arguments at the first top-level comma
            depth = 0
            quote = None
            cut = None
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
                    cut = i
                    break
            if cut is None:
                continue
            label = body[:cut]
            pred = body[cut + 1:]
            # only look at literal labels
            lab = re.findall(r'"([^"]{6,})"', label)
            label_text = " ".join(lab)
            if not label_text or not STRONG.search(label_text):
                continue
            # the predicate's first expression (up to the next top-level comma)
            depth = 0
            quote = None
            pc = None
            for i, c in enumerate(pred):
                if quote:
                    if c == "\\":
                        continue
                    if pred.startswith(quote, i):
                        quote = None
                    continue
                if c in "\"'":
                    quote = c * 3 if pred.startswith(c * 3, i) else c
                    continue
                if c in "([{":
                    depth += 1
                elif c in ")]}":
                    depth -= 1
                elif c == "," and depth == 0:
                    pc = i
                    break
            first = pred[:pc] if pc is not None else pred
            if DISCRIMINATING.search(first):
                continue
            line = text[:m.start()].count("\n") + 1
            hits.append((p.name, line, label_text[:95], " ".join(first.split())[:80]))
    print(f"FLAGGED {len(hits)} check(s) where the label is strong and the predicate is not\n")
    for name, line, lab, pred in hits:
        print(f"{name}:{line}\n    label: {lab}\n    pred : {pred}\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
