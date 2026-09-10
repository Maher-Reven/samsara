//! The worked example, as a narrative.
//!
//! Everything here runs against the deterministic testkit: no network, no API
//! key, no cost. That is deliberate — a demo that needs credentials is a demo
//! nobody runs.

use samsara_core::prelude::*;
use samsara_core::testkit::{run_agent, run_assembler, Assembly, FakeBackend, Style, WorldAccess};

use crate::ui;

const TARGET: &str = "/var/reports/stale.csv";

fn invariants() -> Vec<Box<dyn Invariant>> {
    vec![
        Box::new(NoDuplicateEffects::only(["delete_file"]).exempting_idempotent("idempotency_key")),
        Box::new(TerminatesWithin(24)),
    ]
}

fn record(style: Style) -> (Trace, MemCas) {
    let mut cas = MemCas::new();
    let mut recorder = Recorder::new(FakeBackend::new(1), &mut cas, Canonicalizer::default());
    run_agent(&mut recorder, style);
    (recorder.finish("delete-stale-report"), cas)
}

pub fn run(seeds: u64, max_faults: usize) -> Result<(), Box<dyn std::error::Error>> {
    println!(
        "\n{}\n{}",
        ui::bold("samsara demo \u{2014} a duplicate side effect, found and minimised"),
        ui::dim("no network, no API key, no cost")
    );

    // -- 1 ------------------------------------------------------------------
    ui::heading(1, "Record a normal run");
    let (trace, mut cas) = record(Style::Naive);
    for event in &trace.events {
        println!("     {}", ui::dim(&event.summary()));
    }
    let clean = check_all(&trace, &cas, &invariants());
    if clean.is_empty() {
        ui::ok(&format!(
            "{} effects, no violations \u{2014} this is the run that passes review",
            trace.len()
        ));
    }

    // -- 2 ------------------------------------------------------------------
    ui::heading(2, "Search for a fault schedule that breaks it");
    ui::info(&format!(
        "trying seeds 0..{seeds}, up to {max_faults} faults each"
    ));

    let Some(finding) = search(&trace, &mut cas, 0..seeds, max_faults, &invariants(), |r| {
        run_agent(r, Style::Naive);
    }) else {
        ui::ok("no seed broke the agent \u{2014} it survived every schedule tried");
        return Ok(());
    };

    ui::bad(&format!(
        "seed {} breaks it: {}",
        ui::bold(&finding.seed.to_string()),
        ui::yellow(&finding.schedule.describe())
    ));
    for v in &finding.violations {
        println!("       {}", ui::red(&v.report()));
    }

    // -- 3 ------------------------------------------------------------------
    ui::heading(3, "Reproduce it");
    let again = evaluate(
        &trace,
        &mut cas,
        finding.schedule.clone(),
        &invariants(),
        |r| {
            run_agent(r, Style::Naive);
        },
    );
    if again == finding.violations {
        ui::ok(&format!(
            "`samsara repro {}` gives byte-identical results, on any machine",
            finding.seed
        ));
    } else {
        ui::bad("non-deterministic \u{2014} this would be a bug in Samsara itself");
    }

    // -- 4 ------------------------------------------------------------------
    ui::heading(4, "Shrink it to the part that matters");
    let shrunk = minimise(&trace, &mut cas, &finding, &invariants(), |r| {
        run_agent(r, Style::Naive);
    });
    ui::info(&format!(
        "{} faults \u{2192} {}, in {} replays",
        shrunk.started_with,
        ui::bold(&shrunk.schedule.len().to_string()),
        shrunk.evaluations
    ));
    ui::ok(&format!(
        "the entire bug is: {}",
        ui::yellow(&shrunk.schedule.describe())
    ));

    // -- 5 ------------------------------------------------------------------
    ui::heading(5, "The counterfactual timeline");
    let branch = {
        let mut replayer = Replayer::new(
            &trace,
            &mut cas,
            Mode::Counterfactual {
                schedule: shrunk.schedule.clone(),
            },
        )?;
        run_agent(&mut replayer, Style::Naive);
        replayer.finish("counterfactual").branch
    };
    for event in &branch.events {
        let line = format!("     {}", event.summary());
        match &event.fault {
            Some(f) => println!(
                "{}  {}",
                ui::yellow(&line),
                ui::yellow(&format!("\u{2190} injected {}", f.label()))
            ),
            None => println!("{}", ui::dim(&line)),
        }
    }
    ui::info("the agent believed the first delete failed, so it tried again");

    // -- 6 ------------------------------------------------------------------
    ui::heading(6, "Confirm the bug is real, not an artefact of replay");
    let mut live_cas = MemCas::new();
    let mut recorder = Recorder::new(
        FakeBackend::new(1).lossy("delete_file", 1),
        &mut live_cas,
        Canonicalizer::default(),
    );
    let report = run_agent(&mut recorder, Style::Naive);
    let deletes = recorder.world().delete_count(TARGET);
    ui::info(&format!(
        "same agent, live backend, one response lost in flight \u{2014} agent reports {}",
        if report.succeeded {
            "success"
        } else {
            "failure"
        }
    ));
    if deletes > 1 {
        ui::bad(&format!("{TARGET} was really deleted {deletes} times"));
    } else {
        ui::ok(&format!("{TARGET} deleted once"));
    }

    // -- 7 ------------------------------------------------------------------
    ui::heading(7, "Apply the fix and re-run the same schedule");
    ui::info("send an idempotency key derived from the task, stable across attempts");
    let (fixed_trace, mut fixed_cas) = record(Style::Idempotent);
    let after = evaluate(
        &fixed_trace,
        &mut fixed_cas,
        shrunk.schedule.clone(),
        &invariants(),
        |r| {
            run_agent(r, Style::Idempotent);
        },
    );
    if after.is_empty() {
        ui::ok(&format!(
            "seed {} no longer reproduces \u{2014} keep it as a regression test",
            finding.seed
        ));
    } else {
        for v in &after {
            ui::bad(&v.report());
        }
    }

    println!(
        "\n{}\n",
        ui::cyan("  \u{2192} samsara bundle <trace> --out bundle.json   opens in the web timeline")
    );
    Ok(())
}

