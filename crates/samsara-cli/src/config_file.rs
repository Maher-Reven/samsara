//! Loading the properties an agent must hold.
//!
//! What counts as "wrong" is not something Samsara can know. `delete_file`
//! twice is an incident; `search` twice is a waste. Only the person who
//! wrote the agent can say which of their tools is which, so the properties
//! live in a file beside the agent and every command reads the same one.

use std::path::{Path, PathBuf};

use samsara_core::prelude::*;

use crate::ui;

/// Default file name, looked for in the working directory.
pub const DEFAULT: &str = "samsara.toml";

/// Load the declared properties.
///
/// An explicit `--config` that does not exist is an error: the user asked
/// for specific properties and silently enforcing different ones would be
/// the worst possible response. A missing *default* file is not an error,
/// but it is announced, because running with weak properties without
/// realising is how a green result comes to mean nothing.
pub fn load(explicit: Option<&Path>) -> Result<Config, Box<dyn std::error::Error>> {
    if let Some(path) = explicit {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        return parse(&text, path);
    }

    let default = PathBuf::from(DEFAULT);
    if default.exists() {
        let text = std::fs::read_to_string(&default)?;
        return parse(&text, &default);
    }

    ui::info(&format!(
        "no {DEFAULT} found \u{2014} enforcing only that the run terminates. \
         Declare your effectful tools to get more than that."
    ));
    Ok(Config::conservative_default())
}

fn parse(text: &str, path: &Path) -> Result<Config, Box<dyn std::error::Error>> {
    let config: Config = toml::from_str(text).map_err(|e| format!("{}: {e}", path.display()))?;

    if config.is_empty() {
        // A file that declares nothing is almost certainly a mistake, and
        // it would otherwise produce a confident green result that means
        // nothing at all.
        return Err(format!(
            "{} declares no invariants; a sweep against no properties always passes",
            path.display()
        )
        .into());
    }
    Ok(config)
}

/// Write a starting point, with the common properties present and
/// commented rather than absent.
pub fn scaffold(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    if path.exists() {
        return Err(format!("{} already exists", path.display()).into());
    }
    std::fs::write(path, TEMPLATE)?;
    ui::ok(&format!("wrote {}", path.display()));
    ui::info("declare your effectful tools, then run `samsara sweep`");
    Ok(())
}

const TEMPLATE: &str = r#"# What must never happen.
#
# Samsara injects every fault it can and checks these after each one. The
# properties are yours to declare: only you know which of your tools change
# the world and which merely read it.

label = "my-agent"

# No side effect happens twice.
#
# List the tools that actually mutate something. Leaving this empty watches
# every tool, which sounds safer and is not: naturally idempotent reads will
# trip it and train you to ignore the result.
[[invariant]]
type = "no_duplicate_effects"
tools = ["delete_file", "charge_card", "send_email"]
# If your tools accept an idempotency key stable across retries, name the
# argument here and those calls are exempt -- retrying them is correct.
# idempotency_key = "idempotency_key"

# The run finishes. Catches the retry storm.
[[invariant]]
type = "terminates_within"
effects = 200

# After a prerequisite fails, the dependent step must not run anyway.
# [[invariant]]
# type = "never_after_failure"
# tool = "send_receipt"
# after = "charge_card"

# If something happened, its audit record must have happened too.
# [[invariant]]
# type = "requires"
# tool = "charge_card"
# then = "log_audit"

# A per-tool ceiling, for the runaway a whole-run budget is too coarse to see.
# [[invariant]]
# type = "max_calls"
# tool = "search"
# max = 20

# [[invariant]]
# type = "token_budget"
# tokens = 100000
"#;
