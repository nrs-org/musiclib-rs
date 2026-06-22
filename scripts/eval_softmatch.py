#!/usr/bin/env python3
"""
Evaluate the softmatch heuristic against human labels exported from
scripts/softmatch.html ("Export Labels JSON" button).

Input shape (one object per pair, keyed by "<entry_a>-<entry_b>"):
    {
      "ai_verdict": "MERGE" | "RELATE" | "DISTINCT" | "BARRIER",
      "ai_kind":    "<relate kind or ''>",
      "label":      "pending"
                  | "approved"                       # human agrees with the AI
                  | "override:<VERDICT>[:<kind>]",   # human disagrees
      "title_a", "title_b", ...
    }

Ground truth ("gold") is derived per pair:
    - approved          -> gold = ai_verdict (+ ai_kind)
    - override:X[:kind] -> gold = X          (+ kind)
    - pending/orphaned  -> no ground truth, excluded

NOTE ON SCOPE. The labeling pass covered the positive predictions
(MERGE / RELATE). The huge DISTINCT pool is mostly unlabeled, so:
    * Precision of MERGE / RELATE is measured fully and is trustworthy.
    * Recall is only over the labeled subset; it does NOT account for true
      matches hiding among the unlabeled DISTINCT pairs. Treat recall and any
      DISTINCT-class number as a floor, not the truth.

Usage:
    python3 scripts/eval_softmatch.py [labels.json]
    python3 scripts/eval_softmatch.py --disagreements   # only list mismatches
"""
import json
import sys
from collections import Counter, defaultdict

VERDICTS = ["MERGE", "RELATE", "DISTINCT", "BARRIER"]


def parse_gold(rec):
    """Return (gold_verdict, gold_kind) or (None, None) if not labeled."""
    label = rec.get("label", "pending")
    if rec.get("orphaned") or label == "pending":
        return None, None
    if label == "approved":
        return rec.get("ai_verdict"), (rec.get("ai_kind") or "")
    if label.startswith("override:"):
        parts = label.split(":", 2)  # override : VERDICT [: kind]
        verdict = parts[1] if len(parts) > 1 else None
        kind = parts[2] if len(parts) > 2 else ""
        return verdict, kind
    return None, None


def prf(tp, fp, fn):
    prec = tp / (tp + fp) if (tp + fp) else float("nan")
    rec = tp / (tp + fn) if (tp + fn) else float("nan")
    f1 = 2 * prec * rec / (prec + rec) if (prec + rec) and prec == prec and rec == rec else float("nan")
    return prec, rec, f1


def fmt(x):
    return f"{x:.3f}" if x == x else "  n/a"


