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
