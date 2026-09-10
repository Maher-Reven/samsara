//! Declared properties, and sweeping a real agent against them.
//!
//! Until now every command enforced the same three properties, hardcoded to
//! the bugs in the built-in demo. That made the tool a demonstration rather
//! than something anyone could point at their own agent: only the person who
//! wrote it knows which of their tools change the world.

use std::process::Command;

fn samsara() -> Command {
    Command::new(env!("CARGO_BIN_EXE_samsara"))
}

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("samsara-cfg-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn init_writes_a_config_that_parses() {
    let dir = scratch("init");
    let path = dir.join("samsara.toml");

    let out = samsara()
        .arg("init")
        .arg("--out")
        .arg(&path)
        .output()
        .unwrap();
    assert!(out.status.success());

    // The scaffold must be valid input to the thing that reads it. A
    // template that does not parse is worse than no template.
    let text = std::fs::read_to_string(&path).unwrap();
    let config: samsara_core::prelude::Config = toml::from_str(&text).unwrap();
    assert!(!config.is_empty(), "the template must declare something");
    assert!(config.names().iter().any(|n| n.contains("duplicate")));
}

#[test]
fn init_refuses_to_overwrite() {
    let dir = scratch("init-twice");
    let path = dir.join("samsara.toml");
    samsara()
        .arg("init")
        .arg("--out")
        .arg(&path)
        .output()
        .unwrap();

    let out = samsara()
        .arg("init")
        .arg("--out")
        .arg(&path)
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "clobbering declared properties must not be silent"
    );
}

#[test]
fn a_config_declaring_nothing_is_an_error() {
    // It would otherwise produce a confident green result meaning nothing:
    // a sweep against no properties always passes.
    let dir = scratch("empty");
    let path = dir.join("empty.toml");
    std::fs::write(&path, "label = \"x\"\n").unwrap();

    let out = samsara()
        .args(["verify", "/dev/null", "--config"])
        .arg(&path)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stderr) + String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success());
    assert!(text.contains("declares no invariants"), "{text}");
}

#[test]
fn a_typo_in_a_property_is_rejected_rather_than_ignored() {
    // Believing you enforce a property you do not is the worst outcome
    // available, so unknown fields are refused.
    let dir = scratch("typo");
    let path = dir.join("typo.toml");
    std::fs::write(
        &path,
        "[[invariant]]\ntype = \"max_calls\"\ntool = \"a\"\nmaximum = 3\n",
    )
    .unwrap();

    let out = samsara()
        .args(["verify", "/dev/null", "--config"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(!out.status.success());
}

#[test]
fn an_unknown_property_type_is_rejected() {
    let dir = scratch("unknown");
    let path = dir.join("unknown.toml");
    std::fs::write(&path, "[[invariant]]\ntype = \"no_such_thing\"\n").unwrap();

    let out = samsara()
        .args(["verify", "/dev/null", "--config"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(!out.status.success());
}

#[test]
fn a_missing_explicit_config_is_an_error_not_a_fallback() {
    // Asking for specific properties and silently getting different ones is
    // the failure mode worth being loud about.
    let out = samsara()
        .args([
            "verify",
            "/dev/null",
            "--config",
            "/nonexistent/samsara.toml",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(text.contains("cannot read"), "{text}");
}

#[test]
fn declared_properties_catch_what_the_defaults_would_miss() {
    // `never_after_failure` is not something Samsara could have guessed:
    // only the agent's author knows a receipt must not follow a failed
    // charge. This is the whole reason the file exists.
    use samsara_core::prelude::*;

    let config: Config = toml::from_str(
        r#"
        [[invariant]]
        type = "never_after_failure"
        tool = "send_receipt"
        after = "charge_card"
        "#,
    )
    .unwrap();

    let invariants = config.build();
    assert_eq!(invariants.len(), 1);
    assert_eq!(invariants[0].name(), "never_after_failure");
    assert!(config.names()[0].contains("send_receipt"));
}
