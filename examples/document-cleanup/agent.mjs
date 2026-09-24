/**
 * A document cleanup agent, written the way these are actually written.
 *
 * Every questionable decision in here is one I have seen in real agent code,
 * and none of them look wrong on the page:
 *
 *   1. A destructive action gated on a model's confidence score.
 *   2. Three metadata lookups fired concurrently, folded in as they land.
 *   3. A retry on a delete, because the call "failed".
 *   4. An audit record written after the fact.
 *   5. A notification sent once the work is done.
 *
 * It runs correctly. It will pass any test you write against its happy path.
 */

import { wrapTools, now, random, isAttached } from "@samsara/shim";

// Samsara sets this when attached. Unattached, the agent talks to the local
// stub provider, so the example runs on its own and the demo can show it
// working before Samsara is involved at all.
const MODEL = process.env.ANTHROPIC_BASE_URL ?? "http://127.0.0.1:8899";
const CONFIDENCE_BAR = 0.8;
const MAX_ATTEMPTS = 3;

/**
 * `FIXED=1` turns on both fixes, so a demo can show the same agent before
 * and after without switching files.
 *
 *   - a margin band, so no delete rides on a hair's-width confidence
 *   - an idempotency key stable across retries, so a retried delete is
 *     deduplicated rather than repeated
 */
const FIXED = process.env.FIXED === "1";
const MARGIN = 0.05;

// --- the world -------------------------------------------------------------
//
// Deliberately real: these mutate a file so a demo can show that the world
// was touched twice, rather than that a counter went up twice.

import { appendFileSync } from "node:fs";
const LEDGER = process.env.LEDGER ?? "./ledger.log";
const record = (line) => appendFileSync(LEDGER, line + "\n");

const tools = wrapTools({
  fetch_metadata: async ({ id }) => ({ id, size: id.length * 100 }),
  delete_document: async ({ id }) => {
    record(`DELETED ${id}`);
    return { deleted: true, id };
  },
  log_audit: async ({ action, id }) => {
    record(`AUDIT ${action} ${id}`);
    return { logged: true };
  },
  notify_owner: async ({ id }) => {
    record(`NOTIFIED ${id}`);
    return { sent: true };
  },
});

// --- the model -------------------------------------------------------------

async function classify(id) {
  const response = await fetch(`${MODEL}/v1/messages`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      model: "claude-sonnet-4",
      messages: [{ role: "user", content: `is document ${id} stale?` }],
    }),
  });
  return response.json();
}

// --- the agent -------------------------------------------------------------

async function main() {
  const id = "report-2019";

  // 1. Ask the model. It returns a typed verdict and a confidence score.
  const verdict = await classify(id);
  const { answer, confidence } = verdict;

  // 2. Three lookups at once, folded in as they arrive.
  const parts = await Promise.all(
    ["owner", "course", "size"].map((f) => tools.fetch_metadata({ id: `${id}:${f}` })),
  );
  const summary = parts.map((p) => p.id).join(",");

  if (answer !== "yes") {
    console.log(`keeping ${id}`);
    return;
  }

  const bar = FIXED ? CONFIDENCE_BAR + MARGIN : CONFIDENCE_BAR;
  if (confidence <= bar) {
    // The fixed agent escalates in the band rather than guessing, so no
    // destructive action depends on a difference the model cannot support.
    if (FIXED && confidence > CONFIDENCE_BAR - MARGIN) {
      console.log(`escalating ${id} (confidence ${confidence})`);
    } else {
      console.log(`keeping ${id} (confidence ${confidence})`);
    }
    return;
  }

  // 3. Delete, retrying on failure with jittered backoff.
  let deleted = false;
  for (let attempt = 1; attempt <= MAX_ATTEMPTS && !deleted; attempt++) {
    try {
      await tools.delete_document(
        FIXED ? { id, idempotency_key: `delete:${id}` } : { id },
      );
      deleted = true;
    } catch {
      const jitter = await random();
      const base = 100 << (attempt - 1);
      const wakeAt = (await now()) + base + jitter * base;
      void wakeAt;
    }
  }

  if (!deleted) {
    console.log(`gave up on ${id}`);
    return;
  }

  // 4 and 5. Record it, tell the owner.
  await tools.log_audit({ action: "delete", id });
  await tools.notify_owner({ id });
  console.log(`deleted ${id} (${summary})`);
}

main().catch((e) => {
  console.error(String(e?.message ?? e));
  process.exit(1);
});
