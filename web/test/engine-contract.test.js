/**
 * The contract between the timeline page and the WebAssembly engine.
 *
 * These are written in different languages with nothing checking that they
 * agree. The engine serialises Rust structs to JSON; the page reads fields
 * out of that JSON by name. Rename a field in Rust and nothing fails to
 * compile, nothing fails to type-check, and the wasm loads perfectly — the
 * page just renders `undefined` in every row.
 *
 * That failure mode is why this file exists. It asserts every field the page
 * actually reads, so the contract breaks in CI instead of in front of a
 * visitor.
 *
 * Run against the nodejs-target build (`make web-test`), which is the same
 * Rust as the browser build with different bindings.
 */

import { test } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const { Session, version } = require("../pkg-node/samsara_wasm.js");

/** Every field `renderEvents` and `describe` touch. */
function assertEventShape(event, where) {
  for (const field of ["seq", "kind", "name", "identity", "id_short", "logical_time"]) {
    assert.notEqual(event[field], undefined, `${where}: missing \`${field}\``);
  }
  assert.equal(typeof event.seq, "number", `${where}: seq must be a number`);
  assert.equal(typeof event.id_short, "string");
  assert.equal(event.id_short.length, 12, `${where}: id_short is rendered verbatim`);

  // `describe()` reaches into request.body for .path and .step.
  assert.ok("request" in event, `${where}: missing request`);
  assert.ok(event.request === null || typeof event.request === "object");
  if (event.request && event.kind === "tool") {
    assert.ok("body" in event.request, `${where}: describe() reads request.body`);
  }

  // `summarise()` switches on outcome.status.
  assert.ok("outcome" in event, `${where}: missing outcome`);
  if (event.outcome) {
    assert.ok(
      ["ok", "err"].includes(event.outcome.status),
      `${where}: unexpected outcome.status ${event.outcome.status}`,
    );
  }
  // Optional, but must be present as a key so `event.shadow` is not a typo.
  assert.ok("shadow" in event, `${where}: missing shadow key`);
  assert.ok("fault" in event, `${where}: missing fault key`);
}

function assertRunShape(run, where) {
  for (const field of ["events", "violations", "holes", "faultable"]) {
    assert.notEqual(run[field], undefined, `${where}: missing \`${field}\``);
  }
  assert.ok(Array.isArray(run.events), `${where}: events must be an array`);
  assert.ok(Array.isArray(run.faultable));
  assert.equal(typeof run.holes, "number");
  run.events.forEach((e, i) => assertEventShape(e, `${where} event ${i}`));
  run.violations.forEach((v, i) => {
    assert.equal(typeof v.invariant, "string", `${where} violation ${i}`);
    assert.equal(typeof v.detail, "string", `${where} violation ${i}`);
    assert.ok("at_seq" in v, `${where} violation ${i}: page reads at_seq`);
  });
}

test("the engine exposes a version string", () => {
  assert.match(version(), /^\d+\.\d+\.\d+$/);
});

test("recording() returns the shape the page renders", () => {
  const run = JSON.parse(new Session("naive").recording());
  assertRunShape(run, "recording");

  assert.ok(run.events.length > 0, "the demo recording is not empty");
  assert.equal(run.violations.length, 0, "the clean recording has no violations");
  assert.ok(run.faultable.length > 0, "there is something to click");
});

test("fork() applies a fault and reports the violation", () => {
  const session = new Session("naive");
  const recording = JSON.parse(session.recording());
  const target = recording.events.find((e) => e.name === "delete_file");
  assert.ok(target, "the demo has a delete_file effect to break");

  // Exactly the payload the page's fault palette builds.
  const run = JSON.parse(
    session.fork(JSON.stringify({ points: [{ seq: target.seq, fault: { type: "timeout" } }] })),
  );
  assertRunShape(run, "fork");

  const faulted = run.events.filter((e) => e.fault);
  assert.equal(faulted.length, 1, "one fault applied");
  assert.equal(typeof faulted[0].fault, "string", "the page renders fault as a label");

  // The page prints `event.shadow` as "really returned: ...".
  assert.ok(faulted[0].shadow, "a faulted effect carries its shadow");
  assert.equal(faulted[0].shadow.status, "ok", "the suppressed outcome had succeeded");

  assert.equal(run.violations.length, 1, "the duplicate delete is caught");
  assert.match(run.violations[0].invariant, /duplicate/);
});

test("every fault in the page's palette is accepted", () => {
  // Mirrors the FAULTS array in web/index.html. A fault the engine rejects
  // renders an error object into the timeline instead of a run.
  const palette = [
    { type: "timeout" },
    { type: "error", code: "503" },
    { type: "truncate", keep: 12 },
    { type: "malformed" },
    { type: "delay", ms: 4000 },
  ];

  const session = new Session("naive");
  const target = JSON.parse(session.recording()).events.find((e) => e.name === "delete_file");

  for (const fault of palette) {
    const run = JSON.parse(
      session.fork(JSON.stringify({ points: [{ seq: target.seq, fault }] })),
    );
    assert.equal(run.error, undefined, `engine rejected ${JSON.stringify(fault)}`);
    assertRunShape(run, `fork with ${fault.type}`);
  }
});

test("a malformed schedule reports an error rather than panicking", () => {
  // A wasm panic poisons the module and every later click silently fails.
  const run = JSON.parse(new Session("naive").fork('{"points":"not an array"}'));
  assert.equal(typeof run.error, "string", "bad input must surface as an error field");
});

test("search() returns the fields the page renders", () => {
  const finding = JSON.parse(new Session("naive").search(500, 4));
  assert.notEqual(finding, null, "the naive agent is breakable within 500 seeds");

  for (const field of [
    "seed",
    "description",
    "shrunk",
    "shrunk_description",
    "evaluations",
    "started_with",
    "violations",
  ]) {
    assert.notEqual(finding[field], undefined, `search: missing \`${field}\``);
  }

  // The page assigns `finding.shrunk.points` straight into its schedule.
  assert.ok(Array.isArray(finding.shrunk.points));
  assert.ok(finding.shrunk.points.length >= 1);
  for (const point of finding.shrunk.points) {
    assert.equal(typeof point.seq, "number");
    assert.equal(typeof point.fault.type, "string");
  }
});

test("the fixed agent survives what breaks the naive one", () => {
  const finding = JSON.parse(new Session("naive").search(500, 4));
  const run = JSON.parse(
    new Session("idempotent").fork(JSON.stringify({ points: finding.shrunk.points })),
  );
  assert.equal(run.violations.length, 0, "the toggle in the page must show a real difference");
});
