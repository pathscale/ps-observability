//! The shipped CLI must fail as a CLI, not panic behind an early dispatch.

use std::process::Command;

/// Both directions matter: measuring before the click rejects a repair and
/// accepts a regression. This drives the actual renderer and shipped CLI.
#[test]
#[ignore = "requires QA_HOST pointing to a font-enabled chuzz-headless build"]
fn paint_verdict_uses_the_state_after_the_action() {
    let host = std::env::var_os("QA_HOST").expect("set QA_HOST to chuzz-headless");
    let fixture =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/paint-action-fixture");
    for (group, success, verdict) in [
        ("drag", true, "pointer-drag-commits"),
        ("cancel", true, "pointer-drag-cancels"),
        ("gridcell", true, "explicit-gridcell-activates"),
        ("restore", true, "restored-contrast-passes"),
        ("break", false, "broken-contrast-fails"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_ps-qa"))
            .arg("--app")
            .arg(fixture.join("ps-qa.ron"))
            .arg("qa-hosted")
            .arg(group)
            .arg("--host")
            .arg(&host)
            .arg("--page")
            .arg(fixture.join("page.html"))
            .arg("--checks")
            .arg(fixture.join("checks"))
            .output()
            .expect("run ps-qa against native fixture");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.success(), success, "{stdout}\n{stderr}");
        assert!(stdout.contains(verdict), "{stdout}\n{stderr}");
        if !success {
            assert!(
                stdout.contains("below their contrast floor"),
                "{stdout}\n{stderr}"
            );
        }
    }
}

#[test]
fn component_sweep_without_profile_returns_a_clear_error() {
    let working_directory = std::env::temp_dir().join(format!(
        "ps-qa-no-profile-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the epoch")
            .as_nanos()
    ));
    std::fs::create_dir(&working_directory).expect("create isolated working directory");

    let output = Command::new(env!("CARGO_BIN_EXE_ps-qa"))
        .current_dir(&working_directory)
        .args([
            "sweep-components",
            "--host",
            "missing-host",
            "--dists",
            "missing-dists",
        ])
        .output()
        .expect("run ps-qa");

    std::fs::remove_dir(&working_directory).expect("remove isolated working directory");
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(!output.status.success(), "a product sweep needs a profile");
    assert!(stderr.contains("no application profile"), "{stderr}");
    assert!(!stderr.contains("panicked at"), "{stderr}");
}

/// A named descriptor that is not there must not attach to somebody else's host.
///
/// The failure this pins is silent: discovery would fall through to the newest
/// descriptor in the temporary directory, which on a machine running several
/// suites at once is another site's application. The run then succeeds and
/// reports a tree that is real, plausible, and about the wrong page.
#[test]
fn a_named_descriptor_that_does_not_exist_is_an_error() {
    let missing = std::env::temp_dir().join(format!(
        "ps-qa-absent-descriptor-{}-{}.json",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the epoch")
            .as_nanos()
    ));
    assert!(!missing.exists(), "the test needs a path that is not there");

    let output = Command::new(env!("CARGO_BIN_EXE_ps-qa"))
        .args([
            "--descriptor",
            missing.to_str().expect("a UTF-8 temporary path"),
            "dom",
            "Save",
        ])
        .output()
        .expect("run ps-qa");

    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.contains(missing.to_str().expect("a UTF-8 temporary path")),
        "the error must name the descriptor that is missing: {stderr}"
    );
    assert!(
        !stderr.contains("no reachable inspector descriptor found"),
        "a named descriptor must not fall through to discovery: {stderr}"
    );
    assert!(!stderr.contains("panicked at"), "{stderr}");
}
