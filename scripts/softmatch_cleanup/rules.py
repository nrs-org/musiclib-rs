"""Precision-first cleanup of learned edition relations. Dry-run by default."""
import sqlite3, re, unicodedata, collections, json, sys, random

import os
DB = os.environ.get("MUSICLIB_DB", os.path.expanduser("~/.local/share/musiclib-rs/musiclib.db"))

BRACKETS = r"\(\)\[\]【】「」『』（）〈〉《》<>＜＞〔〕"
OPEN = "([【「『（〈《<＜〔"
CLOSE = ")]】」』）〉》>＞〕"
# Markers that keep the recording's identity: don't make a title "marked".
NEUTRAL = re.compile(r"^(\d{4} )?(digital )?remaster(ed)?( \d{4})?( version)?$|^(single|album|original|full|mv|music video|official music video|official mv|official video|lyric video|audio)( ver(sion)?\.?)?$|^feat\b|^ft\b|^with\b|^from\b|^cv\b|^tvアニメ|^tv anime|^op$|^ed$|^主題歌|.*主題歌$|.*テーマ$|^prod\b|^music\b|^lyrics\b|^vocal\b|^official$|^mv$|^pv$")
def norm(s):
    s = unicodedata.normalize("NFKC", s).casefold()
    s = re.sub(r"[\s　]+", " ", s)
    return s.strip()
def squash(s):  # comparison key: drop punctuation/space
    return re.sub(r"[\W_]+", "", s)
def split_title(t):
    """-> (base key set, marker list) for one alias."""
    t = norm(t)
    markers = []
    # bracketed segments
    def grab(m):
        markers.append(m.group(1).strip()); return " "
    t2 = re.sub(r"[(\[【「『（〈《<＜〔]([^()\[\]【】「」『』（）〈〉《》<>＜＞〔〕]*)[)\]】」』）〉》>＞〕]", grab, t)
    # ' - suffix' / ' -suffix-' / '~suffix~'
    m = re.match(r"^(.*?)\s+[-–—~～]\s*(.+?)\s*[-–—~～]?\s*$", t2)
    segs = [t2]
    if m and m.group(1).strip():
        segs = [m.group(1)]; markers.append(m.group(2).strip())
        if not MARKERISH.search(m.group(2)): segs.append(m.group(2))
    m = re.match(r"^(.*?)\s*[-~～]([^-~～]+)[-~～]\s*$", segs[0])
    if m and m.group(1).strip():
        segs = [m.group(1)]; markers.append(m.group(2).strip())
    # feat.
    segs = [re.split(r"\s(feat\.?|ft\.?|featuring)\s", s)[0] for s in segs]
    # 'A / B' and 'A／B' segments (youtube "artist / title", "title／artist")
    keys = set()
    for s in segs:
        for part in re.split(r"\s*[/／|｜]\s*", s):
            k = squash(part)
            if len(k) >= 2 or (k and not k.isascii()): keys.add(k)
    parts = []
    for m in markers: parts += [p.strip() for p in re.split(r"[/／;,]", m)]
    real = [squash(m) for m in parts if m and not NEUTRAL.match(m) and squash(m)]
    return keys, real
MARKERISH = re.compile(r"instrumental|inst\\b|off ?vocal|karaoke|remix|\\bmix\\b|\\bver\\b|ver\\.|version|edit\\b|live|acoustic|remaster|cover|short|size|arrange|stem|demo|session|concert|a-?side|b-?side|ライブ|カラオケ|歌ってみた|instrumental|ｲﾝｽﾄ|インスト|オフボーカル|off vocal|music video|\\bmv\\b|\\bpv\\b|official|lyric|piano|orchestra|jazz|eurobeat|lofi|lo-fi|minus ?one|acapella|a cappella|sing-?along|\\d{4}")
GENERIC = re.compile(r"^(intro|outro|interlude|untitled|track \d+|instrumental|inst|off vocal|bonus track|skit|prologue|epilogue|opening|ending|overture|reprise|ボーナストラック|アイキャッチ.*|イントロ|アウトロ|オープニング|エンディング|序曲)$")
def placeholder(name):
    n = norm(name)
    return (n.startswith("[") and n.endswith("]")) or bool(GENERIC.match(n)) or bool(GENERIC.match(squash(n)))

