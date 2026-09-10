//! End-to-end tests for the attach path.
//!
//! The engine has a hundred tests and the thing that connects it to a real
//! agent had none. That asymmetry is the dangerous kind: someone reads the
//! README, believes it, runs `samsara record` against their agent, and if
//! that first contact fails they never reach the tested part.
//!
//! So these tests drive the **real binary** as a subprocess, against a stub
//! upstream standing in for the provider, with an agent that speaks the wire
//! protocol exactly as a real one would. Nothing here is mocked except the
//! provider itself, and no API key is needed.

use samsara_core::prelude::{Fault, FaultSchedule};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// A port nothing is listening on.
///
/// Binding to 0 and immediately releasing races with anyone else doing the
/// same, which is why the proxy is given a few attempts below rather than one.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("can bind a loopback port")
        .local_addr()
        .expect("bound socket has an address")
        .port()
}

/// A stand-in for the model provider.
///
/// Counts what it served, so a test can assert that replay performed *no*
/// upstream calls — the claim that replay is free is worth checking rather
/// than assuming.
struct Upstream {
    port: u16,
    calls: Arc<AtomicUsize>,
    _thread: std::thread::JoinHandle<()>,
}

impl Upstream {
    fn start() -> Upstream {
        let server = tiny_http::Server::http("127.0.0.1:0").expect("stub upstream binds");
        let port = server.server_addr().to_ip().expect("ip addr").port();
        let calls = Arc::new(AtomicUsize::new(0));

        let counter = Arc::clone(&calls);
        let thread = std::thread::spawn(move || {
            for mut request in server.incoming_requests() {
                let mut body = Vec::new();
                let _ = request.as_reader().read_to_end(&mut body);
                counter.fetch_add(1, Ordering::SeqCst);

                // A response shaped like a real one, including a usage block
                // so the token-budget invariant has something to read.
                let payload = br#"{"id":"msg_stub","content":[{"type":"text","text":"delete it"}],"usage":{"input_tokens":120,"output_tokens":40}}"#;
                let response = tiny_http::Response::from_data(payload.to_vec()).with_header(
                    tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
                        .unwrap(),
                );
                let _ = request.respond(response);
            }
        });

        Upstream {
            port,
            calls,
            _thread: thread,
        }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn served(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

fn samsara() -> Command {
    Command::new(env!("CARGO_BIN_EXE_samsara"))
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("samsara-e2e-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// An agent, as far as the proxy can tell: one model call over HTTP and one
/// tool call over the shim protocol.
///
/// Written in `sh` with `curl` on purpose. A Node agent would test the real
/// TypeScript shim but would drag a build step into `cargo test`; the shim is
/// covered separately by its own suite. What this exercises is the part with
/// no other coverage — the proxy's HTTP surface and the exact bytes on the
/// wire.
const AGENT: &str = r#"
set -e
curl -sS -X POST "$ANTHROPIC_BASE_URL/v1/messages" \
  -H 'content-type: application/json' \
  -H 'x-api-key: test-key-not-a-secret' \
  -d '{"model":"claude-sonnet-4","messages":[{"role":"user","content":"remove the stale report"}]}' \
  > /dev/null

DECISION=$(curl -sS -X POST "$SAMSARA_ENDPOINT/begin" \
  -H 'content-type: application/json' \
  -d '{"name":"delete_file","body":{"path":"/var/reports/stale.csv"}}')
case "$DECISION" in
  *'"action":"return"'*)
    # Replay: the engine already knows the answer, so the tool must not run.
    ;;
  *'"action":"execute"'*)
    # Recording: do the work, then report it. The side effect is a file, so
    # a test can assert on whether the world was really touched.
    #
    # `call` correlates this result with its begin; without it two concurrent
    # calls would claim each other's outcomes.
    CALL=$(printf '%s' "$DECISION" | sed -n 's/.*"call":"\([^"]*\)".*/\1/p')
    echo "deleted" >> "$SIDE_EFFECT_LOG"
    curl -sS -X POST "$SAMSARA_ENDPOINT/end" \
      -H 'content-type: application/json' \
      -d "{\"call\":\"$CALL\",\"outcome\":{\"status\":\"ok\",\"value\":{\"ok\":true,\"path\":\"/var/reports/stale.csv\"}}}" \
      > /dev/null
    ;;
  *)
    echo "unexpected decision: $DECISION" >&2; exit 3 ;;
esac
"#;

/// Run `samsara record` around a shell agent. Returns the trace path.
fn record(dir: &Path, upstream: &Upstream, agent: &str) -> PathBuf {
    let out = dir.join("run.samsara.jsonl");

    // The proxy binds a port we picked a moment ago; retry if we lost the race.
    for attempt in 0..4 {
        let status = samsara()
            .args(["record", "--out"])
            .arg(&out)
            .args(["--port", &free_port().to_string()])
            .arg("--")
            .args(["sh", "-c", agent])
            .env("SAMSARA_UPSTREAM", upstream.url())
            .env("SIDE_EFFECT_LOG", dir.join("side-effects.log"))
            .output()
            .expect("the samsara binary runs");

        if status.status.success() && out.exists() {
            return out;
        }
        if attempt == 3 {
            panic!(
                "record failed after 4 attempts\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&status.stdout),
                String::from_utf8_lossy(&status.stderr)
            );
        }
    }
    unreachable!()
}

fn read_trace(path: &Path) -> serde_json::Value {
    let text = std::fs::read_to_string(path).expect("trace is readable");
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    serde_json::json!({
        "header": serde_json::from_str::<serde_json::Value>(lines[0]).expect("header parses"),
        "events": lines[1..]
            .iter()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).expect("event parses"))
            .collect::<Vec<_>>(),
    })
}

