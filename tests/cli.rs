//! Integration tests for the `supervisor-rs` binary's exit code behaviour.

use std::io::Write;
use std::process::Command;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_supervisor-rs"))
}

#[test]
fn exits_success_when_all_processes_start_and_exit_cleanly() {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    write!(
        file,
        r#"
        [[process]]
        name = "ok"
        command = ["/usr/bin/env", "true"]
        restart = "never"
        "#
    )
    .unwrap();

    let status = bin().arg(file.path()).status().unwrap();
    assert!(status.success());
}

#[test]
fn exits_failure_when_a_process_fails_to_spawn() {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    write!(
        file,
        r#"
        [[process]]
        name = "ok"
        command = ["/usr/bin/env", "true"]
        restart = "never"

        [[process]]
        name = "missing"
        command = ["/no/such/binary-xyz"]
        restart = "never"
        "#
    )
    .unwrap();

    let status = bin().arg(file.path()).status().unwrap();
    assert!(!status.success());
    assert_eq!(status.code(), Some(1));
}

#[test]
fn exits_with_usage_error_for_wrong_argument_count() {
    let status = bin().status().unwrap();
    assert_eq!(status.code(), Some(2));
}
