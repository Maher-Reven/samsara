# Samsara

[![ci](https://github.com/Maher-Reven/samsara/actions/workflows/ci.yml/badge.svg)](https://github.com/Maher-Reven/samsara/actions/workflows/ci.yml)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](#license)

**Deterministic replay and fault injection for LLM agents.**
Your agent failed once. Make it fail on demand.

**[→ Break an agent in your browser](https://maher-reven.github.io/samsara/)** — the
real engine compiled to WebAssembly. Click any effect to fault it and watch
the two timelines diverge. No install, no API key, nothing sent anywhere.

---

A tool call times out. The agent retries. But the first call *succeeded* — the
file was already deleted, the customer was already charged. The agent does it
twice.

In production this happens one run in a few hundred, and you will never
reproduce it. With Samsara it is `samsara repro 91238`: deterministic, on any
machine, in CI, forever.

```
$ samsara demo          # no API key, no network, no cost

1. Record a normal run
         0 model   claude-sonnet-4          93c3d5f0689b
         1 tool    delete_file              7cb5ff61e051
         2 model   claude-sonnet-4          50717eb66f10
  ✓ 3 effects, no violations — this is the run that passes review

2. Search for a fault schedule that breaks it
  • trying seeds 0..500, up to 4 faults each
  ✗ seed 0 breaks it: #0 error(503), #1 error(503), #2 duplicate
       [no_duplicate_effects] at effect #4: `delete_file` took effect at #1
       and again at #4 with identical arguments — the side effect happened twice

4. Shrink it to the part that matters
  • 3 faults → 1, in 4 replays
  ✓ the entire bug is: #1 error(503)

5. The counterfactual timeline
         0 model   claude-sonnet-4          93c3d5f0689b
     !   1 tool    delete_file              7cb5ff61e051  ← injected error(503)
         2 random  next_f64                 281bd384c205
         3 clock   now_ms                   88dfd2b65a56
         4 tool    delete_file              7cb5ff61e051
         5 model   claude-sonnet-4          50717eb66f10
  • the agent believed the first delete failed, so it tried again

6. Confirm the bug is real, not an artefact of replay
  • same agent, live backend, one response lost in flight — agent reports success
  ✗ /var/reports/stale.csv was really deleted 2 times

7. Apply the fix and re-run the same schedule
  ✓ seed 0 no longer reproduces — keep it as a regression test
```

## Try it

```bash
cargo run --bin samsara -- demo     # the worked example above
cargo run --bin samsara -- emit     # write demo traces to ./traces
cargo run --bin samsara -- show traces/counterfactual.samsara.jsonl
```

The [browser timeline](https://maher-reven.github.io/samsara/) runs the same
engine. Drop a bundle onto it to inspect one of your own recordings:

```bash
samsara bundle run.samsara.jsonl --out bundle.json
```

A loaded trace can be inspected but not forked, and that is a real
distinction rather than a missing feature: forking means re-running the agent
under different conditions, and a bundle contains the recording, not the
program that produced it. Counterfactuals live in `samsara replay`, where the
agent is present.

To build the page locally:

```bash
make web && python3 -m http.server -d web 8000
```

## The idea

An agent run is deterministic apart from the effects it performs. Record every
effect and you can re-run the agent offline, feeding it the recorded answers.
Three things follow:

1. **Replay.** A production failure runs again on your laptop, with no API key
   and no cost.
2. **Counterfactuals.** Replay faithfully to step *n*, inject a fault, and let
   the agent run free. Does it recover, or corrupt its state?
3. **Search.** A fault schedule is generated from a `u64`, so "is this agent
   robust?" becomes a search over seeds, and a failure is one integer.

### What counts as an effect

Model calls, tool calls, **the clock, and the RNG**.

The last two are the ones people forget, and they are exactly the ones that
matter. Retry backoff is a jittered sleep computed from a clock reading and a
random draw. Leave those uncontrolled and the retry bug — the whole reason you
are here — is unreproducible.

### The four pieces

| | |
|---|---|
| **Recording proxy** | `samsara record -- npm start` points the child's `ANTHROPIC_BASE_URL` at a local endpoint. **No TLS interception**: we are simply the configured base URL and make the upstream call ourselves, so there is no CA certificate to install. |
| **Trace format** | JSONL — one header line, one line per effect — with payloads content-addressed by BLAKE3 into a sibling `.objects/` directory. Readable in a diff, because traces end up in pull requests as regression fixtures. |
| **Tool shim** | ~150 lines of TypeScript. Contains no policy at all: it asks the engine what to do and does that, so a shim can never drift from the engine. It reports one thing the engine cannot observe — which calls were issued concurrently. |
| **Replay engine** | Two modes. *Strict* asserts the agent asks exactly the same questions in the same order. *Counterfactual* replays to the fork point, injects a fault, then lets the agent run free. |

### The trick that makes offline counterfactuals work

Once the agent has been lied to, it starts asking things the recording never
anticipated, and positional matching is finished. The obvious answers are "call
the real API again" (expensive, nondeterministic, defeats the purpose) or "give
up".

Samsara does neither. It indexes every effect by *identity* — kind, name, and
canonicalised arguments — and serves post-fork effects from that index. Which
works because of one observation:

> **A retry has the same identity as the call it retries.**

Time out `delete_file(path=/x)` and the agent tries again. The oracle already
knows what `delete_file(path=/x)` returns, because it watched it succeed thirty
milliseconds ago. So it answers, the agent proceeds happily — and the file has
now been deleted twice. That is the bug, reproduced offline, from a seed, with
no API key.

### A timeout is not a failure

It is an *absence of information*. The request may have been received,
executed, and its response lost coming back. Treating a timeout as "did not
happen" is precisely the mistake the agent makes, so an invariant that made the
same mistake would never catch it. Samsara resolves every call to **landed /
maybe landed / did not land**, and because an injected fault records a *shadow*
— the outcome it suppressed — a counterfactual can say the side effect happened
twice with certainty rather than hedging.

## Is replay actually faithful?

Everything above is worthless if it isn't, so it is a property test rather than
a claim:

```rust
replay(record(run)) ≡ run
```

Asserted over 256 generated configurations per run, against real agent code,
with no network and no API key — the fake model, tools, clock and RNG all live
in the testkit so the suite runs in CI for free. Alongside it: replay performs
zero backend calls, replay is repeatable, a swapped agent is *detected* rather
than tolerated, and an agent that stops early is caught.

## Attaching it to your agent

```bash
samsara record --out run.samsara.jsonl -- npm start
```

Model calls are captured with no code change. For tool calls, wrap them:

```ts
import { wrapTools } from "@samsara/shim";

const tools = wrapTools({
  delete_file: async ({ path }) => fs.rm(path),
});
```

`wrapTools` is a no-op when `SAMSARA_ENDPOINT` is unset, so it costs nothing in
production. Then check a trace in CI — it exits non-zero on a violation:

```bash
samsara verify run.samsara.jsonl \
  --effectful delete_file,send_email,charge_card \
  --idempotency-key idempotency_key
```

## Concurrent tool calls

Some bugs are not failures. Every call succeeds, nothing retries, no budget is
exceeded — and the agent still produces the wrong answer, because two results
came back in the other order.

```
$ samsara demo --scenario order

1. Record a normal run
         0 model   claude-sonnet-4          ab737d1cec25
         1 tool    render_section           13e9bd8f2ee7  batch 0
         2 tool    render_section           2d3e94c1d8e0  batch 0
         3 tool    render_section           5f37c58803e0  batch 0
         4 tool    save_document            4e7c1c229df8
  ✓ document assembled as <intro> + <summary> + <appendix>

2. Nothing is wrong with this run
  ✓ no duplicate effects, no runaway, every call succeeded
  • no invariant can catch this, because no single run is wrong

3. Ask whether the order mattered
  ✗ concurrent batch #0 is order-dependent — reordering it with seed 0
    changes effect #4
      in request order: tool:save_document:4e7c1c229df8
      reordered:        tool:save_document:5ee530d71bc6

4. See the damage
     recorded:  <intro> + <summary> + <appendix>
     reordered: <appendix> + <summary> + <intro>

5. Apply the fix
  ✓ survives all 200 permutations
```

**Testing order-dependence does not need threads, it needs control over
order** — and real concurrency gives you the opposite. So `perform_batch` runs
the calls one at a time and *chooses* the sequence, which is what a
deterministic simulator does and why a finding reproduces from a seed.

Against a real agent the same thing happens over HTTP. The shim reports which
calls it issued together — only the agent's own process can know that, since
"concurrently in flight" and "back to back" are indistinguishable to a server
— and on replay the proxy holds the batch at a barrier, sorts it into the
order the recording used, applies the permutation, and hands the responses
back one at a time. The
model is faithful precisely where it matters: an agent that collects results
and sorts them by index is unaffected by any permutation, and correctly so; an
agent that folds each result into shared state as it lands is not.

This cannot be an invariant, because order-dependence is not a property of one
run — it is a relation between two. No single trace is wrong; the pair
disagrees. So Samsara runs the agent with calls completing in request order,
runs it again permuted, and compares what it *did afterwards*, with each
batch's internal order discarded (otherwise every well-behaved agent would
look guilty).

## Replaying it

Feed a recorded trace back into your real agent process. Nothing leaves the
machine and nothing is spent, because every answer comes from the recording.

**Did I change anything?**

```bash
samsara replay run.samsara.jsonl --strict -- npm start
```

Strict replay asserts the agent asks the same questions in the same order.
Edit a prompt, reorder a tool list, bump an SDK, then run this: a behavioural
change becomes a located divergence naming the field that moved, rather than a
vague sense that something is different. Exits non-zero if anything diverged.

**Does it survive adversity?**

```bash
samsara replay run.samsara.jsonl --seed 91238 \
  --effectful delete_file -- npm start
```

Injects the fault schedule that seed generates and checks the resulting run
for violations. During replay the shim never executes the real tool, so
reproducing a delete-twice bug does not delete anything twice. Add the seed to
CI and the build goes red if the bug comes back.

## What it does not do yet

Stated plainly, because a README that only lists strengths is not worth reading:

- **An injected timeout surfaces as an error, not as latency.** Replay answers
  immediately with the error rather than making the caller wait, so a bug that
  depends on wall-clock deadline handling — rather than on the failure itself
  — is out of reach.
- **Streaming responses are recorded whole.** Chunk boundaries are not
  preserved, so mid-stream truncation faults are unavailable for model calls.
- **The batch barrier waits, bounded.** To reorder concurrent calls the proxy
  must hold early arrivals until the rest of their burst appears — but a
  counterfactual agent may have diverged and may never issue them. So the wait
  expires after two seconds and schedules whatever turned up, with a warning.
  The alternative is a tool that deadlocks on exactly the runs it exists to
  investigate.
- **Samsara guarantees the scheduled order, not the agent's.** Responses are
  handed back one at a time, so a real agent observes results in the chosen
  order. Once they are on the wire, the agent's own runtime decides what it
  does with them; the trace records what Samsara scheduled.
- **The oracle is content-addressed, not state-aware.** A stateful tool
  (`stat` before and after a write) returns its recorded answers in order and
  then holds the last one, which is right for retries and wrong for polling.
- **One provider shape.** Anthropic-style requests; the canonicaliser's default
  volatile-path set is tuned for it.
- **TypeScript shim only.** Python is a weekend, not a rewrite — the shim
  carries no logic.

## Notes from the build

Four bugs the test suite found that I would not have found by reading:

**Floats do not survive a round trip.** The metamorphic property failed with no
divergences and no holes — replay was behaviourally perfect but its trace
hashed differently. Cause: a jitter value of `0.09466872426617445` came back
from `parse → Value → serialise` one ULP lighter. The fix is architectural
rather than numerical: *a digest must be taken from bytes that were preserved,
never from bytes that were regenerated.* The replayer already had the exact
recorded bytes and was throwing them away.

**Identity needs more than arguments.** The demo showed `clock` and `random`
sharing a digest — both have empty bodies. The same collision means
`delete_file{path:"/x"}` and `create_file{path:"/x"}` are indistinguishable,
and since the oracle answers by identity, it would silently serve one effect's
response to a completely different effect. Identity is now over kind, name,
*and* arguments.

**A test that matches its own fixture is not a test.** The first end-to-end
test asserted that `samsara show` output contained `delete_file`, and it
passed with tool recording ripped out of the proxy — because `show` prints the
trace label, the label is the agent command, and the command string contains
`delete_file`. Mutation testing caught it; nothing else would have. The tests
now assert against event rows only.

**Faults must be keyed to the branch, not the recording.** Keying them to
recorded position seems natural and is wrong: the first injected fault knocks
the run off-script, after which "recorded effect #7" corresponds to nothing the
agent is doing, and every later fault silently fails to fire. Multi-fault
schedules became meaningless and the shrinker had nothing to shrink. Branch
position stays well defined after the fork, and says what you actually mean:
*fail the fourth call this agent makes.*

## Layout

```
crates/samsara-core    engine: trace, CAS, canonicalisation, replay, faults,
                       shrinking, invariants, batch scheduling  (68 tests)
crates/samsara-cli     the `samsara` binary: record, replay, verify, and
                       end-to-end tests driving it against a stub provider
                       (14 tests)
crates/samsara-wasm    WebAssembly bindings for the timeline
shim/typescript        the tool-side shim  (12 tests)
web/                   the browser timeline, and a contract test pinning
                       every JSON field the page reads  (7 tests)
```

## Prior art

Record/replay debugging is old and well understood — [`rr`][rr] for native
code, [FoundationDB's deterministic simulation][fdb] for distributed systems.
Applying it to agents is recent: [AgentRR][agentrr], [AgentCheck][agentcheck]
and [Causal Agent Replay][car] all explore the same territory from the research
side. Samsara is the engineering counterpart — a tool you can point at your own
agent this afternoon.

[rr]: https://rr-project.org/
[fdb]: https://apple.github.io/foundationdb/testing.html
[agentrr]: https://arxiv.org/pdf/2505.17716
[agentcheck]: https://arxiv.org/html/2607.11098
[car]: https://arxiv.org/pdf/2606.08275

## License

MIT or Apache-2.0, at your option.