// ---------------------------------------------------------------------------

#[test]
fn recording_captures_both_a_model_call_and_a_tool_call() {
    let dir = scratch("capture");
    let upstream = Upstream::start();
    let trace_path = record(&dir, &upstream, AGENT);

    let trace = read_trace(&trace_path);
    let events = trace["events"].as_array().expect("events array");

    assert_eq!(
        events.len(),
        2,
        "one model call and one tool call: {events:#?}"
    );

    assert_eq!(events[0]["kind"], "model");
    assert_eq!(
        events[0]["name"], "claude-sonnet-4",
        "the model name is lifted out of the request body"
    );
    assert_eq!(events[1]["kind"], "tool");
    assert_eq!(events[1]["name"], "delete_file");

    // Sequence numbers must be contiguous or the trace will not load.
    assert_eq!(events[0]["seq"], 0);
    assert_eq!(events[1]["seq"], 1);

    assert_eq!(
        upstream.served(),
        1,
        "exactly one call reached the provider"
    );
}

#[test]
fn the_recorded_trace_loads_back_through_the_cli() {
    let dir = scratch("roundtrip");
    let upstream = Upstream::start();
    let trace_path = record(&dir, &upstream, AGENT);

    // `show` is the cheapest proof that the trace and its object store are a
    // coherent pair on disk — it resolves every payload out of the CAS.
    let out = samsara()
        .arg("show")
        .arg(&trace_path)
        .arg("--payloads")
        .output()
        .expect("show runs");

    assert!(
        out.status.success(),
        "show failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);

    // Assert against the event rows only, never the whole output.
    //
    // The first version of this test searched all of stdout for
    // "delete_file" and passed even with tool recording ripped out of the
    // proxy — because `show` prints the trace label, and the label is the
    // agent command, which contains the string "delete_file". Mutation
    // testing caught it. A test that matches its own fixture text is not a
    // test.
    let rows: Vec<&str> = text
        .lines()
        .skip_while(|l| !l.trim_start().starts_with('0'))
        .collect();
    assert!(!rows.is_empty(), "no event rows in output:\n{text}");
    let rows = rows.join("\n");

    assert!(
        rows.contains("tool") && rows.contains("delete_file"),
        "the tool effect must appear as an event row:\n{rows}"
    );
    assert!(
        rows.contains("/var/reports/stale.csv"),
        "payloads must resolve out of the object store:\n{rows}"
    );
    assert!(
        rows.contains("claude-sonnet-4"),
        "the model effect must appear too:\n{rows}"
    );
}

#[test]
fn payloads_are_written_to_the_sibling_object_store() {
    let dir = scratch("objects");
    let upstream = Upstream::start();
    let trace_path = record(&dir, &upstream, AGENT);

    let objects = dir.join("run.samsara.objects");
    assert!(objects.is_dir(), "object store sits beside the trace");

    // Two effects, each with a request and an outcome. Four payloads, unless
    // two happen to be byte-identical — content addressing would dedupe them.
    let count = walk(&objects);
    assert!(
        (3..=4).contains(&count),
        "expected 3-4 objects, found {count}"
    );

    // Every digest the trace names must resolve.
    let trace = read_trace(&trace_path);
    for event in trace["events"].as_array().unwrap() {
        for field in ["request", "outcome"] {
            let digest = event[field].as_str().expect("digest is a string");
            let path = objects.join(&digest[..2]).join(&digest[2..]);
            assert!(path.exists(), "dangling digest {digest} for {field}");
        }
    }
}

fn walk(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| {
                    if e.path().is_dir() {
                        walk(&e.path())
                    } else {
                        1
                    }
                })
                .sum()
        })
        .unwrap_or(0)
}

