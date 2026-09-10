//! The recording proxy.
//!
//! # Why a proxy, and why not a real one
//!
//! Samsara attaches to an agent by being the thing it talks to. `samsara
//! record -- npm start` starts a local HTTP server, points the child
//! process's `ANTHROPIC_BASE_URL` (and the OpenAI equivalent) at it, and
//! forwards everything upstream while writing it down.
//!
//! Note what this is *not*: it is not an interposing proxy that terminates
//! TLS. There is no CA certificate to generate, no root store to poison, no
//! `NODE_EXTRA_CA_CERTS`. We are simply the configured endpoint, and we make
//! the upstream HTTPS call ourselves. The entire category of "install our
//! certificate to continue" is avoided, which is the difference between a
//! tool people try and a tool people abandon in the first five minutes.
//!
//! # The tool side
//!
//! The proxy sees model calls. It cannot see tools, because tools usually run
//! in the agent's own process. So there is a second, tiny protocol for those,
//! spoken by a ~100-line shim in the agent's language:
//!
//! ```text
//! POST /_samsara/begin  {kind, name, body}
//!   -> {"action": "execute"}              # record mode: go and do it
//!   -> {"action": "return", "outcome":..} # replay mode: here is the answer
//!
//! POST /_samsara/end    {outcome}
//!   -> {"outcome": ...}                   # possibly faulted; surface this
//! ```
//!
//! Keeping the decision server-side means the shim contains no policy at all
//! — it cannot drift from the engine, and porting it to another language is
//! an afternoon rather than a project.

use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use samsara_core::prelude::*;
use serde_json::{json, Value};

use crate::ui;

/// Shared state between the HTTP threads.
struct Session {
    cas: FsCas,
    canon: Canonicalizer,
    events: Vec<Event>,
    /// The effect currently in flight on the tool protocol, if any.
    pending: Option<EffectRequest>,
    logical_time: u64,
}

impl Session {
    /// Record a completed effect and return the outcome to surface.
    fn record(&mut self, request: &EffectRequest, outcome: &Outcome) -> std::io::Result<()> {
        let identity = request.identity(&self.canon);
        let request_digest = self.cas.put(&serde_json::to_vec(request)?)?;
        let outcome_digest = self.cas.put(&serde_json::to_vec(outcome)?)?;

        self.events.push(Event {
            seq: self.events.len() as u64,
            kind: request.kind,
            name: request.name.clone(),
            identity,
            request: request_digest,
            outcome: outcome_digest,
            fault: None,
            shadow: None,
            logical_time: self.logical_time,
        });
        self.logical_time += 1;
        Ok(())
    }
}

/// Run the proxy, spawn the child command, and write a trace when it exits.
pub fn run(out: &Path, port: u16, command: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if command.is_empty() {
        return Err("nothing to run: pass the agent command after `--`".into());
    }

    let objects = crate::objects_dir(out);
    let session = Arc::new(Mutex::new(Session {
        cas: FsCas::open(&objects)?,
        canon: Canonicalizer::default(),
        events: Vec::new(),
        pending: None,
        logical_time: 0,
    }));

    let server = tiny_http::Server::http(("127.0.0.1", port))
        .map_err(|e| format!("cannot bind 127.0.0.1:{port}: {e}"))?;
    let base = format!("http://127.0.0.1:{port}");

    ui::ok(&format!("proxy listening on {base}"));
    ui::info(&format!("objects in {}", objects.display()));

    // Serve until the child exits.
    let stop = Arc::new(AtomicU64::new(0));
    let serving = {
        let session = Arc::clone(&session);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            for request in server.incoming_requests() {
                if stop.load(Ordering::Relaxed) == 1 {
                    break;
                }
                if let Err(e) = handle(request, &session) {
                    eprintln!("{} {e}", ui::yellow("proxy:"));
                }
            }
        })
    };

    // The child talks to us instead of to the provider.
    let status = std::process::Command::new(&command[0])
        .args(&command[1..])
        .env("ANTHROPIC_BASE_URL", &base)
        .env("OPENAI_BASE_URL", format!("{base}/v1"))
        .env("SAMSARA_ENDPOINT", format!("{base}/_samsara"))
        .status()
        .map_err(|e| format!("cannot run `{}`: {e}", command[0]))?;

    stop.store(1, Ordering::Relaxed);
    // Unblock the accept loop with a throwaway request.
    let _ = ureq::get(&format!("{base}/_samsara/ping"))
        .timeout(std::time::Duration::from_millis(250))
        .call();
    let _ = serving.join();

    let session = session.lock().unwrap();
    let trace = Trace {
        header: TraceHeader {
            label: command.join(" "),
            canonicalizer: session.canon.clone(),
            recorded_at_ms: now_ms(),
            ..TraceHeader::default()
        },
        events: session.events.clone(),
    };
    std::fs::write(out, trace.to_jsonl())?;

    ui::ok(&format!(
        "{} \u{2014} {} effects (child exited {})",
        out.display(),
        trace.len(),
        status.code().unwrap_or(-1)
    ));
    Ok(())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn handle(
    mut request: tiny_http::Request,
    session: &Arc<Mutex<Session>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = request.url().to_string();
    let method = request.method().as_str().to_string();

    let mut body = Vec::new();
    request.as_reader().read_to_end(&mut body)?;

    let response = if let Some(rest) = url.strip_prefix("/_samsara") {
        tool_protocol(rest, &body, session)?
    } else {
        forward(&method, &url, &body, request.headers(), session)?
    };

    request.respond(response)?;
    Ok(())
}

