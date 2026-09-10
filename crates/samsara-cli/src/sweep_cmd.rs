//! `samsara sweep` — check everything, then say what "everything" was.
//!
//! Random fault injection answers "is there a bug?". It can never answer
//! "is there no bug?", because a sample is not a proof and five hundred
//! seeds is still a sample.
//!
//! Exhaustive checking is normally unaffordable: it costs one trial per
//! point in the space, and trials are expensive. Not here. A replay makes no
//! network call, spends no tokens and reads no clock, so the entire finite
//! fault space runs in under a second — and the claim changes from "we found
//! this" to "there is nothing else to find, within these bounds".
//!
//! The bounds are the point, which is why they end up in a certificate
//! rather than in a sentence someone has to trust.

use std::path::{Path, PathBuf};

use samsara_core::prelude::*;
use samsara_core::testkit::{run_agent, run_assembler, Assembly, FakeBackend, Style};

use crate::ui;

/// Which worked example to sweep.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Subject {
    /// The retrying agent and its duplicate-delete bug.
    Retry,
    /// The assembling agent and its order dependence.
    Order,
    /// The fixed versions of both.
    Fixed,
}

fn invariants() -> Vec<Box<dyn Invariant>> {
    vec![
        Box::new(NoDuplicateEffects::only(["delete_file"]).exempting_idempotent("idempotency_key")),
        Box::new(TerminatesWithin(24)),
    ]
}

fn invariant_names() -> Vec<String> {
    invariants().iter().map(|i| i.name().to_string()).collect()
}

pub fn run(
    subject: Subject,
    pairs: bool,
    max_replays: usize,
    out: Option<PathBuf>,
    check: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let certificate = match subject {
        Subject::Retry => sweep_retry(Style::Naive, pairs, max_replays),
        Subject::Fixed => sweep_retry(Style::Idempotent, pairs, max_replays),
        Subject::Order => sweep_order(Assembly::AsTheyArrive, pairs, max_replays),
    };

    report(&certificate);

    if let Some(path) = &out {
        std::fs::write(path, certificate.to_json())?;
        ui::ok(&format!("certificate written to {}", path.display()));
    }

    if let Some(path) = &check {
        return compare(&certificate, path);
    }
    Ok(())
}

fn sweep_retry(style: Style, pairs: bool, max_replays: usize) -> Certificate {
    let mut cas = MemCas::new();
    let mut recorder = Recorder::new(FakeBackend::new(1), &mut cas, Canonicalizer::default());
    run_agent(&mut recorder, style);
    let trace = recorder.finish("delete-stale-report");

    let coverage = sweep(&trace, &mut cas, &invariants(), max_replays, pairs, |r| {
        run_agent(r, style);
    });
    let orders = interleavings(&trace, &mut cas, |r| {
        run_agent(r, style);
    });

    Certificate::new(
        trace.id(),
        trace.header.label.clone(),
        invariant_names(),
        coverage,
        orders,
    )
}

fn sweep_order(assembly: Assembly, pairs: bool, max_replays: usize) -> Certificate {
    let mut cas = MemCas::new();
    let mut recorder = Recorder::new(FakeBackend::new(1), &mut cas, Canonicalizer::default());
    run_assembler(&mut recorder, assembly);
    let trace = recorder.finish("assemble-report");

    let coverage = sweep(&trace, &mut cas, &invariants(), max_replays, pairs, |r| {
        run_assembler(r, assembly);
    });
    let orders = interleavings(&trace, &mut cas, |r| {
        run_assembler(r, assembly);
    });

    Certificate::new(
        trace.id(),
        trace.header.label.clone(),
        invariant_names(),
        coverage,
        orders,
    )
}

fn report(certificate: &Certificate) {
    let c = &certificate.coverage;

    println!(
        "\n{}  {}",
        ui::bold(&certificate.label),
        ui::dim(certificate.trace.short())
    );

    ui::heading(1, "What was checked");
    ui::info(&format!(
        "{} faultable positions \u{d7} {} fault kinds",
        c.positions, c.kinds
    ));
    ui::info(&format!(
        "{} single-fault schedules{}",
        c.singles_checked,
        if c.singles_exhaustive {
            " (all of them)"
        } else {
            " (budget reached)"
        }
    ));
    if c.pairs_checked > 0 {
        ui::info(&format!(
            "{} pair schedules{}",
            c.pairs_checked,
            if c.pairs_exhaustive {
                " (all of them)"
            } else {
                " (budget reached)"
            }
        ));
    }
    for order in &certificate.interleavings {
        ui::info(&format!(
            "batch #{}: {} of {} orderings{}",
            order.batch,
            order.checked,
            order.total,
            if order.exhaustive {
                " (all of them)"
            } else {
                " (sampled)"
            }
        ));
    }
    let interleaving_replays: usize = certificate.interleavings.iter().map(|o| o.replays).sum();
    ui::info(&format!(
        "{} replays total ({} fault, {} ordering)",
        c.replays + interleaving_replays,
        c.replays,
        interleaving_replays
    ));

    ui::heading(2, "What it means");
    // Each claim carries its own verdict. Marking a true positive statement
    // with a cross because something *else* failed would misreport it.
    if c.is_clean() {
        ui::ok(&c.claim());
    } else {
        ui::bad(&c.claim());
    }
    for order in &certificate.interleavings {
        if order.is_order_independent() {
            ui::ok(&order.claim());
        } else {
            ui::bad(&order.claim());
        }
    }

    if !c.failures.is_empty() {
        ui::heading(3, "Failing schedules");
        for case in c.failures.iter().take(8) {
            println!("     {}", ui::yellow(&case.description));
            for violation in &case.violations {
                println!("       {}", ui::red(&violation.report()));
            }
        }
        if c.failures.len() > 8 {
            ui::info(&format!("\u{2026} and {} more", c.failures.len() - 8));
        }
    }

    for order in &certificate.interleavings {
        if !order.is_order_independent() {
            ui::heading(4, "Ordering");
            ui::bad(&format!(
                "{} of {} orderings change what the agent does",
                order.divergent.len(),
                order.total
            ));
            ui::info(&format!(
                "{} of {} adjacent pairs commute",
                order.commuting_pairs, order.adjacent_pairs
            ));
        }
    }
    println!();
}

/// Re-verify against a committed certificate.
fn compare(current: &Certificate, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let previous = Certificate::parse(&std::fs::read_to_string(path)?)?;
    let differences = current.differences(&previous);

    if differences.is_empty() {
        ui::ok(&format!("matches {}", path.display()));
        return Ok(());
    }
    for difference in &differences {
        ui::bad(difference);
    }
    // Non-zero so this works as a CI gate. Coverage that quietly shrinks is
    // the regression worth catching, and it does not change the verdict.
    std::process::exit(1);
}
