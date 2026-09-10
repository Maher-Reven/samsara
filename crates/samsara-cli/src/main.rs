//! `samsara` — deterministic replay and fault injection for LLM agents.

mod bundle;
mod demo;
mod record;
mod ui;

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use samsara_core::prelude::*;

#[derive(Parser, Debug)]
#[command(
    name = "samsara",
    version,
    about = "Deterministic replay and fault injection for LLM agents",
    long_about = "Samsara records every effect an agent performs — model calls, tool calls, \
                  the clock, the RNG — so the run can be replayed offline, forked at any \
                  point, and broken on purpose to see whether the agent survives."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the worked example end to end: find a duplicate side effect,
    /// reproduce it from a seed, and shrink it to one fault.
    ///
    /// Needs no API key and touches no network.
    Demo {
        /// How many seeds to try before giving up.
        #[arg(long, default_value_t = 500)]
        seeds: u64,
        /// Maximum faults per generated schedule.
        #[arg(long, default_value_t = 4)]
        max_faults: usize,
    },

    /// Record an agent run by proxying its model calls.
    ///
    /// Starts a local endpoint, points the child process at it, and writes a
    /// trace when the child exits. No TLS interception and no certificate to
    /// install — Samsara is simply the configured base URL.
    Record {
        /// Where to write the trace.
        #[arg(short, long, default_value = "run.samsara.jsonl")]
        out: PathBuf,
        /// Port for the local endpoint.
        #[arg(long, default_value_t = 8787)]
        port: u16,
        /// The agent command, after `--`.
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },

    /// Reproduce a known failure from its seed.
    ///
    /// Exits non-zero if the failure still reproduces, so it works as a
    /// regression test: add the seed to CI and the build goes red if the bug
    /// comes back.
    Repro {
        /// The seed from a previous `search`.
        seed: u64,
        /// Check the fixed agent instead of the buggy one.
        #[arg(long)]
        fixed: bool,
    },

    /// Write the demo traces to disk, for `show`, `verify` and `bundle`.
    Emit {
        /// Directory to write into.
        #[arg(short, long, default_value = "traces")]
        out: PathBuf,
    },

    /// Print a recorded trace.
    Show {
        /// Path to a `.samsara.jsonl` trace.
        trace: PathBuf,
        /// Also print request and response payloads.
        #[arg(long)]
        payloads: bool,
    },

    /// Check a recorded trace against the built-in invariants.
    Verify {
        trace: PathBuf,
        /// Tools whose effects must not happen twice. Defaults to every tool.
        #[arg(long, value_delimiter = ',')]
        effectful: Vec<String>,
        /// Argument name carrying an idempotency key; such calls are exempt.
        #[arg(long)]
        idempotency_key: Option<String>,
        /// Maximum effects before the run is considered runaway.
        #[arg(long, default_value_t = 256)]
        max_effects: usize,
        /// Maximum model tokens.
        #[arg(long)]
        token_budget: Option<u64>,
    },

    /// Package a trace and its payloads into a single JSON file the web
    /// timeline can open.
    Bundle {
        trace: PathBuf,
        /// Where to write the bundle.
        #[arg(short, long, default_value = "bundle.json")]
        out: PathBuf,
    },
}

fn main() {
    if let Err(e) = run() {
        eprintln!("{} {e}", ui::red("error:"));
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    match Cli::parse().command {
        Command::Demo { seeds, max_faults } => demo::run(seeds, max_faults),
        Command::Record { out, port, command } => record::run(&out, port, &command),
        Command::Repro { seed, fixed } => demo::repro(seed, fixed),
        Command::Emit { out } => demo::emit(&out),
        Command::Show { trace, payloads } => show(&trace, payloads),
        Command::Verify {
            trace,
            effectful,
            idempotency_key,
            max_effects,
            token_budget,
        } => verify(
            &trace,
            effectful,
            idempotency_key,
            max_effects,
            token_budget,
        ),
        Command::Bundle { trace, out } => bundle::write(&trace, &out),
    }
}

/// Load a trace and the CAS that sits beside it.
///
/// Convention: `run.samsara.jsonl` is accompanied by `run.samsara.objects/`.
/// Keeping them adjacent means a trace can be copied around as a pair, and
/// `git add` picks up both without anyone having to remember.
fn load(path: &std::path::Path) -> Result<(Trace, FsCas), Box<dyn std::error::Error>> {
    let file =
        std::fs::File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    let trace = Trace::read(std::io::BufReader::new(file))?;
    let cas = FsCas::open(objects_dir(path))?;
    Ok((trace, cas))
}

pub fn objects_dir(trace_path: &std::path::Path) -> PathBuf {
    let name = trace_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "trace".into());
    let stem = name.strip_suffix(".jsonl").unwrap_or(&name);
    trace_path.with_file_name(format!("{stem}.objects"))
}

fn show(path: &std::path::Path, payloads: bool) -> Result<(), Box<dyn std::error::Error>> {
    let (trace, cas) = load(path)?;

    println!(
        "{}  {}  {} effects",
        ui::bold(&trace.header.label),
        ui::dim(&trace.header.engine),
        trace.len()
    );
    if let Some(schedule) = &trace.header.schedule {
        println!(
            "{} {}",
            ui::dim("  faults:"),
            ui::yellow(&schedule.describe())
        );
    }
    println!();

    for event in &trace.events {
        let line = event.summary();
        println!(
            "{}",
            if event.fault.is_some() {
                ui::yellow(&line)
            } else {
                line
            }
        );

        if let Some(fault) = &event.fault {
            println!(
                "       {} {}",
                ui::dim("injected"),
                ui::yellow(&fault.label())
            );
        }
        if payloads {
            for (label, digest) in [("req", &event.request), ("res", &event.outcome)] {
                if let Ok(Some(bytes)) = cas.get(digest) {
                    let text = String::from_utf8_lossy(&bytes);
                    println!("       {} {}", ui::dim(label), truncate(&text, 160));
                }
            }
        }
    }
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    let flat = s.replace('\n', " ");
    if flat.chars().count() <= max {
        return flat;
    }
    let head: String = flat.chars().take(max).collect();
    format!("{head}\u{2026}")
}

fn verify(
    path: &std::path::Path,
    effectful: Vec<String>,
    idempotency_key: Option<String>,
    max_effects: usize,
    token_budget: Option<u64>,
) -> Result<(), Box<dyn std::error::Error>> {
    let (trace, cas) = load(path)?;

    let mut duplicates = if effectful.is_empty() {
        NoDuplicateEffects::all()
    } else {
        NoDuplicateEffects::only(effectful)
    };
    if let Some(field) = idempotency_key {
        duplicates = duplicates.exempting_idempotent(field);
    }

    let mut invariants: Vec<Box<dyn Invariant>> = vec![
        Box::new(duplicates),
        Box::new(TerminatesWithin(max_effects)),
    ];
    if let Some(budget) = token_budget {
        invariants.push(Box::new(TokenBudget(budget)));
    }

    let violations = check_all(&trace, &cas, &invariants);
    if violations.is_empty() {
        ui::ok(&format!("{} effects, no violations", trace.len()));
        Ok(())
    } else {
        for v in &violations {
            ui::bad(&v.report());
        }
        // A non-zero exit is the point of this command: it is meant to run in
        // CI, where nobody reads the output unless the build goes red.
        std::process::exit(1);
    }
}
