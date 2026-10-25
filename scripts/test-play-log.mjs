// Run with: node scripts/test-play-log.mjs (no third-party dependencies).
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import vm from "node:vm";

const html = readFileSync(new URL("../src/bin/server/frontend/index.html", import.meta.url), "utf8");
const logSource = html.slice(html.indexOf("  const PlayLog = (() => {"), html.indexOf("  // ---------- library changes:"));

function player(startResponse = () => Promise.resolve({ play_id: 7, listened_seconds: 0 })) {
  let wall = 0;
  const reports = [];
  const starts = [];
  const beacons = [];
  const events = {};
  const noted = [];
  const log = vm.runInNewContext(`${logSource}; PlayLog`, {
    performance: { now: () => wall * 1000 },
    postJson: (url, body) => { starts.push(body); return startResponse(body); },
    fetch: async (url, options) => { reports.push({ url, ...JSON.parse(options.body) }); return { ok: true }; },
    navigator: { sendBeacon: (url, body) => { beacons.push({ url, body }); return true; } },
    window: { addEventListener: (name, fn) => events[name] = fn },
    ShuffleMix: { notePlayed: (id) => noted.push(id) },
    Blob,
  });
  return { log, reports, starts, beacons, events, noted, tick: (seconds, position) => { wall += seconds; log.progress(position); } };
}

test("the complete frontend JavaScript parses", () => {
  for (const [, source] of html.matchAll(/<script\b[^>]*>([\s\S]*?)<\/script>/g)) new vm.Script(source);
});

test("reports listening while excluding pauses and forward/backward seeks", async () => {
  const p = player();
  p.log.start(42, "stable-session");
  await Promise.resolve();
  p.tick(0, 0);
  p.tick(15, 15);
  p.tick(20, 15); // Paused.
  p.tick(0, 100); // Seek forward.
  p.tick(15, 115);
  assert.equal(p.reports[0].listened_seconds, 30);
  p.tick(0, 0); // Seek backward.
  p.tick(10, 10);
  p.log.finish();
  assert.equal(p.reports[1].listened_seconds, 40);
  assert.equal(p.reports[0].reporter, p.reports[1].reporter);
  assert.equal(p.starts[0].session_key, "stable-session");
  assert.deepEqual(p.noted, [42]);
});

test("a resumed play reports only the new reporter's listening time", async () => {
  const p = player(() => Promise.resolve({ play_id: 7, listened_seconds: 90 }));
  p.log.start(42, "stable-session");
  await Promise.resolve();
  p.tick(0, 0);
  p.tick(30, 30);
  assert.equal(p.reports[0].listened_seconds, 30);
  const reporter = p.reports[0].reporter;
  p.log.start(42, "stable-session");
  await Promise.resolve();
  p.tick(0, 30);
  p.tick(30, 60);
  assert.equal(p.reports[1].url, "/api/plays/7");
  assert.equal(p.reports[1].listened_seconds, 30);
  assert.notEqual(p.reports[1].reporter, reporter);
  p.tick(5, 65);
  p.events.pagehide();
  assert.equal(JSON.parse(await p.beacons[0].body.text()).listened_seconds, 35);
});

test("a delayed start response flushes the old track after a track change", async () => {
  let resolveStart;
  const p = player(() => new Promise((resolve) => resolveStart = resolve));
  p.log.start(1, "first");
  const firstResponse = resolveStart;
  p.tick(0, 0);
  p.tick(20, 20);
  p.log.start(2, "second");
  firstResponse({ play_id: 10 });
  await Promise.resolve();
  assert.equal(p.reports[0].url, "/api/plays/10");
  assert.equal(p.reports[0].listened_seconds, 20);
});