#[test]
fn verify_passes_a_clean_recording_and_fails_a_duplicated_one() {
    let dir = scratch("verify");
    let upstream = Upstream::start();

    // One delete: clean.
    let clean = record(&dir, &upstream, AGENT);
    let out = samsara()
        .arg("verify")
        .arg(&clean)
        .args(["--effectful", "delete_file"])
        .output()
        .expect("verify runs");
    assert!(
        out.status.success(),
        "a single delete must pass: {}",
        String::from_utf8_lossy(&out.stdout)
    );

    // The same delete twice, as a retry would produce.
    let dir = scratch("verify-dup");
    let doubled = AGENT.to_string() + &AGENT.replace("set -e", "");
    let bad = record(&dir, &upstream, &doubled);

    let out = samsara()
        .arg("verify")
        .arg(&bad)
        .args(["--effectful", "delete_file"])
        .output()
        .expect("verify runs");

    assert!(
        !out.status.success(),
        "a duplicated side effect must exit non-zero, which is how CI catches it"
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("no_duplicate_effects"),
        "and must say which invariant broke: {text}"
    );
}

#[test]
fn an_upstream_error_is_recorded_rather_than_swallowed() {
    let dir = scratch("upstream-down");

    // No stub at all: the proxy cannot reach the provider.
    let out = dir.join("run.samsara.jsonl");
    let agent = r#"curl -sS -X POST "$ANTHROPIC_BASE_URL/v1/messages" \
        -H 'content-type: application/json' \
        -d '{"model":"claude-sonnet-4","messages":[]}' > /dev/null || true"#;

    let status = samsara()
        .args(["record", "--out"])
        .arg(&out)
        .args(["--port", &free_port().to_string()])
        .arg("--")
        .args(["sh", "-c", agent])
        .env(
            "SAMSARA_UPSTREAM",
            format!("http://127.0.0.1:{}", free_port()),
        )
        .output()
        .expect("binary runs");

    assert!(
        status.status.success(),
        "the proxy survives an unreachable provider"
    );

    let trace = read_trace(&out);
    let events = trace["events"].as_array().unwrap();
    assert_eq!(
        events.len(),
        1,
        "the failed call is still an effect and must be recorded"
    );
    assert_eq!(events[0]["kind"], "model");
}