fn json_response(status: u16, value: Value) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    let body = serde_json::to_vec(&value).unwrap_or_default();
    tiny_http::Response::from_data(body)
        .with_status_code(status)
        .with_header(
            tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
                .expect("static header"),
        )
}

/// The tool-side protocol.
fn tool_protocol(
    path: &str,
    body: &[u8],
    session: &Arc<Mutex<Session>>,
) -> Result<tiny_http::Response<std::io::Cursor<Vec<u8>>>, Box<dyn std::error::Error>> {
    match path {
        "/ping" => Ok(json_response(200, json!({"ok": true}))),

        "/begin" => {
            let incoming: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
            let request = EffectRequest {
                kind: EffectKind::Tool,
                name: incoming
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
                body: incoming.get("body").cloned().unwrap_or(Value::Null),
            };
            session.lock().unwrap().pending = Some(request);
            // Recording: the shim goes and does the work.
            Ok(json_response(200, json!({"action": "execute"})))
        }

        "/end" => {
            let incoming: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
            let outcome: Outcome = incoming
                .get("outcome")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_else(|| Outcome::ok(incoming.clone()));

            let mut session = session.lock().unwrap();
            if let Some(request) = session.pending.take() {
                session.record(&request, &outcome)?;
            }
            Ok(json_response(200, json!({"outcome": outcome})))
        }

        other => Ok(json_response(
            404,
            json!({"error": format!("no such endpoint: {other}")}),
        )),
    }
}

/// Forward a model call upstream, recording both halves.
fn forward(
    method: &str,
    url: &str,
    body: &[u8],
    headers: &[tiny_http::Header],
    session: &Arc<Mutex<Session>>,
) -> Result<tiny_http::Response<std::io::Cursor<Vec<u8>>>, Box<dyn std::error::Error>> {
    let upstream_base = std::env::var("SAMSARA_UPSTREAM")
        .unwrap_or_else(|_| "https://api.anthropic.com".to_string());
    let target = format!("{}{}", upstream_base.trim_end_matches('/'), url);

    let mut call = ureq::request(method, &target);
    for header in headers {
        let name = header.field.as_str().as_str();
        // Host must name the upstream, and hop-by-hop headers must not be
        // relayed. Everything else — crucially the API key — passes through
        // untouched: the proxy authenticates as the caller, and never stores
        // credentials.
        if name.eq_ignore_ascii_case("host")
            || name.eq_ignore_ascii_case("content-length")
            || name.eq_ignore_ascii_case("connection")
            || name.eq_ignore_ascii_case("accept-encoding")
        {
            continue;
        }
        call = call.set(name, header.value.as_str());
    }

    let parsed_request: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let model = parsed_request
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    let result = if body.is_empty() {
        call.call()
    } else {
        call.send_bytes(body)
    };

    let (status, response_body) = match result {
        Ok(response) => {
            let status = response.status();
            let mut text = String::new();
            response.into_reader().read_to_string(&mut text)?;
            (status, text)
        }
        // A provider error is data, not a failure of the proxy: the agent
        // must see it, and the trace must contain it.
        Err(ureq::Error::Status(status, response)) => {
            let mut text = String::new();
            response.into_reader().read_to_string(&mut text)?;
            (status, text)
        }
        Err(e) => (502, json!({"error": e.to_string()}).to_string()),
    };

    let outcome = if (200..300).contains(&status) {
        Outcome::ok(
            serde_json::from_str(&response_body).unwrap_or(Value::String(response_body.clone())),
        )
    } else {
        Outcome::err(status.to_string(), response_body.clone())
    };

    session
        .lock()
        .unwrap()
        .record(&EffectRequest::model(model, parsed_request), &outcome)?;

    Ok(tiny_http::Response::from_data(response_body.into_bytes())
        .with_status_code(status)
        .with_header(
            tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
                .expect("static header"),
        ))
}
