// Run with: node scripts/test-graph-layout.mjs (no third-party dependencies).
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
const memorySource = source("  function createGraphLayoutMemory()", "  // The kinds `/api/relations/relate`");
const restoreSource = source("    function restoreNodePositions(", "    // Join fresh data");
const hashSource = source("  function hashUnit(", "  // d3-force's own internals");
const dragSource = source("    function dragStarted(", "    // View mode inspects;");
const camera = { x: 40, y: -20, k: 1.2 };
const point = (id, x, y, userPinned = false) => ({ id: String(id), x, y, userPinned });
const plain = (value) => JSON.parse(JSON.stringify(value));

function editor(storage = new Map(), extra = {}) {
  const context = vm.createContext({
    localStorage: { getItem: (k) => storage.get(k), setItem: (k, v) => storage.set(k, v) },
    setTimeout: () => 1, clearTimeout: () => {},
    currentNodesById: new Map(), renderedViewMode: "song", currentEntryId: 1,
    centerX: 500, centerY: 320,
    ...extra,
  });
  vm.runInContext(`${memorySource}\n${restoreSource}\n${hashSource}\nconst layoutMemory = createGraphLayoutMemory();`, context);
  return {
    context, storage,
    run: (code) => vm.runInContext(code, context),
    remember(mode, nodes, anchor = 1) {
      context.nodes = nodes;
      context.camera = camera;
      vm.runInContext(`layoutMemory.remember(${JSON.stringify(mode)}, nodes, ${anchor}, camera); layoutMemory.flush();`, context);
    },
    restore(mode, nodes, visible = []) {
      context.nodes = nodes;
      context.currentNodesById = new Map(visible.map((n) => [n.id, n]));
      vm.runInContext(`restoreNodePositions(nodes, ${JSON.stringify(mode)}, 1000, 640)`, context);
      return plain(nodes);
    },
  };
}

test("hidden nodes and dragged pins return to their saved positions after focus changes and reloads", () => {
  const first = editor();
  first.remember("song", [point(1, 500, 320), point(2, 123, 456, true)]);
  // Focusing elsewhere only remembers its own visible nodes.
  first.remember("song", [point(3, 700, 600)], 3);
  const reloaded = editor(first.storage);
  const nodes = reloaded.restore("song", [{ id: "2", treeX: 200, treeY: 100 }]);
  assert.deepEqual(nodes.map(({ x, y, fx, fy, userPinned }) => ({ x, y, fx, fy, userPinned })), [
    { x: 123, y: 456, fx: 123, fy: 456, userPinned: true },
  ]);
  assert.deepEqual(plain(reloaded.run('layoutMemory.camera("song", 1)')), camera);
});

test("connection changes keep existing tree nodes in place and lay out only new nodes", () => {
  const e = editor();
  const nodes = e.restore("song", [
    { id: "1", treeX: 100, treeY: 100 },
    { id: "2", treeX: 0, treeY: 300 },
    { id: "3", treeX: 200, treeY: 0 },
  ], [point(1, 500, 320), point(2, 555, 222)]);
  assert.deepEqual(nodes.map(({ x, y, fx, fy }) => [x, y, fx, fy]), [
    [500, 320, 500, 320], [555, 222, 555, 222], [700, 320, 700, 320],
  ]);
});

test("Song and All relations keep independent placements", () => {
  const e = editor();
  e.remember("song", [point(1, 100, 200, true)]);
  e.remember("raw", [point(1, 300, 400)]);
  const visible = [point(1, 100, 200, true)];
  assert.equal(e.restore("raw", [{ id: "1" }], visible)[0].x, 300);
  e.context.renderedViewMode = "raw";
  assert.equal(e.restore("song", [{ id: "1", treeX: 0 }], [point(1, 300, 400)])[0].x, 100);
});

test("a newly dropped placement takes precedence over its cached position", () => {
  const e = editor();
  e.remember("song", [point(2, 100, 200, true)]);
  const [n] = e.restore("song", [point(2, 600, 700, true)]);
  assert.equal(n.fx, 600);
  assert.equal(n.fy, 700);
});

test("clicks and slight pointer wobbles preserve an earlier dragged placement", () => {
  let temperature = 0;
  let reheats = 0;
  const simulation = {
    alphaTarget() { return this; },
    alpha(value) { if (value === undefined) return temperature; temperature = value; return this; },
    restart() { reheats++; return this; },
  };
  const e = editor(new Map(), {
    simulation, ALPHA_DRAG: 0.2, ticked() {}, rememberLayout() {}, handleNodeClick() {},
  });
  e.run(dragSource);
  e.context.node = { ...point(2, 123, 456, true), fx: 123, fy: 456, treeX: 200, treeY: 100 };
  for (const wobble of [0, 2]) {
    e.context.wobble = wobble;
    e.run(`dragStarted({x: 123, y: 456}, node);
      if (wobble) dragged({x: 123 + wobble, y: 456}, node);
      dragEnded({x: 123 + wobble, y: 456}, node);`);
    assert.deepEqual(plain(e.context.node), { id: "2", x: 123, y: 456, userPinned: true,
      fx: 123, fy: 456, treeX: 200, treeY: 100, __dragStartEventX: 123, __dragStartEventY: 456 });
  }
  assert.equal(reheats, 0, "clicks leave neighboring nodes still");
  e.run(`dragStarted({x: 123, y: 456}, node);
    dragged({x: 200, y: 500}, node);
    dragged({x: 124, y: 456}, node);
    dragEnded({x: 124, y: 456}, node);`);
  assert.equal(reheats, 2);
  assert.equal(e.context.node.fx, 124, "returning near the start is still a drag");
  assert.equal(e.context.node.userPinned, true);
});

test("remembered raw nodes stay fixed during a refresh settle without becoming user pins", () => {
  const e = editor();
  e.context.renderedViewMode = "raw";
  e.context.currentNodesById = new Map([["1", point(1, 150, 250)]]);
  e.context.nodes = [{ id: "1" }];
  const result = e.run('restoreNodePositions(nodes, "raw", 1000, 640)');
  assert.equal(result.releaseAfterSettle.length, 1);
  assert.equal(e.context.nodes[0].fx, 150);
  assert.equal(e.context.nodes[0].userPinned, false);
});

test("missing, corrupt or unavailable browser storage does not break layout", () => {
  for (const data of ["invalid json", "null", '{"song":{"positions":[["1",null]]}}']) {
    const e = editor(new Map([["musiclib_graph_layout_v1", data]]));
    assert.equal(e.restore("song", [{ id: "1", treeX: 0, treeY: 0 }])[0].x, 500);
  }
  const unavailable = editor(new Map(), { localStorage: {
    getItem() { throw new Error("unavailable"); }, setItem() { throw new Error("quota"); },
  } });
  unavailable.remember("song", [point(1, 12, 34)]);
  assert.equal(unavailable.restore("song", [{ id: "1" }])[0].x, 12);
});