#[test]
fn the_child_exit_code_does_not_lose_the_trace() {
    let dir = scratch("child-fails");
    let upstream = Upstream::start();
    let out = dir.join("run.samsara.jsonl");

    // An agent that does real work and then crashes — the common case when
    // you are recording precisely because something is going wrong.
    let agent = format!("{AGENT}\nexit 42\n");

    let result = samsara()
        .args(["record", "--out"])
        .arg(&out)
        .args(["--port", &free_port().to_string()])
        .arg("--")
        .args(["sh", "-c", &agent])
        .env("SAMSARA_UPSTREAM", upstream.url())
        .env("SIDE_EFFECT_LOG", dir.join("side-effects.log"))
        .output()
        .expect("binary runs");

    assert!(
        out.exists(),
        "a crashing agent must still leave a trace behind: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let trace = read_trace(&out);
    assert_eq!(trace["events"].as_array().unwrap().len(), 2);
}

// ---------------------------------------------------------------------------
// Replay
// ---------------------------------------------------------------------------

/// How many times the agent really performed its side effect.
fn side_effects(dir: &Path) -> usize {
    std::fs::read_to_string(dir.join("side-effects.log"))
        .map(|s| s.lines().filter(|l| !l.trim().is_empty()).count())
        .unwrap_or(0)
}

/// Run `samsara replay`. Returns its combined output and whether it succeeded.
fn replay(dir: &Path, trace: &Path, extra: &[&str], agent: &str) -> (String, bool) {
    for attempt in 0..4 {
        let out = samsara()
            .arg("replay")
            .arg(trace)
            .args(["--port", &free_port().to_string()])
            .args(extra)
            .arg("--")
            .args(["sh", "-c", agent])
            .env("SIDE_EFFECT_LOG", dir.join("side-effects.log"))
            // No SAMSARA_UPSTREAM on purpose: a replay that reaches for a
            // provider should fail loudly, not quietly succeed.
            .env_remove("SAMSARA_UPSTREAM")
            .output()
            .expect("the samsara binary runs");

        let text = String::from_utf8_lossy(&out.stdout).to_string()
            + &String::from_utf8_lossy(&out.stderr);
        // Distinguish "lost the port race" from "replay reported a failure".
        if out.status.success() || !text.contains("cannot bind") {
            return (text, out.status.success());
        }
        if attempt == 3 {
            panic!("replay never got a port: {text}");
        }
    }
    unreachable!()
}

#[test]
fn strict_replay_of_an_unchanged_agent_finds_no_divergence() {
    let dir = scratch("replay-strict");
    let upstream = Upstream::start();
    let trace = record(&dir, &upstream, AGENT);

    assert_eq!(
        side_effects(&dir),
        1,
        "recording really performed the effect"
    );
    assert_eq!(upstream.served(), 1);
    std::fs::remove_file(dir.join("side-effects.log")).ok();

    let (text, ok) = replay(&dir, &trace, &["--strict"], AGENT);

    assert!(ok, "an unchanged agent must replay cleanly:\n{text}");
    assert!(text.contains("no divergence"), "{text}");

    // The two claims that make replay worth having at all.
    assert_eq!(
        side_effects(&dir),
        0,
        "replay must not perform the side effect — this is the entire point"
    );
    assert_eq!(
        upstream.served(),
        1,
        "replay must not call the provider: still just the one recording call"
    );
}

#[test]
fn strict_replay_catches_an_agent_that_changed() {
    let dir = scratch("replay-changed");
    let upstream = Upstream::start();
    let trace = record(&dir, &upstream, AGENT);

    // The same agent, now deleting a different file — the shape of a real
    // regression, where an edit quietly changes a tool argument.
    let changed = AGENT.replace("/var/reports/stale.csv", "/var/reports/OTHER.csv");
    let (text, ok) = replay(&dir, &trace, &["--strict"], &changed);

    assert!(!ok, "a changed agent must fail strict replay:\n{text}");
    assert!(text.contains("divergence"), "{text}");
    assert!(
        text.contains("path"),
        "the report must name the field that changed:\n{text}"
    );
}

#[test]
fn a_fault_makes_the_agent_retry_and_duplicate_its_side_effect() {
    let dir = scratch("replay-fault");
    let upstream = Upstream::start();

    // An agent that retries once on a failed tool call, written the way real
    // agents are: a failure is treated as "it did not happen".
    let retrying = r#"
curl -sS -X POST "$ANTHROPIC_BASE_URL/v1/messages" \
  -H 'content-type: application/json' \
  -d '{"model":"claude-sonnet-4","messages":[{"role":"user","content":"remove the stale report"}]}' \
  > /dev/null

attempt() {
  DECISION=$(curl -sS -X POST "$SAMSARA_ENDPOINT/begin" \
    -H 'content-type: application/json' \
    -d '{"name":"delete_file","body":{"path":"/var/reports/stale.csv"}}')
  case "$DECISION" in
    *'"status":"err"'*) return 1 ;;
    *'"action":"return"'*) return 0 ;;
    *)
      CALL=$(printf '%s' "$DECISION" | sed -n 's/.*"call":"\([^"]*\)".*/\1/p')
      echo "deleted" >> "$SIDE_EFFECT_LOG"
      curl -sS -X POST "$SAMSARA_ENDPOINT/end" \
        -H 'content-type: application/json' \
        -d "{\"call\":\"$CALL\",\"outcome\":{\"status\":\"ok\",\"value\":{\"ok\":true}}}" > /dev/null
      return 0 ;;
  esac
}
attempt || attempt
"#;

    let trace = record(&dir, &upstream, retrying);
    assert_eq!(
        side_effects(&dir),
        1,
        "the recording deleted it exactly once"
    );

    // Now fault the tool call: the agent is told it failed and tries again,
    // against a call that had already taken effect.
    //
    // The seed is computed rather than hardcoded. A literal here silently
    // stops testing anything the moment the generator's weights change --
    // which is exactly what happened when reordering was added.
    let parsed = read_trace(&trace);
    let events = parsed["events"].as_array().unwrap();
    let faultable: Vec<u64> = events
        .iter()
        .filter(|e| e["kind"] == "model" || e["kind"] == "tool")
        .map(|e| e["seq"].as_u64().unwrap())
        .collect();
    let delete_seq = events
        .iter()
        .find(|e| e["name"] == "delete_file")
        .and_then(|e| e["seq"].as_u64())
        .expect("the recording deletes something");

    let seed = (0..5000u64)
        .find(|seed| {
            // A fault that *masks* a call which really succeeded. Truncation
            // and reordering do not make the agent retry.
            matches!(
                FaultSchedule::generate(*seed, &faultable, 4).at(delete_seq),
                Some(Fault::Timeout) | Some(Fault::Error { .. })
            )
        })
        .expect("some seed masks the delete");

    let (text, ok) = replay(
        &dir,
        &trace,
        &[
            "--seed",
            &seed.to_string(),
            "--max-faults",
            "4",
            "--effectful",
            "delete_file",
        ],
        retrying,
    );

    assert!(
        !ok,
        "a duplicated side effect must exit non-zero so CI catches it:\n{text}"
    );
    assert!(
        text.contains("no_duplicate_effects"),
        "and must name the invariant that broke:\n{text}"
    );
}

