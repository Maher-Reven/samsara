//! Packaging a trace for the web timeline.
//!
//! The viewer is a static page with no server behind it, so a trace has to
//! arrive as one self-contained file: the event log plus every payload it
//! references. Payloads are JSON text, so they embed as strings rather than
//! base64 — the bundle stays readable, and `jq` still works on it.

use std::collections::BTreeMap;
use std::path::Path;

use samsara_core::prelude::*;
use serde::Serialize;

use crate::ui;

#[derive(Serialize)]
struct Bundle<'a> {
    /// Bundle schema version, independent of the trace format version.
    bundle_version: u32,
    trace: &'a Trace,
    /// digest -> payload, as text.
    objects: BTreeMap<String, String>,
}

pub fn write(trace_path: &Path, out: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let file = std::fs::File::open(trace_path)
        .map_err(|e| format!("cannot open {}: {e}", trace_path.display()))?;
    let trace = Trace::read(std::io::BufReader::new(file))?;
    let cas = FsCas::open(crate::objects_dir(trace_path))?;

    let mut objects = BTreeMap::new();
    let mut missing = 0usize;

    for event in &trace.events {
        // `shadow` is deliberately included: the timeline shows what a fault
        // suppressed, which is most of the value of looking at a branch.
        let referenced = [
            Some(&event.request),
            Some(&event.outcome),
            event.shadow.as_ref(),
        ];
        for digest in referenced.into_iter().flatten() {
            if objects.contains_key(digest.as_str()) {
                continue;
            }
            match cas.get(digest)? {
                Some(bytes) => {
                    objects.insert(
                        digest.to_string(),
                        String::from_utf8_lossy(&bytes).into_owned(),
                    );
                }
                None => missing += 1,
            }
        }
    }

    let bundle = Bundle {
        bundle_version: 1,
        trace: &trace,
        objects,
    };
    let json = serde_json::to_vec(&bundle)?;
    std::fs::write(out, &json)?;

    ui::ok(&format!(
        "{} \u{2014} {} effects, {} payloads, {} KB",
        out.display(),
        trace.len(),
        bundle.objects.len(),
        json.len() / 1024
    ));
    if missing > 0 {
        // Worth saying loudly: a bundle with holes renders a timeline with
        // blank rows, and the reader will assume the tool is broken.
        ui::bad(&format!(
            "{missing} payload(s) not found in {} \u{2014} the timeline will show gaps",
            crate::objects_dir(trace_path).display()
        ));
    }
    Ok(())
}
