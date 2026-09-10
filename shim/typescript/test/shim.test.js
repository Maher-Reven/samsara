/**
 * Tests for the tool shim.
 *
 * The shim is thin, but "thin" is not "obviously correct": it decides whether
 * the real tool runs at all. Getting that wrong during replay means deleting
 * a file for real while you are debugging why a file was deleted for real.
 *
 * `fetch` is stubbed so the engine's side of the protocol can be scripted,
 * including the `return` branch that the recording proxy does not yet emit.
 */

import { test, beforeEach, afterEach } from "node:test";
import assert from "node:assert/strict";

process.env.SAMSARA_ENDPOINT = "http://127.0.0.1:0/_samsara";
const { wrapTool, wrapTools, isAttached, SamsaraToolError } = await import("../dist/index.js");

const realFetch = globalThis.fetch;
let calls = [];

/** Script the engine's replies, in order, per endpoint. */
function stub(replies) {
  calls = [];
  globalThis.fetch = async (url, init) => {
    const path = String(url).slice(String(url).indexOf("/_samsara") + "/_samsara".length);
    calls.push({ path, body: JSON.parse(init.body) });
    const reply = replies[path];
    if (reply === undefined) throw new Error(`unscripted call to ${path}`);
    return { ok: true, status: 200, json: async () => reply };
  };
}

beforeEach(() => { calls = []; });
afterEach(() => { globalThis.fetch = realFetch; });

test("isAttached reflects the environment", () => {
  assert.equal(isAttached(), true);
});

test("recording: the real tool runs and its result is reported", async () => {
  stub({
    "/begin": { action: "execute" },
    "/end": { outcome: { status: "ok", value: { ok: true } } },
  });

  let ran = 0;
  const del = wrapTool("delete_file", async ({ path }) => {
    ran += 1;
    return { ok: true, path };
  });

  const result = await del({ path: "/x" });

  assert.equal(ran, 1, "the tool must actually run while recording");
  assert.deepEqual(result, { ok: true });
  assert.deepEqual(calls.map((c) => c.path), ["/begin", "/end"]);
  assert.deepEqual(calls[0].body, { name: "delete_file", body: { path: "/x" } });
  assert.deepEqual(calls[1].body.outcome, { status: "ok", value: { ok: true, path: "/x" } });
});

test("replay: the real tool does NOT run", async () => {
  // The single most important property in this file. If the shim executes a
  // tool during replay, replaying a run that deleted a file deletes it again.
  stub({ "/begin": { action: "return", outcome: { status: "ok", value: "recorded" } } });

  let ran = 0;
  const del = wrapTool("delete_file", async () => { ran += 1; return "live"; });

  const result = await del({ path: "/x" });

  assert.equal(ran, 0, "replay must never touch the real world");
  assert.equal(result, "recorded");
  assert.deepEqual(calls.map((c) => c.path), ["/begin"], "and must not report an end");
});

test("an injected fault surfaces as a throw", async () => {
  stub({
    "/begin": { action: "execute" },
    "/end": { outcome: { status: "err", code: "timeout", message: "injected" } },
  });

  const del = wrapTool("delete_file", async () => ({ ok: true }));

  await assert.rejects(() => del({ path: "/x" }), (error) => {
    assert.ok(error instanceof SamsaraToolError);
    assert.equal(error.code, "timeout");
    return true;
  });
});

test("the engine may fault an outcome the tool reported as success", async () => {
  // The lost-response case: the work happened, the caller is told it did not.
  stub({
    "/begin": { action: "execute" },
    "/end": { outcome: { status: "err", code: "timeout", message: "lost in flight" } },
  });

  let ran = 0;
  const del = wrapTool("delete_file", async () => { ran += 1; return { ok: true }; });

  await assert.rejects(() => del({ path: "/x" }));
  assert.equal(ran, 1, "the side effect really happened");
  assert.equal(calls[1].body.outcome.status, "ok", "and was reported honestly to the engine");
});

test("a throwing tool is recorded rather than lost", async () => {
  stub({
    "/begin": { action: "execute" },
    "/end": { outcome: { status: "err", code: "tool_error", message: "disk on fire" } },
  });

  const del = wrapTool("delete_file", async () => { throw new Error("disk on fire"); });

  await assert.rejects(() => del({ path: "/x" }));
  assert.equal(calls[1].body.outcome.status, "err");
  assert.equal(
    calls[1].body.outcome.code,
    "tool_error",
    "an unclassified throw must not be labelled a timeout — it never reached anything, " +
      "so nothing can have taken effect, and the duplicate check must stay certain",
  );
});

test("wrapTools wraps every entry and preserves names", async () => {
  stub({
    "/begin": { action: "return", outcome: { status: "ok", value: 1 } },
  });

  const tools = wrapTools({
    alpha: async () => "live",
    beta: async () => "live",
  });

  assert.deepEqual(Object.keys(tools), ["alpha", "beta"]);
  assert.equal(await tools.alpha({}), 1);
  assert.equal(calls[0].body.name, "alpha");
});

test("null arguments are sent as null, not dropped", async () => {
  stub({ "/begin": { action: "return", outcome: { status: "ok", value: null } } });
  const t = wrapTool("ping", async () => "live");
  await t(undefined);
  assert.equal(calls[0].body.body, null);
});

test("detached: wrapTools is a pass-through", async () => {
  // Import a second copy with the endpoint unset, so the production path —
  // where the shim must cost nothing — is genuinely exercised.
  const saved = process.env.SAMSARA_ENDPOINT;
  delete process.env.SAMSARA_ENDPOINT;
  const fresh = await import(`../dist/index.js?detached=${Date.now()}`);

  const original = async () => "live";
  const tools = fresh.wrapTools({ delete_file: original });

  assert.equal(fresh.isAttached(), false);
  assert.equal(tools.delete_file, original, "must be the very same function, not a wrapper");

  process.env.SAMSARA_ENDPOINT = saved;
});