#[test]
fn replay_writes_a_branch_that_records_its_parent() {
    let dir = scratch("replay-branch");
    let upstream = Upstream::start();
    let trace = record(&dir, &upstream, AGENT);
    let branch = dir.join("branch.samsara.jsonl");

    let (text, _) = replay(&dir, &trace, &["--out", branch.to_str().unwrap()], AGENT);
    assert!(branch.exists(), "branch trace was not written:\n{text}");

    let parsed = read_trace(&branch);
    assert!(!parsed["events"].as_array().unwrap().is_empty());
    assert_eq!(
        parsed["header"]["parent"].as_str().map(|s| s.len()),
        Some(64),
        "a branch must record the trace it forked from"
    );
}

// ---------------------------------------------------------------------------
// Concurrency over the wire
// ---------------------------------------------------------------------------

/// An agent that fires three tool calls at once and appends each result to a
/// file as it lands — genuinely concurrent, three real processes racing.
///
/// It reports a batch id the way the shim does, because only the agent's own
/// process can know which calls it issued together.
const CONCURRENT_AGENT: &str = r#"
curl -sS -X POST "$ANTHROPIC_BASE_URL/v1/messages" \
  -H 'content-type: application/json' \
  -d '{"model":"claude-sonnet-4","messages":[{"role":"user","content":"assemble"}]}' \
  > /dev/null