def load(db):
    names = collections.defaultdict(set)
    for eid, n in db.execute("""select es.entry_id, a.name from entry_source es join entry_alias a
            on a.source=es.source and a.identifier=es.identifier"""):
        names[eid].add(n)
    parents = collections.defaultdict(set)
    for eid, ps, pi in db.execute("""select es.entry_id, c.parent_source, c.parent_identifier from entry_child c
            join entry_source es on es.source=c.child_source and es.identifier=c.child_identifier"""):
        parents[eid].add((ps, pi))
    fetched = dict(db.execute("select entry_id, max(fetched_at is not null) from entry_source group by 1"))
    ty = dict(db.execute("select id, entry_type from entry"))
    return names, parents, fetched, ty

def entry_view(names_):
    keys, plain, msets = set(), False, []
    for n in names_:
        k, m = split_title(n)
        keys |= k
        if not m: plain = True
        msets.append(frozenset(m))
    return keys, plain, msets

def lev_ratio(x, y):
    if abs(len(x) - len(y)) > 3: return 0.0
    prev = list(range(len(y) + 1))
    for i, cx in enumerate(x, 1):
        cur = [i]
        for j, cy in enumerate(y, 1):
            cur.append(min(prev[j] + 1, cur[j - 1] + 1, prev[j - 1] + (cx != cy)))
        prev = cur
    return 1 - prev[-1] / max(len(x), len(y))

def shares_base(ka, kb):
    if ka & kb: return True
    return any(min(len(x), len(y)) >= 5 and (x.startswith(y) or y.startswith(x)) and abs(len(x) - len(y)) <= 2 for x in ka for y in kb)

def chained(ma, mb):
    """One side's markers are a strict subset of the other's (e.g. game edit vs game edit + instrumental)."""
    return any(x < y or y < x for x in ma for y in mb)

def judge(ty, a, b, conf, names, parents, fetched):
    """Return a drop reason or None (keep)."""
    if ty == "artist": return "artist relate (not an edition)"
    if ty == "release":
        if parents[a] & parents[b]: return "release siblings under one parent"
        if not fetched.get(a) or not fetched.get(b): return "release stub"
        return None
    if ty != "track": return None
    na, nb = names[a], names[b]
    if any(placeholder(n) for n in na | nb): return "placeholder title"
    ka, pa, ma = entry_view(na); kb, pb, mb = entry_view(nb)
    if not shares_base(ka, kb): return "no shared base title"
    if not pa and not pb and not chained(ma, mb): return "siblings (both sides are versions)"
    if conf < 0.4: return "low confidence"
    return None

if __name__ == "__main__":
    apply = "--apply" in sys.argv
    db = sqlite3.connect(DB if apply else f"file:{DB}?mode=ro", uri=not apply)
    names, parents, fetched, ty = load(db)
    rows = db.execute("""select r.entry_a, r.entry_b, r.kind, r.origin, r.confidence, r.extra from entry_relation r
        where r.enabled=1 and r.origin in ('heuristic','jev') and r.kind != 'same_identity'
          and r.kind in ('derived_from','alt_version','live','remix','instrumental','cover','arrangement')""").fetchall()
    out = []
    for a, b, k, o, c, x in rows:
        why = judge(ty[a], a, b, c, names, parents, fetched)
        out.append((a, b, k, o, c, x, why))
    cnt = collections.Counter((ty[a], why) for a, b, k, o, c, x, why in out)
    for (t, w), n in sorted(cnt.items(), key=lambda z: (z[0][0], str(z[0][1]))): print(f"{t:8} {str(w):40} {n}")
    json.dump(out, open("decisions.json", "w"))

DEGEN = re.compile(r"instrumental|inst(ver(sion)?)?$|offvocal|karaoke|minusone|カラオケ|インスト(?!ップ)|オフボーカル|stem|acapella|acappella|singalong|backingtrack")
COVER = re.compile(r"\\bcover\\b|covered by|歌ってみた|うたってみた|カバー|\\bsings\\b")
def judge_merge(ty, a, b, names):
    if ty != "track": return None
    na, nb = names[a], names[b]
    if any(placeholder(n) for n in na | nb): return "placeholder title"
    dg = lambda n: bool(DEGEN.search(squash(norm(n))))
    all_a, any_a = all(map(dg, na)), any(map(dg, na))
    all_b, any_b = all(map(dg, nb)), any(map(dg, nb))
    if (all_a and not any_b) or (all_b and not any_a): return "instrumental vs vocal"

    return None
