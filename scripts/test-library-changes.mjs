// Run with: node scripts/test-library-changes.mjs (no third-party dependencies).
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import vm from "node:vm";

const html = readFileSync(new URL("../src/bin/server/frontend/index.html", import.meta.url), "utf8");
function source(start, end) {
  const from = html.indexOf(start);
  const to = html.indexOf(end, from);
  assert.ok(from >= 0 && to > from, `frontend section: ${start}`);
  return html.slice(from, to);
}
const changes = source("  const LibraryChanges = (() => {", "  // ---------- now-playing drawer:");
const writes = source("  async function postJson(", "  function setStatus(");
const songEdit = source("    async function songEdit(", "    // Identity links go through");
const replay = source("    async function replay(", "    let replaying = false;");

function clients({ broadcast = true } = {}) {
  const channels = [];
  class BroadcastChannel {
    constructor() { channels.push(this); }
    postMessage(data) {
      for (const peer of channels) if (peer !== this) peer.onmessage({ data });
    }
  }
  function client() {
    let nextTimer = 0;
    const timers = new Map();
    const requests = [], recorded = [], errors = [], refreshed = [];
    const row = { a: 1, b: 2, kind: "cover", enabled: true };
    const reply = { ok: true, status: 200, body: { undo: [row], redo: [row] } };
    const api = vm.runInNewContext(`${changes}\n${writes}\n${songEdit}\n${replay}
      ;({ LibraryChanges, postJson, songEdit, replay })`, {
      BroadcastChannel: broadcast ? BroadcastChannel : undefined,
      setTimeout: (fn) => { const id = ++nextTimer; timers.set(id, fn); return id; },
      clearTimeout: (id) => timers.delete(id),
      fetch: async (path, options) => {
        requests.push({ path, body: JSON.parse(options.body) });
        return { ok: reply.ok, status: reply.status, json: async () => reply.body };
      },
      errorEl: { hidden: true },
      record: (entry) => recorded.push(entry),
      flash: () => {},
      refreshAfterEdit: (id) => refreshed.push(id),
      currentEntryId: 1,
      showError: (message) => errors.push(message),
    });
    let notifications = 0;
    api.LibraryChanges.subscribe(() => notifications++);
    return {
      ...api, requests, recorded, errors, refreshed, reply,
      get notifications() { return notifications; },
      flush() {
        const pending = [...timers.values()];
        timers.clear();
        for (const fn of pending) fn();
      },
    };
  }
  return { editor: client(), player: client() };
}

test("graph song edits refresh the player locally and in another tab", async () => {
  const { editor, player } = clients();
  const operations = ["relate", "disable", "enable", "move", "detach", "set_original", "set_parent"];
  for (const [i, op] of operations.entries()) {
    assert.equal(await editor.songEdit(op, { op, a: 1, b: 2 }), true);
    editor.flush();
    player.flush();
    assert.equal(editor.notifications, i + 1, op);
    assert.equal(player.notifications, i + 1, op);
    assert.equal(editor.requests[i].path, "/api/songs/edit");
    assert.equal(editor.requests[i].body.op, op);
  }
  assert.equal(editor.recorded.length, operations.length);
  assert.equal(editor.refreshed.length, operations.length);
});

test("undo and redo of a song edit each refresh both tabs", async () => {
  const { editor, player } = clients();
  await editor.songEdit("Cover", { op: "relate", a: 1, b: 2, kind: "cover" });
  editor.flush();
  player.flush();
  let undo = editor.recorded[0];
  for (let count = 2; count <= 3; count++) {
    undo = await editor.replay(undo);
    editor.flush();
    player.flush();
    assert.equal(editor.notifications, count);
    assert.equal(player.notifications, count);
    assert.equal(editor.requests.at(-1).path, "/api/relations/rows");
  }
});

test("rejected graph edits leave the player and undo history alone", async () => {
  const { editor, player } = clients();
  const blocked = [];
  editor.reply.ok = false;
  editor.reply.status = 409;
  editor.reply.body = { message: "Different songs" };
  assert.equal(await editor.songEdit("Merge", { op: "set_parent", entries: [2], parent: 1 }, { onBlocked: (message) => blocked.push(message) }), false);
  assert.deepEqual(blocked, ["Different songs"]);
  editor.reply.status = 500;
  assert.equal(await editor.songEdit("Cover", { op: "relate" }), false);
  assert.deepEqual(editor.errors, ["Different songs"]);
  editor.flush();
  player.flush();
  assert.equal(editor.notifications, 0);
  assert.equal(player.notifications, 0);
  assert.equal(editor.recorded.length, 0);
  assert.equal(editor.refreshed.length, 0);
});

test("a burst of relation and song writes coalesces into one player refresh", async () => {
  const { editor, player } = clients();
  for (const kind of ["link", "unlink", "derive", "relate", "unrelate", "restore"]) {
    await editor.postJson(`/api/relations/${kind}`, { a: 1, b: 2 });
  }
  await editor.postJson("/api/songs/edit", { op: "detach", entries: [2] });
  editor.flush();
  player.flush();
  assert.equal(editor.notifications, 1);
  assert.equal(player.notifications, 1);
  await editor.postJson("/api/playback/command", { type: "pause" });
  editor.flush();
  player.flush();
  assert.equal(editor.notifications, 1);
  assert.equal(player.notifications, 1);
});

test("song edits still refresh the local player without BroadcastChannel", async () => {
  const { editor } = clients({ broadcast: false });
  await editor.songEdit("Detach", { op: "detach", entries: [2] });
  editor.flush();
  assert.equal(editor.notifications, 1);
});
