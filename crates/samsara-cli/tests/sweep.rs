//! `samsara sweep` as a CI gate.
//!
//! The value of a certificate is that it fails when coverage changes, not
//! only when a test breaks. These check that it actually does.

use std::process::Command;

fn samsara() -> Command {
    Command::new(env!("CARGO_BIN_EXE_samsara"))
}

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("samsara-sweep-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn the_fixed_agent_certificate_is_clean_and_exhaustive() {
    let dir = scratch("clean");
    let cert = dir.join("cert.json");

    let out = samsara()
        .args(["sweep", "--subject", "fixed", "--pairs", "--out"])
        .arg(&cert)
        .output()
        .unwrap();
    assert!(out.status.success());

    let text = std::fs::read_to_string(&cert).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();

    assert_eq!(parsed["verdict"], "clean");
    assert_eq!(parsed["coverage"]["singles_exhaustive"], true);
    assert_eq!(parsed["coverage"]["pairs_exhaustive"], true);
    assert!(parsed["claims"][0]
        .as_str()
        .unwrap()
        .contains("no single fault breaks"));
}

#[test]
fn a_certificate_is_identical_across_runs() {
    // Timestamps or durations would make this file churn in every diff and
    // therefore useless as a committed artifact.
    let dir = scratch("stable");
    let (a, b) = (dir.join("a.json"), dir.join("b.json"));

    for path in [&a, &b] {
        assert!(samsara()
            .args(["sweep", "--subject", "fixed", "--pairs", "--out"])
            .arg(path)
            .output()
            .unwrap()
            .status
            .success());
    }
    assert_eq!(
        std::fs::read_to_string(&a).unwrap(),
        std::fs::read_to_string(&b).unwrap()
    );
}

#[test]
fn checking_against_a_matching_certificate_passes() {
    let dir = scratch("match");
    let cert = dir.join("cert.json");
    samsara()
        .args(["sweep", "--subject", "fixed", "--pairs", "--out"])
        .arg(&cert)
        .output()
        .unwrap();

    let out = samsara()
        .args(["sweep", "--subject", "fixed", "--pairs", "--check"])
        .arg(&cert)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn a_regression_fails_the_gate() {
    let dir = scratch("regress");
    let cert = dir.join("cert.json");
    samsara()
        .args(["sweep", "--subject", "fixed", "--pairs", "--out"])
        .arg(&cert)
        .output()
        .unwrap();

    // The buggy agent checked against the fixed agent's certificate.
    let out = samsara()
        .args(["sweep", "--subject", "retry", "--pairs", "--check"])
        .arg(&cert)
        .output()
        .unwrap();

    assert!(!out.status.success(), "a regression must fail the gate");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("verdict changed"), "{text}");
}

#[test]
fn shrinking_coverage_fails_the_gate_even_though_nothing_broke() {
    // The quiet regression this exists for: still green, but checking less.
    // A gate that only compared verdicts would wave it through.
    let dir = scratch("shrink");
    let cert = dir.join("cert.json");
    samsara()
        .args(["sweep", "--subject", "fixed", "--pairs", "--out"])
        .arg(&cert)
        .output()
        .unwrap();

    // Same agent, same clean verdict — but pairs are no longer swept.
    let out = samsara()
        .args(["sweep", "--subject", "fixed", "--check"])
        .arg(&cert)
        .output()
        .unwrap();

    assert!(
        !out.status.success(),
        "dropping the pair sweep must fail even though nothing broke"
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("pair coverage"), "{text}");
}

#[test]
fn the_committed_certificates_still_hold() {
    // The repo's own gate. If someone changes the engine and the coverage
    // moves, this is what tells them.
    // Integration tests run with the package as the working directory, not
    // the workspace, so the path has to be anchored.
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("workspace root");

    for (subject, name) in [
        ("fixed", "fixed-agent.json"),
        ("order", "assembling-agent.json"),
    ] {
        let path = root.join("certificates").join(name);
        let path = path.to_str().unwrap();
        let mut command = samsara();
        command.args(["sweep", "--subject", subject]);
        if subject == "fixed" {
            command.arg("--pairs");
        }
        let out = command.args(["--check", path]).output().unwrap();
        assert!(
            out.status.success(),
            "{path} no longer matches:\n{}",
            String::from_utf8_lossy(&out.stdout)
        );
    }
}
