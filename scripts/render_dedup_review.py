#!/usr/bin/env python3
"""Render a dedup task set as a self-contained HTML review sheet."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Any

from validate_dedup_ledger import load_jsonl


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--tasks", type=Path, default=Path("data/dedup-calibration.private.jsonl")
    )
    parser.add_argument(
        "--lane-a", type=Path, default=Path("data/dedup-agent-labels/luna-low-a")
    )
    parser.add_argument(
        "--lane-b", type=Path, default=Path("data/dedup-agent-labels/luna-low-b")
    )
    parser.add_argument("--adjudications", type=Path, default=Path("data/dedup-adjudications.final.jsonl"))
    parser.add_argument("--output", type=Path, default=Path("data/dedup-review.html"))
    return parser.parse_args()


def load_dir(path: Path) -> list[dict[str, Any]]:
    result: list[dict[str, Any]] = []
    for source in sorted(path.glob("*.jsonl")):
        result.extend(load_jsonl(source))
    return result


def compact_view(view: dict[str, Any]) -> dict[str, Any]:
    records = []
    for record in view["records"]:
        facts = []
        for fact in record["facts"]:
            value = fact["value"]
            if isinstance(value, dict):
                value = value.get("name") or value.get("title") or value.get("value") or value
            facts.append({"field": fact["field"], "value": value})
        records.append(
            {
                "source": record["source"],
                "identifier": record["identifier"],
                "facts": facts,
            }
        )
    return {"title": view.get("display_title") or "(untitled view)", "records": records}


def main() -> None:
    args = parse_args()
    tasks = load_jsonl(args.tasks)
    lane_a = {row["item_id"]: row for row in load_dir(args.lane_a)}
    lane_b = {row["item_id"]: row for row in load_dir(args.lane_b)}
    latest = {}
    if args.adjudications.exists():
        for row in load_jsonl(args.adjudications):
            latest[row["item_id"]] = row

    rows = []
    for index, task in enumerate(tasks, 1):
        item_id = task["item_id"]
        a_row = lane_a.get(item_id)
        b_row = lane_b.get(item_id)
        a = (
            a_row["judgment"]["factual"]["entity_relation"]
            if a_row is not None
            else "unlabeled"
        )
        b = (
            b_row["judgment"]["factual"]["entity_relation"]
            if b_row is not None
            else "unlabeled"
        )
        adjudication = latest.get(item_id)
        if adjudication:
            status = adjudication["status"]
            final = (
                "excluded"
                if status == "excluded"
                else adjudication["judgment"]["factual"]["entity_relation"]
            )
            relations = [
                relation["predicate"] + ": " + json.dumps(relation["metadata"], ensure_ascii=False)
                for relation in adjudication["judgment"]["factual"]["relations"]
            ]
            rationale = adjudication["judgment"]["rationale"]
            resolution = "adjudicated"
        elif a_row is not None and b_row is not None:
            status = "agreed" if a == b else "missing_adjudication"
            final = a if a == b else "unresolved"
            relations = []
            rationale = a_row["judgment"]["rationale"]
            resolution = status
        else:
            status = "unlabeled"
            final = "unlabeled"
            relations = []
            rationale = "No annotation has been attached to this task."
            resolution = "unlabeled"
        hidden = task.get("hidden", {})
        rows.append(
            {
                "n": index,
                "id": item_id.removeprefix("sha256:"),
                "type": task["entry_type"],
                "frame": hidden.get("sampling_frame", "unknown"),
                "stratum": hidden.get("stratum", ""),
                "left": compact_view(task["left"]),
                "right": compact_view(task["right"]),
                "vote_a": a,
                "vote_b": b,
                "final": final,
                "status": status,
                "resolution": resolution,
                "relations": relations,
                "rationale": rationale,
            }
        )

    payload = json.dumps(rows, ensure_ascii=False).replace("</", "<\\/")
    template = """<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Dedup dataset review</title>
