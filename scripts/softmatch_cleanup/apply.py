"""Apply the cleanup. Dry-run unless --apply. Writes cleanup_log.jsonl (undo data)."""
import os, sqlite3, json, sys, time, collections
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from rules import *

TRANSFORMS = [  # first match wins, checked on the derived side's markers
    ("instrumental", r"instrumental|inst(ver(sion)?)?$|^inst|offvocal|karaoke|minusone|カラオケ|インスト(?!ップ)|オフボーカル|stem|backingtrack|singalong|acapella|acappella"),
    ("tv_size", r"tvsize|tvver|tvedit|tvアニメサイズ|tvサイズ"),
    ("short_ver", r"short|ショート"),
    ("game_ver", r"gamesize|gamever|gameedit|ゲームサイズ"),
    ("remix", r"remix|rmx|mix$|bootleg|flip"),
    ("live", r"live|concert|session|ライブ|day\d|公演"),
    ("cover", r"cover|歌ってみた|カバー|coveredby"),
    ("edit", r"edit$|radioedit|extended|12inch|7inch"),
    ("arrangement", r"acoustic|arrange|アレンジ|piano|orchestra|jazz|lofi|eurobeat|unplugged|ver$|version$"),
]
TR = [(t, re.compile(p)) for t, p in TRANSFORMS]
def transformation(msets):
    toks = [m for ms in msets for m in ms]
    for t, rx in TR:
        if any(rx.search(m) for m in toks): return t
    return None

def direction(names, a, b):
    """derived entry from markers, or None when the titles don't say."""
    ka, pa, ma = entry_view(names[a]); kb, pb, mb = entry_view(names[b])
    if pa and not pb: return b, mb
    if pb and not pa: return a, ma
    if not pa and not pb:
        if any(x < y for x in ma for y in mb): return b, mb
        if any(y < x for x in ma for y in mb): return a, ma
    return None, None

apply = "--apply" in sys.argv
db = sqlite3.connect(DB)
names, parents, fetched, ty = load(db)
now = int(time.time())
log = []
stats = collections.Counter()

rel_rows = db.execute("""select entry_a, entry_b, kind, origin, confidence, extra from entry_relation
    where enabled=1 and origin in ('heuristic','jev') and kind in ('derived_from','alt_version','live','remix','instrumental','cover','arrangement')""").fetchall()
for a, b, k, o, c, x in rel_rows:
    why = judge(ty[a], a, b, c, names, parents, fetched)
    if why:
        stats[f"disable {ty[a]} {k}: {why}"] += 1
        log.append({"op": "disable", "a": a, "b": b, "kind": k, "why": why})
        continue
    if ty[a] != "track": continue
    extra = json.loads(x) if x else {}
    if not isinstance(extra, dict):  # combined extras (combine_extra arrays): leave alone
        stats["combined extra left as is"] += 1; continue
    d, ms = direction(names, a, b)
    new = dict(extra)
    if d is not None:
        src = b if d == a else a
        if extra.get("derived_entry") not in (None, d): stats["direction flipped"] += 1
        elif extra.get("derived_entry") is None: stats["direction added"] += 1
        new["derived_entry"], new["source_entry"] = d, src
        t = transformation(ms)
        if t: new["transformation"] = t
        else: new.pop("transformation", None)
    else:
        # titles don't say which is the version: model kind is unreliable, keep it only if it isn't degenerate
        if new.get("transformation") in ("instrumental",):
            new.pop("transformation"); stats["unmarked instrumental label dropped"] += 1
    if new.get("transformation") != extra.get("transformation"): stats["transformation changed"] += 1
    if new != extra:
        new["cleanup"] = "2026-10-17"
        log.append({"op": "extra", "a": a, "b": b, "kind": k, "old": x, "new": json.dumps(new, ensure_ascii=False)})

merge_rows = db.execute("select entry_a, entry_b, origin from entry_relation where enabled=1 and kind='same_identity' and origin in ('heuristic','jev')").fetchall()
for a, b, o in merge_rows:
    why = judge_merge(ty[a], a, b, names)
    if why:
        stats[f"unmerge {ty[a]}: {why}"] += 1
        log.append({"op": "unmerge", "a": a, "b": b, "why": why})

for k, v in sorted(stats.items()): print(f"{v:6}  {k}")
if not apply:
    print("dry run; pass --apply"); sys.exit()

with db:
    for e in log:
        if e["op"] == "disable":
            db.execute("update entry_relation set enabled=0 where entry_a=? and entry_b=? and kind=?", (e["a"], e["b"], e["kind"]))
        elif e["op"] == "extra":
            db.execute("update entry_relation set extra=? where entry_a=? and entry_b=? and kind=?", (e["new"], e["a"], e["b"], e["kind"]))
        else:  # mirror record_identity_feedback(Different)
            db.execute("""insert into dedup_feedback(entry_a, entry_b, judgment, origin, note, created_at)
                values (?,?,'different_identity','cleanup',?,?)""", (e["a"], e["b"], "softmatch cleanup 2026-10-17: " + e["why"], now))
            db.execute("""insert into entry_relation(entry_a, entry_b, kind, confidence, origin, enabled, extra)
                values (?,?,'different_identity',1.0,'cleanup',1,null)
                on conflict(entry_a, entry_b, kind) do update set enabled=1""", (e["a"], e["b"]))
            db.execute("update entry_relation set enabled=0 where entry_a=? and entry_b=? and kind='same_identity'", (e["a"], e["b"]))
log_path = os.path.join(os.path.dirname(DB), time.strftime("softmatch-cleanup-%Y%m%d-%H%M%S.jsonl"))
with open(log_path, "w") as f:
    for e in log: f.write(json.dumps(e, ensure_ascii=False) + "\n")
print("applied", len(log), "changes; undo log:", log_path)
