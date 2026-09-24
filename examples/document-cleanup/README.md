# Demo: a document cleanup agent

A complete worked example that exercises every part of Samsara, in about five
minutes, with no API key and no network.

The agent deletes stale documents. It is written the way these are actually
written, and every questionable decision in it looks reasonable on the page:

```js
const { answer, confidence } = await classify(id);      // a typed verdict
const parts = await Promise.all(lookups);               // three at once
if (confidence > 0.8) {
  for (let i = 1; i <= 3 && !deleted; i++) {
    try { await tools.delete_document({ id }); deleted = true; }
    catch { /* back off and retry */ }
  }
  await tools.log_audit({ action: "delete", id });
  await tools.notify_owner({ id });
}
```

It works. It deletes the right document, logs it, tells the owner. Any test
you write against that path passes.

## Setup

From the repo root, one command builds everything and runs the whole thing:

```bash
make example            # runs it straight through
make example-paced      # pauses between beats, for presenting live
```

`--paced` waits for Enter before each beat, so there is room to talk. The
narration is below.

To drive it by hand instead:

```bash
cargo build --release                  # from the repo root
cd shim/typescript && npm install && npm run build && cd -
cd examples/document-cleanup && npm install
node provider.mjs &                    # a stub model, so no API key is needed
```

`samsara` is then at `../../target/release/samsara`.

---

## The demo, in six beats

### 1. It works. Show that first.

```bash
node agent.mjs && cat ledger.log
```

```
DELETED report-2019
AUDIT delete report-2019
NOTIFIED report-2019
```

> *"Three effects, right order, correct outcome. Nothing to see."*

### 2. Record one run

```bash
samsara record --out run.samsara.jsonl -- node agent.mjs
samsara show run.samsara.jsonl
```

```
0 model   claude-sonnet-4    b530bf27041a
1 tool    fetch_metadata     e92e763f0835
2 tool    fetch_metadata     bb26508fb69d     ← issued concurrently
3 tool    fetch_metadata     44ca9eb9239e
4 tool    delete_document    28c64180cc24
5 tool    log_audit          b1509e588127
6 tool    notify_owner       d73966badf3d
```

> *"Every effect, including which three were in flight together. That file
> plus its object store is now a fixture. Everything after this is free."*

### 3. Replay it — and show the world stays untouched

```bash
rm ledger.log
samsara replay run.samsara.jsonl --strict -- node agent.mjs
wc -l ledger.log        # file does not exist
```

> *"Same agent, same answers, no divergence — and nothing was deleted. The
> tools never ran. That is what makes the next part safe."*

This is also the regression check: change a prompt, run it again, and any
behavioural change comes back as a located divergence.

### 4. Break it — every way it can be broken

```bash
samsara sweep run.samsara.jsonl -- node agent.mjs
```

```
1. What was checked
  • 7 faultable positions × 5 fault kinds
  • 35 single-fault schedules (all of them)
  • batch #1: 6 of 6 orderings (all of them)
  • 42 replays total

2. What it means
  ✗ 2 of 35 single faults break it
  ✓ batch #1 is order-independent across all 6 orderings
  ✗ `confidence` at 0.8 changes what the agent does
```

Three results, and it is worth pausing on each.

**The duplicate delete** — the flagship:

```
#4 timeout
  [no_duplicate_effects] `delete_document` took effect at #4 and again
  at #7 with identical arguments — the side effect happened twice
```

> *"A timeout is not a failure. It is an absence of information: the request
> may have been received, executed, and its response lost coming back.
> The agent treats it as 'did not happen' and retries. We know better,
> because we recorded what the call really returned before we hid it."*

**The ordering check passed** — and that matters too:

> *"Three concurrent calls, all six orderings run, no difference. `Promise.all`
> preserves request order, so this agent is genuinely fine. A detector that
> cried wolf here would be worse than none."*

**The threshold** — the one nothing else finds:

```
4. Thresholds
  ✗ effect #0: an effectful call changes when `confidence` moves
    across 0.8 (recorded 0.86)
       at 0.8:       (nothing)
       at 0.8000001: tool:delete_document
```

> *"Every call succeeded. Nothing timed out. A document is deleted or not on
> a difference of one part in a million — finer than the model's own
> precision. And look which side: at exactly 0.8, nothing. That is `>` versus
> `>=`, and no amount of breaking the network would ever have found it."*

### 5. Fix it, and prove the fix

The same agent, with two changes: an idempotency key stable across retries,
and a margin band where it escalates instead of guessing.

```bash
FIXED=1 samsara record --out fixed.samsara.jsonl -- node agent.mjs
FIXED=1 samsara sweep fixed.samsara.jsonl -- node agent.mjs
```

```
2. What it means
  ✓ no single fault breaks this agent — all 35 were checked
  ✓ batch #1 is order-independent across all 6 orderings
```

> *"Not 'we tried a hundred things and nothing broke'. All thirty-five. The
> fault space is finite, and that is the whole of it."*

### 6. Keep it fixed

```bash
FIXED=1 samsara sweep fixed.samsara.jsonl --pairs \
  --out samsara.cert.json -- node agent.mjs
```

Commit `samsara.cert.json`. In CI:

```bash
FIXED=1 samsara sweep fixed.samsara.jsonl --pairs \
  --check samsara.cert.json -- node agent.mjs
```

> *"It fails on a new bug. It also fails when coverage shrinks — if someone
> drops the pair sweep, the verdict stays green and the check still goes red.
> A pass/fail gate would call that no change."*

---

## If you have thirty seconds instead of five minutes

```bash
samsara demo                      # the whole story, no setup at all
```

Or open **https://maher-reven.github.io/samsara/** and click an effect.

## What each beat demonstrates

| Beat | Capability |
|---|---|
| 2 | Recording via proxy — no code change for model calls, a shim for tools |
| 3 | Deterministic replay, strict divergence detection, zero side effects |
| 4 | Exhaustive fault coverage, invariants, concurrency, thresholds |
| 5 | The before/after, and what "exhaustive" buys over "we tried some" |
| 6 | Certificates as a CI gate, including coverage regressions |

## Notes for questions you will get

**"How long does it take?"** About three seconds for 42 replays here. Each is
a real process launch of your agent; the cost is `fork`, not the model.

**"Does it need my API key?"** Only to record once. Everything after replays
from the recording.

**"What if my agent is not JavaScript?"** Model calls work with any language —
the proxy is just the configured base URL. Tool calls need a shim, and only
TypeScript has one today.

**"Is this an eval?"** Yes, of behaviour rather than answers. It will never
tell you the model picked the wrong document. It tells you that when the call
fails, you delete it twice.
