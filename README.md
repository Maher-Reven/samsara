# Samsara

[![ci](https://github.com/Maher-Reven/samsara/actions/workflows/ci.yml/badge.svg)](https://github.com/Maher-Reven/samsara/actions/workflows/ci.yml)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](#license)

**Deterministic replay and fault injection for LLM agents.**
Your agent failed once. Make it fail on demand.

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

Or open the browser timeline — click any effect to break it and watch the two
timelines diverge. It runs the real engine compiled to WebAssembly, so nothing
in it is a mock-up:

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
| **Tool shim** | ~100 lines of TypeScript. Contains no policy at all: it asks the engine what to do and does that, so a shim can never drift from the engine. |
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

## What it does not do yet

Stated plainly, because a README that only lists strengths is not worth reading:

- **Streaming responses are recorded whole.** Chunk boundaries are not
  preserved, so mid-stream truncation faults are unavailable for model calls.
- **Parallel tool calls are not scheduled.** Effects are a linear sequence;
  there is no deterministic interleaving of concurrent calls, so
  reordering bugs are out of reach. This is the largest gap.
- **The oracle is content-addressed, not state-aware.** A stateful tool
  (`stat` before and after a write) returns its recorded answers in order and
  then holds the last one, which is right for retries and wrong for polling.
- **One provider shape.** Anthropic-style requests; the canonicaliser's default
  volatile-path set is tuned for it.
- **TypeScript shim only.** Python is a weekend, not a rewrite — the shim
  carries no logic.

## Notes from the build

Three bugs the test suite found that I would not have found by reading:

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
                       shrinking, invariants  (59 tests)
crates/samsara-cli     the `samsara` binary
crates/samsara-wasm    WebAssembly bindings for the timeline
shim/typescript        the tool-side shim
web/                   the browser timeline
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