section() {
  DECISION=$(curl -sS -X POST "$SAMSARA_ENDPOINT/begin" \
    -H 'content-type: application/json' \
    -d "{\"name\":\"render_section\",\"body\":{\"name\":\"$1\"},\"batch\":1}")
  case "$DECISION" in
    *'"action":"return"'*)
      # Replay: record the order the engine released us in.
      echo "$1" >> "$SIDE_EFFECT_LOG" ;;
    *)
      CALL=$(printf '%s' "$DECISION" | sed -n 's/.*"call":"\([^"]*\)".*/\1/p')
      echo "$1" >> "$SIDE_EFFECT_LOG"
      curl -sS -X POST "$SAMSARA_ENDPOINT/end" \
        -H 'content-type: application/json' \
        -d "{\"call\":\"$CALL\",\"outcome\":{\"status\":\"ok\",\"value\":{\"text\":\"<$1>\"}}}" \
        > /dev/null ;;
  esac
}

section intro &
section summary &
section appendix &
wait
"#;

#[test]
fn the_proxy_groups_genuinely_concurrent_calls_into_a_batch() {
    let dir = scratch("concurrent-record");
    let upstream = Upstream::start();
    let trace_path = record(&dir, &upstream, CONCURRENT_AGENT);

    let trace = read_trace(&trace_path);
    let events = trace["events"].as_array().unwrap();

    let batched: Vec<&serde_json::Value> =
        events.iter().filter(|e| e.get("batch").is_some()).collect();
    assert_eq!(batched.len(), 3, "three calls raced: {events:#?}");
    assert!(
        batched.iter().all(|e| e["batch"] == batched[0]["batch"]),
        "and they share one batch id"
    );

    // The model call came first and alone, so it must not be batched.
    let model = events.iter().find(|e| e["kind"] == "model").unwrap();
    assert!(model.get("batch").is_none(), "a lone call is not a batch");
}

#[test]
fn a_lone_tool_call_is_not_recorded_as_a_batch() {
    // The shim assigns an id to every call because it cannot know at begin
    // time whether anything will join. Singletons must be cleared, or every
    // sequential trace would claim a concurrency that never happened.
    let dir = scratch("lone-call");
    let upstream = Upstream::start();
    let agent = AGENT.replace(
        r#"-d '{"name":"delete_file","body":{"path":"/var/reports/stale.csv"}}'"#,
        r#"-d '{"name":"delete_file","body":{"path":"/var/reports/stale.csv"},"batch":1}'"#,
    );
    let trace_path = record(&dir, &upstream, &agent);

    let trace = read_trace(&trace_path);
    assert!(
        trace["events"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e.get("batch").is_none()),
        "no batch survived: {:#?}",
        trace["events"]
    );
}