/// Reproduce a failure from its seed.
pub fn repro(seed: u64, fixed: bool) -> Result<(), Box<dyn std::error::Error>> {
    let style = if fixed {
        Style::Idempotent
    } else {
        Style::Naive
    };
    let (trace, mut cas) = record(style);

    let schedule = FaultSchedule::generate(seed, &trace.faultable(), 4);
    println!(
        "{} {}\n{} {}",
        ui::dim("seed    "),
        ui::bold(&seed.to_string()),
        ui::dim("schedule"),
        ui::yellow(&schedule.describe())
    );

    let violations = evaluate(&trace, &mut cas, schedule, &invariants(), |r| {
        run_agent(r, style);
    });

    if violations.is_empty() {
        ui::ok("no violations \u{2014} this seed no longer reproduces");
        Ok(())
    } else {
        for v in &violations {
            ui::bad(&v.report());
        }
        std::process::exit(1);
    }
}

/// Write the demo traces to disk so the other subcommands have something to
/// chew on, and so the web timeline has canned data to open.
pub fn emit(dir: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(dir)?;

    // The clean recording, and a counterfactual branch off it.
    let (trace, mem) = record(Style::Naive);
    let schedule = FaultSchedule::of(vec![FaultPoint {
        seq: trace
            .events
            .iter()
            .find(|e| e.name == "delete_file")
            .map(|e| e.seq)
            .unwrap_or(1),
        fault: Fault::Timeout,
    }]);

    let mut cas = mem;
    let branch = {
        let mut replayer = Replayer::new(
            &trace,
            &mut cas,
            Mode::Counterfactual {
                schedule: schedule.clone(),
            },
        )?;
        run_agent(&mut replayer, Style::Naive);
        replayer
            .finish("counterfactual: delete_file times out")
            .branch
    };

    for (name, t) in [("clean", &trace), ("counterfactual", &branch)] {
        let path = dir.join(format!("{name}.samsara.jsonl"));
        let mut objects = FsCas::open(crate::objects_dir(&path))?;

        // Copy across every payload the trace references, so the pair on
        // disk is self-contained.
        for event in &t.events {
            for digest in [
                Some(&event.request),
                Some(&event.outcome),
                event.shadow.as_ref(),
            ]
            .into_iter()
            .flatten()
            {
                if let Some(bytes) = cas.get(digest)? {
                    objects.put(&bytes)?;
                }
            }
        }

        std::fs::write(&path, t.to_jsonl())?;
        ui::ok(&format!("{} \u{2014} {} effects", path.display(), t.len()));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Scenario: concurrent tool calls
// ---------------------------------------------------------------------------

fn record_assembly(assembly: Assembly) -> (Trace, MemCas, Vec<String>) {
    let mut cas = MemCas::new();
    let mut recorder = Recorder::new(FakeBackend::new(1), &mut cas, Canonicalizer::default());
    let document = run_assembler(&mut recorder, assembly);
    (recorder.finish("assemble-report"), cas, document)
}

/// The quiet bug: three sections rendered concurrently, folded into a
/// document as they arrive.
pub fn order(seeds: u64) -> Result<(), Box<dyn std::error::Error>> {
    println!(
        "\n{}\n{}",
        ui::bold("samsara demo \u{2014} an order-dependent bug in concurrent tool calls"),
        ui::dim("no network, no API key, no cost")
    );

    ui::heading(1, "Record a normal run");
    let (trace, mut cas, document) = record_assembly(Assembly::AsTheyArrive);
    for event in &trace.events {
        let line = event.summary();
        match event.batch {
            Some(b) => println!(
                "     {}  {}",
                ui::dim(&line),
                ui::cyan(&format!("batch {b}"))
            ),
            None => println!("     {}", ui::dim(&line)),
        }
    }
    ui::ok(&format!(
        "document assembled as {}",
        ui::bold(&document.join(" + "))
    ));
    ui::info("three sections were rendered concurrently \u{2014} note the shared batch");

    ui::heading(2, "Nothing is wrong with this run");
    let invariants: Vec<Box<dyn Invariant>> = vec![
        Box::new(NoDuplicateEffects::all()),
        Box::new(TerminatesWithin(24)),
    ];
    if check_all(&trace, &cas, &invariants).is_empty() {
        ui::ok("no duplicate effects, no runaway, every call succeeded");
        ui::info("no invariant can catch this, because no single run is wrong");
    }

    ui::heading(3, "Ask whether the order mattered");
    ui::info(&format!(
        "permuting the batch across {seeds} seeds and comparing behaviour"
    ));

    let Some(finding) = search_order_dependence(&trace, &mut cas, 0..seeds, |replayer| {
        run_assembler(replayer, Assembly::AsTheyArrive);
    }) else {
        ui::ok("no ordering changed what the agent did");
        return Ok(());
    };
    ui::bad(&finding.report());

    ui::heading(4, "See the damage");
    let schedule = FaultSchedule::of(vec![FaultPoint {
        seq: finding.at_seq,
        fault: Fault::Reorder { seed: finding.seed },
    }]);
    let mut replayer = Replayer::new(&trace, &mut cas, Mode::Counterfactual { schedule })?;
    let reordered = run_assembler(&mut replayer, Assembly::AsTheyArrive);
    let result = replayer.finish("reordered");

    println!(
        "     {} {}",
        ui::dim("recorded: "),
        ui::green(&document.join(" + "))
    );
    println!(
        "     {} {}",
        ui::dim("reordered:"),
        ui::red(&reordered.join(" + "))
    );
    ui::info("every call succeeded; the document is simply wrong");
    if !result.holes.is_empty() {
        ui::info(&format!(
            "the save that follows has no recorded answer ({} hole) \u{2014} the agent has \
             left the behaviour we observed, which is itself the finding",
            result.holes.len()
        ));
    }

    ui::heading(5, "Apply the fix");
    ui::info("place each result at the index it was requested from, not where it landed");
    let (fixed, mut fixed_cas, _) = record_assembly(Assembly::ByRequestOrder);
    let still = search_order_dependence(&fixed, &mut fixed_cas, 0..seeds, |replayer| {
        run_assembler(replayer, Assembly::ByRequestOrder);
    });
    match still {
        None => ui::ok(&format!("survives all {seeds} permutations")),
        Some(f) => ui::bad(&f.report()),
    }

    println!();
    Ok(())
}