<style>
:root{color-scheme:dark;--bg:#0d1117;--panel:#161b22;--line:#30363d;--text:#e6edf3;--muted:#8b949e;--same:#3fb950;--diff:#f85149;--ins:#d29922;--excluded:#8b949e}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--text);font:14px/1.45 system-ui,sans-serif}header{position:sticky;top:0;z-index:2;padding:18px 24px;background:#0d1117ee;border-bottom:1px solid var(--line);backdrop-filter:blur(8px)}h1{font-size:20px;margin:0 0 12px}.controls{display:flex;gap:8px;flex-wrap:wrap}input,select{background:var(--panel);color:var(--text);border:1px solid var(--line);border-radius:6px;padding:8px 10px}input{min-width:300px;flex:1}.summary{margin-top:10px;color:var(--muted)}main{padding:16px 24px 48px;display:grid;gap:10px}.card{background:var(--panel);border:1px solid var(--line);border-radius:8px;padding:14px}.top{display:flex;align-items:center;gap:9px;flex-wrap:wrap}.num{color:var(--muted)}code{color:#79c0ff}.badge{border:1px solid var(--line);border-radius:999px;padding:2px 8px}.same_identity{color:var(--same)}.different_identity{color:var(--diff)}.insufficient_evidence{color:var(--ins)}.excluded{color:var(--excluded)}.pair{display:grid;grid-template-columns:1fr 1fr;gap:12px;margin-top:12px}.view{border-left:3px solid var(--line);padding-left:10px;min-width:0}.view h2{font-size:16px;margin:0 0 7px}.record{margin:7px 0}.record a{color:#58a6ff;overflow-wrap:anywhere}.facts{color:var(--muted);font-size:13px}.verdict{margin-top:11px;padding-top:10px;border-top:1px solid var(--line)}details{margin-top:8px}summary{cursor:pointer;color:#c9d1d9}.rationale{color:var(--muted)}@media(max-width:760px){.pair{grid-template-columns:1fr}header,main{padding-left:12px;padding-right:12px}input{min-width:100%}}
</style></head><body><header><h1>Dedup dataset review</h1><div class="controls"><input id="q" placeholder="Search title, ID, source, rationale…"><select id="type"><option value="">All types</option><option>artist</option><option>release</option><option>release_group</option><option>track</option></select><select id="final"><option value="">All verdicts</option><option>unlabeled</option><option>same_identity</option><option>different_identity</option><option>insufficient_evidence</option><option>excluded</option></select><select id="resolution"><option value="">All review states</option><option>unlabeled</option><option>agreed</option><option>adjudicated</option></select></div><div class="summary" id="summary"></div></header><main id="rows"></main>
<script>const data=__PAYLOAD__;
const esc=s=>String(s??'').replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
function view(v){return `<section class="view"><h2>${esc(v.title)}</h2>${v.records.map(r=>`<div class="record"><b>${esc(r.source)}</b> · ${/^https?:/.test(r.identifier)?`<a href="${esc(r.identifier)}" target="_blank" rel="noreferrer">${esc(r.identifier)}</a>`:`<code>${esc(r.identifier)}</code>`}<div class="facts">${r.facts.slice(0,12).map(f=>`${esc(f.field)}: ${esc(typeof f.value==='object'?JSON.stringify(f.value):f.value)}`).join('<br>')}${r.facts.length>12?`<br>… ${r.facts.length-12} more facts`:''}</div></div>`).join('')}</section>`}
function render(){const q=document.querySelector('#q').value.toLowerCase(),type=document.querySelector('#type').value,fin=document.querySelector('#final').value,res=document.querySelector('#resolution').value;const shown=data.filter(x=>(!type||x.type===type)&&(!fin||x.final===fin)&&(!res||x.resolution===res)&&(!q||JSON.stringify(x).toLowerCase().includes(q)));document.querySelector('#summary').textContent=`Showing ${shown.length} / ${data.length} · ${shown.filter(x=>x.final==='same_identity').length} same · ${shown.filter(x=>x.final==='different_identity').length} different · ${shown.filter(x=>x.final==='insufficient_evidence').length} insufficient · ${shown.filter(x=>x.final==='excluded').length} excluded`;document.querySelector('#rows').innerHTML=shown.map(x=>`<article class="card"><div class="top"><span class="num">#${x.n}</span><code>${x.id.slice(0,12)}</code><span class="badge">${esc(x.type)}</span><span class="badge">${esc(x.frame)}</span><span class="badge ${x.final}">${esc(x.final)}</span><span class="badge">${esc(x.resolution)}</span></div><div class="pair">${view(x.left)}${view(x.right)}</div><div class="verdict">Luna A: <span class="${x.vote_a}">${esc(x.vote_a)}</span> · Luna B: <span class="${x.vote_b}">${esc(x.vote_b)}</span> · Final: <b class="${x.final}">${esc(x.final)}</b>${x.relations.length?`<br>Relations: ${x.relations.map(esc).join('; ')}`:''}</div><details><summary>Rationale and sampling details</summary><p class="rationale">${esc(x.rationale)}</p><code>${esc(x.stratum)}</code></details></article>`).join('')}
document.querySelectorAll('input,select').forEach(e=>e.addEventListener('input',render));render();</script></body></html>"""
    args.output.write_text(template.replace("__PAYLOAD__", payload), encoding="utf-8")
    print(f"rows={len(rows)} output={args.output}")


if __name__ == "__main__":
    main()
