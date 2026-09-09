//! The shipped CLI must fail as a CLI, not panic behind an early dispatch.

use std::process::Command;

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
