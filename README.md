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

<!-- toc -->

**Using it:** [Who this is for](#who-this-is-for) ·
[How it evaluates](#how-it-evaluates) ·
[Providers](#which-providers) ·
[Install](#install) ·
[API docs](https://maher-reven.github.io/samsara/docs/) ·
[Your first sweep](#your-first-sweep) ·
[Commands](#commands) ·
[What it does not do yet](#what-it-does-not-do-yet)

**How it works:** [The idea](#the-idea) ·
[Architecture](#the-four-pieces) ·
[Canonicalisation](#canonicalisation-why-byte-equality-fails) ·
[Delta debugging](#how-shrinking-works-ddmin) ·
[Exhaustive coverage](#the-part-that-is-actually-different) ·
[Concurrency](#concurrent-tool-calls) ·
[Is replay faithful?](#is-replay-actually-faithful) ·
[Notes from the build](#notes-from-the-build) ·
[Prior art](#prior-art)

## Who this is for

You have an agent that calls tools which **change something** — delete a file,
charge a card, send an email, write to a database — and you cannot answer the
question *"what happens if one of those calls fails at the wrong moment?"*
except by waiting to find out.

Concretely, Samsara is worth your time if:

- Your agent retries failed tool calls. (Almost all do.)
- At least one of your tools is not safe to run twice.
- You have ever seen a bug you could not reproduce.

**It is not for you if** you are measuring answer quality — that is what
[Braintrust][braintrust], [Langfuse][langfuse] and promptfoo do, and they do
it properly. Samsara says nothing about whether your agent gave a good answer.
It says whether it can be made to do something it must never do.

### How it evaluates

Samsara is an eval harness — for **behaviour**, not for output. It subjects
your agent to conditions and returns a verdict, which is what evaluation is.
What it does not do is score answer quality, and every tool that calls itself
an "LLM eval" does exactly that. Same word, different subject.

The method is three things, and they only work together:

**Deterministic replay** turns one recorded run into a fixture you can run a
thousand times for free. **Fault injection** perturbs that fixture in every
way it can be perturbed. **Declared invariants** decide whether the result is
acceptable. Take any one away and the other two are useless: replay without
faults only reproduces what already happened, faults without invariants only
find crashes, and invariants without deterministic replay produce findings
that do not reproduce.

That is verification rather than statistics, and the difference shows up in
what you get back:

| | Scoring ("LLM evals") | Verification (this) |
|---|---|---|
| Input | A dataset of cases | One recorded run |
| Method | Run, judge, aggregate | Replay, perturb, assert |
| Judge | An LLM, or a metric | Invariants over the effect trace |
| Result | "82% on this set" | "no single fault breaks this" |
| Rerun | A different number | Identical, byte for byte |
| A failure is | A score that dropped | A named schedule, minimised to one fault |

**Samsara does not evaluate what your agent said. It evaluates what your agent
did, under conditions you choose.** Those are different axes, and a serious
agent wants both.

Three properties separate this from a scoring eval, and they are the reason
the output is committable:

- **The verdict is deterministic.** Rerun a scoring eval and the number
  moves, so you manage it with thresholds. Rerun this and you get identical
  bytes.
- **The coverage is exhaustive, not sampled.** An eval tells you about the
  cases in your dataset. This tells you there are no more cases — the fault
  space is finite and all of it ran.
- **A failure is a reproducer**, not a metric that dropped: a named schedule,
  minimised to the one fault that matters.

To be unambiguous about one thing: Samsara does not check your evals, run
them, or know they exist. Both tools point at the agent, from different
sides. An agent can score full marks on every eval you own and still charge a
card twice under a timeout — the two measure unrelated properties and can
disagree completely while both being right.

### What you need

| | |
|---|---|
| **Rust 1.88+** | to build the CLI. Nothing is published yet, so you build from source. |
| **Node 20+** | only if you want tool-call coverage, which needs the shim. |
| **An agent that talks HTTP to a provider** | any language. The proxy is the configured base URL, so there is no SDK to swap. |
| **Nothing else** | no API key, no account, no network after recording once. |

### Is it generic?

The model side is: the proxy *is* the configured base URL, so any language
and any SDK that reads `ANTHROPIC_BASE_URL` is captured with no code change.

Everything else depends on what you route through the shim. Samsara can only
see effects it is told about, so an agent that reads a database, a file or the
system clock outside a wrapped call has non-determinism Samsara cannot replay
or perturb. In practice that means wrapping the tools that matter and using
`now()` / `random()` in your retry logic — not a rewrite, but not nothing.

**Samsara evaluates behaviour, not answers** — see
[How it evaluates](#how-it-evaluates). Scoring evals ask whether the answer
was good; this asks whether the agent can be made to do something it must
never do. Different axis, and you probably want both.

Model calls work with any language today. **Tool calls need a shim, and only
TypeScript has one** — a Python port is an afternoon, because the shim carries
no logic, but it does not exist yet. Without a shim you still get model-call
replay and divergence detection; you do not get tool fault injection, which is
where most of the value is.

### Which providers

| | |
|---|---|
| **Anthropic** | works, and is what the canonicaliser's volatile-path defaults are tuned for |
| **OpenAI** | works. Routing is by request path, so `/v1/chat/completions` reaches OpenAI and `/v1/messages` reaches Anthropic, from the same proxy |
| **Gemini, Vertex, Bedrock** | **not intercepted.** Their SDKs do not read `OPENAI_BASE_URL` or `ANTHROPIC_BASE_URL`, so the proxy never sees the call |
| **Anything else** | set `SAMSARA_UPSTREAM` to point the proxy at it — a gateway, a router, a self-hosted model |

Gemini specifically: if you reach it through its OpenAI-compatible endpoint
with an OpenAI client, it is intercepted like any other OpenAI call. Through
the native Google SDK it is not, and adding that means a base-URL override the
SDK actually honours. The engine does not care either way — a model response
is opaque bytes to it — so this is proxy plumbing, not a design limit.

The canonicaliser's default volatile-path list is tuned for Anthropic's
request shape. OpenAI records and replays, but if you see spurious
divergences on fields your SDK regenerates per request, add them to the
volatile list.

[braintrust]: https://braintrust.dev
[langfuse]: https://langfuse.com

## Install

```bash
cargo install --git https://github.com/Maher-Reven/samsara samsara-cli
samsara --help
```

Nothing is published to crates.io or npm, and that is deliberate rather than
unfinished — see [Why it isn't packaged](#why-it-isnt-packaged). Installing
from git is one command either way.

The tool shim installs from the same clone:

```bash
git clone https://github.com/Maher-Reven/samsara
cd samsara/shim/typescript && npm install && npm run build
cd /path/to/your/agent && npm install /path/to/samsara/shim/typescript
```

API docs for the engine are published alongside the demo:
**[maher-reven.github.io/samsara/docs](https://maher-reven.github.io/samsara/docs/)**

## Your first sweep

Five steps, from nothing to *"no single fault breaks this."*

**1. Wrap your tools.** This is the only change to your code, and it is a
no-op unless Samsara is attached, so it is safe to leave in.

```ts
import { wrapTools } from "@samsara/shim";

const tools = wrapTools({
  charge_card:  async (args) => billing.charge(args),
  send_receipt: async (args) => mailer.send(args),
});
```

**2. Declare what must never happen.** `samsara init` writes a template.
Samsara cannot guess which of your tools change the world — that is the one
thing only you know.

```toml
# samsara.toml
[[invariant]]
type = "no_duplicate_effects"
tools = ["charge_card", "send_receipt"]

[[invariant]]
type = "never_after_failure"
tool = "send_receipt"
after = "charge_card"
```

**3. Record one real run.** This is the only step that spends money.

```bash
samsara record --out run.samsara.jsonl -- npm start
```

You now have `run.samsara.jsonl` and a `run.samsara.objects/` beside it.
Commit both — they are your fixture, and everything after this is free.

**4. Break it, exhaustively.**

```bash
samsara sweep run.samsara.jsonl --pairs -- npm start
```

Your agent runs once per fault schedule against recorded responses. No
tokens, no network. A few hundred runs takes seconds.

```
  • 15 single-fault schedules (all of them)
  • 75 pair schedules (all of them)
  ✗ 2 of 15 single faults break it
     #1 timeout
       [no_duplicate_effects] `charge_card` took effect at #1 and again
       at #2 — the side effect happened twice
```

**5. Fix it, then keep it fixed.** Write a certificate and check it in CI:

```bash
samsara sweep run.samsara.jsonl --pairs --out samsara.cert.json -- npm start
# in CI:
samsara sweep run.samsara.jsonl --pairs --check samsara.cert.json -- npm start
```

The check fails on a new bug *and* on coverage quietly shrinking, which a
pass/fail gate would miss. It also fails when you *fix* something —

```
✗ verdict changed: Broken → Clean
✗ failing schedules 22 → 0
```

— because the certificate is now stale and should be re-issued. Like any
snapshot, it is a record of what you last agreed to, not a floor.

## Commands

| | |
|---|---|
| `samsara init` | write a starting `samsara.toml` |
| `samsara record -- <cmd>` | run your agent, capture every effect |
| `samsara sweep <trace> -- <cmd>` | check every fault; write or check a certificate |
| `samsara replay <trace> --strict -- <cmd>` | did my agent change? |
| `samsara replay <trace> --seed N -- <cmd>` | reproduce one specific failure |
| `samsara verify <trace>` | check a recorded run against your properties |
| `samsara show <trace>` | print a trace |
| `samsara bundle <trace>` | package a trace for the [browser timeline](https://maher-reven.github.io/samsara/) |
| `samsara demo` | the worked example, no setup at all |

Every command that can fail exits non-zero, so all of them work as CI gates.

## The part that is actually different

Every tool in this space injects faults at random. They can tell you a bug
exists. None of them can tell you one doesn't, because a sample is not a
proof and five hundred seeds is still a sample.

```
$ samsara sweep --subject fixed --pairs

1. What was checked
  • 3 faultable positions × 5 fault kinds
  • 15 single-fault schedules (all of them)
  • 75 pair schedules (all of them)
  • 90 replays total (90 fault, 0 ordering)

2. What it means
  ✓ no single fault breaks this agent — all 15 were checked;
    no pair does either, across all 75
```

That sentence is the product. It is available for one reason: **a replay
costs nothing.** No tokens, no network, no clock. Exhaustive checking is
normally out of reach because each trial is expensive — here no trial is, so
the entire finite fault space simply runs, in under a second.

Making the space finite is the trick. `Truncate` alone has a parameter, so
the fault space is nominally infinite; collapsing each family to one maximally
destructive representative (`Truncate { keep: 0 }` — survive that and you
survive every larger keep) makes it enumerable, and therefore completable.

The same applies to concurrency. Instead of shuffling a batch two hundred
times and hoping, every ordering runs:

```
  • batch #0: 6 of 6 orderings (all of them)
  ✗ batch #0 behaves differently under 5 of all 6 orderings
  • 0 of 2 adjacent pairs commute
```

### Declare what must never happen

Fault injection only means something if something is watching. The other
tools in this space inject and rely on *your* test suite to notice — which
finds crashes and misses everything an agent does wrong while staying up.

Samsara watches. But what counts as wrong is not something a library can
know: `delete_file` twice is an incident, `search` twice is a waste, and only
the person who wrote the agent can say which of their tools is which. So the
properties are declared in a file that every command reads.

```toml
# samsara.toml
[[invariant]]
type = "no_duplicate_effects"
tools = ["charge_card", "send_email"]
idempotency_key = "idempotency_key"

[[invariant]]
type = "never_after_failure"    # the receipt must not go out
tool = "send_receipt"           # if the charge failed
after = "charge_card"

[[invariant]]
type = "requires"               # nothing is charged without an audit record
tool = "charge_card"
then = "log_audit"

[[invariant]]
type = "max_calls"              # a per-tool ceiling, for the runaway a
tool = "search"                 # whole-run budget is too coarse to see
max = 20
```

`samsara init` writes a starting point. A typo is an error rather than a
silently ignored line, because believing you enforce a property you do not is
the worst outcome available.

| Invariant | Parameters in `samsara.toml` | What it guards |
|---|---|---|
| `no_duplicate_effects` | `tools = [...]`, `idempotency_key = "..."` | Prevents side-effecting tools from executing twice across retries |
| `never_after_failure` | `tool = "..."`, `after = "..."` | Forbids sensitive actions if an earlier prerequisite step failed |
| `requires` | `tool = "..."`, `then = "..."` | Demands a companion action (e.g. audit log) whenever an effect lands |
| `max_calls` | `tool = "..."`, `max = N` | Per-tool rate ceiling, catching runaways a whole-run budget is too coarse to see |
| `terminates_within` | `effects = N` | Caps total steps, catching infinite retry loops |
| `token_budget` | `tokens = N` | Enforces an upper bound on model token consumption |

Then point it at your own agent:

```bash
samsara sweep run.samsara.jsonl --pairs -- npm start
```

```
1. What was checked
  • 3 faultable positions × 5 fault kinds
  • 15 single-fault schedules (all of them)
  • 75 pair schedules (all of them)

2. What it means
  ✗ 2 of 15 single faults break it; 20 pairs do, across all 75

3. Failing schedules
     #1 timeout
       [no_duplicate_effects] `charge_card` took effect at #1 and again
       at #2 with identical arguments — the side effect happened twice
```

Every schedule is a real launch of your agent against replayed responses. No
tokens, no network — the expense is `fork`, not the model.

### The certificate

A claim about coverage is worth nothing if nobody can check it, so `sweep`
writes one:

```json
{
  "trace": "7b8062d5cc30…",
  "invariants": ["no_duplicate_effects", "terminates_within"],
  "coverage": { "singles_checked": 15, "singles_exhaustive": true,
                "pairs_checked": 75, "pairs_exhaustive": true },
  "verdict": "clean",
  "claims": ["no single fault breaks this agent — all 15 were checked; …"]
}
```

It contains no timestamps, durations or hostnames — everything in it is a
function of the trace and the engine version, so two runs produce identical
bytes. That is what makes it worth committing. `samsara sweep --check`
re-runs and exits non-zero on any difference, and the difference worth
catching is not a new failure. It is coverage quietly shrinking while the
verdict stays green:

```
✗ pair coverage 75 → 0
```

A gate that only compared pass/fail would wave that through.
`certificates/` holds this repo's own, and CI re-checks them.

## Try it

```bash
cargo run --bin samsara -- demo               # the worked example above
cargo run --bin samsara -- sweep --pairs      # check the whole fault space
cargo run --bin samsara -- emit               # write demo traces to ./traces
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

Model calls are captured with no code change. The other three you route
through the shim:

```ts
import { wrapTools, now, random } from "@samsara/shim";

const tools = wrapTools({ charge_card: async (a) => billing.charge(a) });

// in your retry loop
const jitter = await random();
const wakeAt = (await now()) + base * jitter;
```

`now()` and `random()` are async because the value comes from the engine, and
they are opt-in rather than a global patch of `Date.now`: a testing tool that
silently rewrites time for every library in your process is a worse bargain
than an `await`. Both fall through to the real thing when Samsara is not
attached.

**A clock read you do not route is invisible**, and invisible non-determinism
is the one thing replay cannot paper over. If your backoff uses bare
`Date.now()`, replay still works — the effects all match — but the *timing* is
whatever the wall clock says, so a bug that depends on it will not reproduce
reliably.

**Zero OS entropy is a compile-time guarantee.** Samsara builds `rand` with
`default-features = false` to deliberately drop `getrandom`. The engine never
queries system entropy; every pseudo-random draw is derived deterministically
from ChaCha8 via an explicit seed. That compile-time guarantee eliminates
invisible variance across machines and CI runners, and is what allows the
entire engine to compile to `wasm32-unknown-unknown` for the browser timeline.

### The four pieces

```
           ┌───────────────────────┐
           │   Agent Under Test    │
           └───────────┬───────────┘
                       │ Effects trait
       ┌───────────────┼───────────────┬──────────────┐
       ▼               ▼               ▼              ▼
   Model Call      Tool Call         Clock           RNG
  (Proxy/Shim)  (Side Effects)     (now_ms)      (next_f64)
       │               │               │              │
       └───────────────┼───────────────┴──────────────┘
                       ▼
           ┌───────────────────────┐
           │     Canonicalizer     │  Redacts volatile fields
           │      (canon.rs)       │  Deterministic key order
           └───────────┬───────────┘
                       ▼
           ┌───────────────────────┐
           │  Content-Addressable  │  BLAKE3-keyed CAS
           │      Store (CAS)      │  Deduplicated payloads
           └───────────┬───────────┘
                       ▼
           ┌───────────────────────┐
           │     Replay Engine     │  • Strict: replay(record(r)) == r
           │   & ResponseOracle    │  • Counterfactual: serve retries by identity
           └───────────┬───────────┘
                       ▼
           ┌───────────────────────┐
           │   Invariant Checker   │  Evaluates Landing (Yes/Maybe/No)
           │    & Shadow Truth     │  using recorded shadows
           └───────────┬───────────┘
                       ▼
           ┌───────────────────────┐
           │   Delta Debugging     │  ddmin: reduces noisy schedules
           │     (shrink.rs)       │  to 1-minimal culprits
           └───────────┬───────────┘
                       ▼
           ┌───────────────────────┐
           │      Certificate      │  Deterministic, timestamp-free
           │   (Git Regression)    │  coverage verdict
           └───────────────────────┘
```

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

### Canonicalisation: why byte equality fails

On replay, the engine must decide whether the effect the agent is asking for
*now* is the same effect it asked for when recorded. Comparing raw JSON or
request bytes fails immediately:
- Provider SDKs stamp fresh request identifiers (`tool_call_id`, `request_id`).
- Wall-clock timestamps leak into payloads (`created_at`, `timestamp`).
- JSON object key orders vary across serialisers and language runtimes.

Samsara runs requests through `Canonicalizer`: volatile paths (such as
`messages.*.tool_call_id`, `request_id`, `timestamp`) are substituted with a
fixed redaction marker, and object keys are lexicographically sorted before
computing a BLAKE3 digest. The exact redaction path set is preserved in the
trace header, ensuring replay and recording evaluate identity under identical
rules.

### A timeout is not a failure

It is an *absence of information*. The request may have been received,
executed, and its response lost coming back. Treating a timeout as "did not
happen" is precisely the mistake the agent makes, so an invariant that made the
same mistake would never catch it. Samsara resolves every call to **landed /
maybe landed / did not land**, and because an injected fault records a *shadow*
— the outcome it suppressed — a counterfactual can say the side effect happened
twice with certainty rather than hedging.

### How shrinking works (`ddmin`)

Finding a failure with random fault injection is straightforward: search seeds
until an invariant breaks. But a random schedule often contains four or five
faults, several of which are irrelevant noise that happened not to alter the
outcome. A bug report with five simultaneous faults is hard to understand and
harder to fix.

Samsara runs **delta debugging** (`ddmin`, Zeller & Hildebrandt 2002) over
failing schedules:

1. It partitions the fault schedule into subsets.
2. It replays the agent against each subset to test whether the invariant still
   breaks.
3. It recursively narrows the schedule until it is **1-minimal**: every
   remaining fault is load-bearing, and removing any single one makes the
   failure disappear.

This turns a multi-fault finding like:

```text
#1 delay(410ms), #2 error(503), #3 truncate(12B), #5 timeout
```

into:

```text
#2 error(503)
```

Delta debugging is only possible because Samsara’s replay is strictly
deterministic. Against a live API, network latency and model jitter make test
outcomes non-deterministic, corrupting the minimization predicate and causing
delta debugging to discard the real culprit. Because Samsara replays offline
against recorded effects, each shrink step takes milliseconds and the result is
repeatable forever.

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

The package is not on npm yet, so install it from a clone — see
[Install](#install). `wrapTools` is a no-op when `SAMSARA_ENDPOINT` is unset,
so it costs nothing in production and is safe to leave in permanently. Then check a trace in CI — it exits non-zero on a violation:

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

## Why it isn't packaged

`cargo install --git` is already a one-line install, and `cargo doc` on
GitHub Pages already gives rendered API docs. Those are the two things
publishing would actually buy.

What publishing would also buy: a permanent claim on a good name, and a
versioning obligation to a userbase of zero. A crate on crates.io is a
promise that someone maintains it. Making that promise before anyone has
asked is how the registry fills with abandoned 0.1.0s.

The names are free and will be claimed the day someone wants to depend on
this. Until then the cost of `--git` is one flag.

## What it does not do yet

Stated plainly, because a README that only lists strengths is not worth reading:

- **An injected timeout surfaces as an error, not as latency.** Replay answers
  immediately with the error rather than making the caller wait, so a bug that
  depends on wall-clock deadline handling — rather than on the failure itself
  — is out of reach.
- **Streaming responses are recorded whole.** Chunk boundaries are not
  preserved, so mid-stream truncation faults are unavailable for model calls.
- **A wide batch is sampled, not enumerated.** Beyond seven concurrent calls
  the ordering space is 5040 runs and the check reports a sample, saying so
  rather than implying completeness.
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
                       shrinking, invariants, batch scheduling, exhaustive
                       coverage, certificates, declared properties
crates/samsara-cli     the `samsara` binary: init, record, replay, sweep,
                       verify, and end-to-end tests driving it against a
                       stub provider
certificates/          committed coverage claims, re-checked by CI
crates/samsara-wasm    WebAssembly bindings for the timeline
shim/typescript        the tool-side shim  (12 tests)
web/                   the browser timeline, and a contract test pinning
                       every JSON field the page reads  (7 tests)
```

## Prior art

Most of what Samsara does has been done before, and it is worth being
specific about which parts.

**Record/replay of LLM calls is solved.** [`standin`][standin] records and
replays LLM API calls across every major provider, with streaming, tool calls
and secret redaction. So do [`vcr-langchain`][vcrlc], [ReqCassette][req], and
the whole [VCR][vcr] lineage behind them. Samsara's proxy and trace format
cover ground these already cover well.

**Deterministic simulation testing is decades old.** [FoundationDB][fdb],
[Antithesis][antithesis], [madsim and turmoil][dst] — seeded fault schedules,
single-threaded simulation, reproduce-from-seed. Samsara borrows the method
wholesale; the FDB write-up is the best introduction to it.

**Chaos injection for agents already ships.** [`agent-chaos`][agentchaos] does
LLM and tool faults, composable and targetable. [AgentChaos][agentchaospaper]
makes the same architectural call Samsara does — inject at the shared HTTP
layer so no source changes are needed. [AgentCheck][agentcheck]'s shared
response cache is close to the identity-indexed oracle here.

Three things I could not find anywhere:

1. **Exhaustive bounded coverage** — enumerating the whole single-fault space
   and saying so, rather than sampling it. Everything above searches randomly.
   Paired with declared properties, the claim becomes one nothing else in this
   space can make: *you say what must never happen, and Samsara reports
   whether any single fault can cause it.* The injectors have no assertions;
   the observability platforms do not inject; the cassette libraries do not
   search. This needs all three.
2. **The landing model and shadow outcome** — the retry/idempotency problem is
   widely recognised, but asserting a duplicate side effect *with certainty*,
   by recording the outcome a fault suppressed, is not something I found.
3. **Order-dependence as a relation between two runs**, with each batch's
   internal order discarded before comparing so correct agents are not flagged.

Even those are a novel combination rather than a novel invention.

[standin]: https://pypi.org/project/standin/
[vcrlc]: https://github.com/amosjyng/vcr-langchain
[req]: https://github.com/lostbean/req_cassette
[vcr]: https://github.com/vcr/vcr
[rr]: https://rr-project.org/
[fdb]: https://apple.github.io/foundationdb/testing.html
[antithesis]: https://antithesis.com
[dst]: https://github.com/ivanyu/awesome-deterministic-simulation-testing
[agentchaos]: https://github.com/deepankarm/agent-chaos
[agentchaospaper]: https://arxiv.org/abs/2608.06790
[agentcheck]: https://arxiv.org/html/2607.11098

## License

MIT or Apache-2.0, at your option.