#[test]
fn replay_holds_concurrent_calls_and_releases_them_in_a_chosen_order() {
    let dir = scratch("concurrent-replay");
    let upstream = Upstream::start();
    let trace_path = record(&dir, &upstream, CONCURRENT_AGENT);

    // Assert on the *branch trace*, not on the agent's log.
    //
    // The branch is what Samsara guarantees and what every downstream
    // analysis reads. The order three separate shell processes manage to
    // append to a file after their responses land is their business, not
    // ours — responses are handed out one at a time so a real agent observes
    // the chosen order, but a race between OS processes after that point is
    // outside anything a proxy can promise.
    let order_for = |seed: u64| -> Vec<String> {
        let branch = dir.join(format!("branch-{seed}.samsara.jsonl"));
        let (text, _) = replay(
            &dir,
            &trace_path,
            &[
                "--seed",
                &seed.to_string(),
                "--max-faults",
                "2",
                "--out",
                branch.to_str().unwrap(),
            ],
            CONCURRENT_AGENT,
        );
        assert!(
            !text.contains("timed out"),
            "the barrier must assemble the batch, not time out:\n{text}"
        );

        let parsed = read_trace(&branch);
        let order: Vec<String> = parsed["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["name"] == "render_section")
            .map(|e| e["identity"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(order.len(), 3, "all three were scheduled:\n{text}");
        order
    };

    let first = order_for(3);
    for attempt in 0..3 {
        assert_eq!(
            order_for(3),
            first,
            "seed 3 must schedule the batch identically every run (attempt {attempt})"
        );
    }

    // And the schedule genuinely controls it: a seed that puts a scheduling
    // fault on the batch must change the order the batch is performed in.
    //
    // The seed is computed rather than guessed. Reordering is one fault kind
    // among seven and has to land on one exact position, so scanning a
    // handful of seeds and hoping is a flaky test; asking the generator
    // which seeds qualify is not.
    let recorded = read_trace(&trace_path);
    let events = recorded["events"].as_array().unwrap();

    let faultable: Vec<u64> = events
        .iter()
        .filter(|e| e["kind"] == "model" || e["kind"] == "tool")
        .map(|e| e["seq"].as_u64().unwrap())
        .collect();
    let batch_start = events
        .iter()
        .find(|e| e.get("batch").is_some())
        .and_then(|e| e["seq"].as_u64())
        .expect("the recording has a batch");

    let seed = (0..5000u64)
        .find(|seed| {
            FaultSchedule::generate(*seed, &faultable, 2)
                .at(batch_start)
                .is_some_and(Fault::is_scheduling)
        })
        .expect("some seed schedules a reordering of the batch");

    let recorded_order: Vec<String> = events
        .iter()
        .filter(|e| e["name"] == "render_section")
        .map(|e| e["identity"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(recorded_order.len(), 3);

    assert_ne!(
        order_for(seed),
        recorded_order,
        "seed {seed} places a reordering on the batch and must change its order"
    );
}

#[test]
fn the_barrier_gives_up_rather_than_hanging() {
    // The one place Samsara can hang. A counterfactual agent may diverge and
    // never issue the calls the barrier is waiting for, so the wait is
    // bounded and the run completes with a warning instead of wedging.
    let dir = scratch("barrier-timeout");
    let upstream = Upstream::start();
    let trace_path = record(&dir, &upstream, CONCURRENT_AGENT);

    // An agent that issues only one of the three calls the recording holds.
    let short = r#"
curl -sS -X POST "$ANTHROPIC_BASE_URL/v1/messages" \
  -H 'content-type: application/json' \
  -d '{"model":"claude-sonnet-4","messages":[{"role":"user","content":"assemble"}]}' \
  > /dev/null
curl -sS -X POST "$SAMSARA_ENDPOINT/begin" \
  -H 'content-type: application/json' \
  -d '{"name":"render_section","body":{"name":"intro"},"batch":1}' > /dev/null
"#;

    let started = std::time::Instant::now();
    let (text, _) = replay(&dir, &trace_path, &[], short);
    let elapsed = started.elapsed();

    assert!(
        text.contains("timed out"),
        "the barrier must report giving up:\n{text}"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(20),
        "it must actually give up, took {elapsed:?}"
    );
}
