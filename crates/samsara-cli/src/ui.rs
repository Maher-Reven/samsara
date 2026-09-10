//! Terminal presentation.
//!
//! No colour crate: a handful of ANSI codes is not worth a dependency, and
//! honouring `NO_COLOR` and a non-tty stdout is three lines either way.

use std::io::IsTerminal;
use std::sync::OnceLock;

fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        // https://no-color.org/ — respected by anything that wants to be a
        // good citizen of a pipeline.
        std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
    })
}

fn paint(code: &str, text: &str) -> String {
    if enabled() {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

pub fn bold(t: &str) -> String {
    paint("1", t)
}
pub fn dim(t: &str) -> String {
    paint("2", t)
}
pub fn red(t: &str) -> String {
    paint("31", t)
}
pub fn green(t: &str) -> String {
    paint("32", t)
}
pub fn yellow(t: &str) -> String {
    paint("33", t)
}
pub fn cyan(t: &str) -> String {
    paint("36", t)
}

/// A section heading.
pub fn heading(n: usize, title: &str) {
    println!("\n{} {}", dim(&format!("{n}.")), bold(title));
}

pub fn ok(msg: &str) {
    println!("  {} {}", green("\u{2713}"), msg);
}

pub fn bad(msg: &str) {
    println!("  {} {}", red("\u{2717}"), msg);
}

pub fn info(msg: &str) {
    println!("  {} {}", dim("\u{2022}"), msg);
}