def bar(n, total, width=24):
    if not total:
        return ""
    filled = round(n / total * width)
    return "█" * filled + "·" * (width - filled)


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    flags = {a for a in sys.argv[1:] if a.startswith("--")}
    path = args[0] if args else "data/softmatch_labels.json"

    with open(path, encoding="utf-8") as f:
        data = json.load(f)

    total = len(data)
    pending = orphaned = 0
    pairs = []  # (key, ai_verdict, ai_kind, gold_verdict, gold_kind, rec)
    for key, rec in data.items():
        if rec.get("orphaned"):
            orphaned += 1
            continue
        gv, gk = parse_gold(rec)
        if gv is None:
            pending += 1
            continue
        pairs.append((key, rec.get("ai_verdict"), rec.get("ai_kind") or "", gv, gk, rec))

    labeled = len(pairs)

    # ── Coverage ──────────────────────────────────────────────────────────
    print(f"\n=== Coverage ===  ({path})")
    print(f"  pairs in file : {total}")
    print(f"  labeled       : {labeled}")
    print(f"  pending       : {pending}")
    if orphaned:
        print(f"  orphaned      : {orphaned}  (skipped)")

    ai_all = Counter(r.get("ai_verdict") for k, r in data.items() if not r.get("orphaned"))
    ai_labeled = Counter(p[1] for p in pairs)
    print("\n  AI verdict coverage (labeled / total):")
    for v in VERDICTS:
        if ai_all.get(v):
            print(f"    {v:9} {ai_labeled.get(v, 0):4} / {ai_all[v]:<5} {bar(ai_labeled.get(v,0), ai_all[v])}")

    if not labeled:
        print("\nNo labeled pairs — nothing to score.")
        return

    if "--disagreements" not in flags:
        # ── Confusion matrix (rows = AI, cols = gold) ─────────────────────
        present = [v for v in VERDICTS if any(p[1] == v or p[3] == v for p in pairs)]
        cm = defaultdict(lambda: defaultdict(int))
        for _, av, _, gv, _, _ in pairs:
            cm[av][gv] += 1

        print("\n=== Confusion matrix  (row = AI predicted, col = human gold) ===")
        head = "  AI \\ gold │" + "".join(f"{g:>10}" for g in present) + f"{'│ total':>9}"
        print(head)
        print("  " + "─" * (len(head) - 2))
        for av in present:
            row = cm[av]
            rtot = sum(row.values())
            cells = "".join(f"{row.get(g,0):>10}" for g in present)
            print(f"  {av:9} │{cells} │{rtot:>7}")
        print("  " + "─" * (len(head) - 2))
        coltot = "".join(f"{sum(cm[a].get(g,0) for a in present):>10}" for g in present)
        print(f"  {'total':9} │{coltot} │{labeled:>7}")

        agree = sum(1 for _, av, _, gv, _, _ in pairs if av == gv)
        print(f"\n  Overall verdict agreement: {agree}/{labeled} = {agree/labeled:.1%}")

        # ── Per-class precision / recall / F1 ─────────────────────────────
        print("\n=== Per-class (precision is solid; recall only over labeled subset) ===")
        print(f"  {'class':9} {'prec':>6} {'recall':>7} {'f1':>6}   {'tp':>4} {'fp':>4} {'fn':>4}")
        for v in present:
            tp = sum(1 for _, av, _, gv, _, _ in pairs if av == v and gv == v)
            fp = sum(1 for _, av, _, gv, _, _ in pairs if av == v and gv != v)
            fn = sum(1 for _, av, _, gv, _, _ in pairs if av != v and gv == v)
            p, r, f = prf(tp, fp, fn)
            print(f"  {v:9} {fmt(p):>6} {fmt(r):>7} {fmt(f):>6}   {tp:>4} {fp:>4} {fn:>4}")

        # ── Headline: positive precision ──────────────────────────────────
        pos = {"MERGE", "RELATE"}
        pos_pred = [p for p in pairs if p[1] in pos]
        pos_ok = sum(1 for p in pos_pred if p[3] in pos)
        if pos_pred:
            print(
                f"\n  Positive precision (AI said MERGE/RELATE, human kept it positive): "
                f"{pos_ok}/{len(pos_pred)} = {pos_ok/len(pos_pred):.1%}"
            )

        # ── RELATE kind accuracy ──────────────────────────────────────────
        rel = [p for p in pairs if p[1] == "RELATE" and p[3] == "RELATE"]
        if rel:
            kind_ok = sum(1 for p in rel if p[2] == p[4])
            print(
                f"  RELATE kind accuracy (both agree it's RELATE): "
                f"{kind_ok}/{len(rel)} = {kind_ok/len(rel):.1%}"
            )

    # ── Disagreements ─────────────────────────────────────────────────────
    disagree = [p for p in pairs if p[1] != p[3] or (p[1] == "RELATE" == p[3] and p[2] != p[4])]
    print(f"\n=== Disagreements ({len(disagree)}) ===")
    for key, av, ak, gv, gk, rec in sorted(disagree, key=lambda p: (p[1], p[3])):
        ai = f"{av}{'/'+ak if ak else ''}"
        gold = f"{gv}{'/'+gk if gk else ''}"
        ta = (rec.get("title_a") or "")[:32]
        tb = (rec.get("title_b") or "")[:32]
        note = f"  ⟨{rec['note']}⟩" if rec.get("note") else ""
        print(f"  [{key:>9}] AI={ai:<22} -> gold={gold:<22} {ta!r} ✕ {tb!r}{note}")


if __name__ == "__main__":
    main()
