//! Integration tests for the `supervisor-rs` binary's command line and exit
//! codes.
//!
//! The exit-code table is fixed from Этап 3 on: usage error → 2, bad config →
//! 1, a process that failed to start → 1, otherwise 0. Этап 5 adds one code of
//! its own — `status` without a running daemon → 1 — and changes nothing else.
//!
//! Every invocation that starts the daemon passes `--state-file` into its own
//! temporary directory. Tests run in parallel, and the default path is shared
//! per uid: without the flag they would overwrite each other's snapshots, and
//! one daemon's cleanup on exit would delete another's file.

use std::io::Write;
use std::process::Command;

use tempfile::TempDir;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_supervisor-rs"))
}

/// Writes a config into a fresh temp dir and returns the dir plus the command
/// line to run it: `run <config> --state-file <dir>/state.toml`.
fn run_config(config: &str) -> (TempDir, Command) {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    let mut file = std::fs::File::create(&config_path).unwrap();
    file.write_all(config.as_bytes()).unwrap();

    let mut cmd = bin();
    cmd.arg("run")
        .arg(&config_path)
        .arg("--state-file")
        .arg(dir.path().join("state.toml"));
    (dir, cmd)
}

#[test]
fn run_exits_success_when_all_processes_exit_cleanly() {
    let (_dir, mut cmd) = run_config(
        r#"
        [[process]]
        name = "ok"
        command = ["/usr/bin/env", "true"]
        restart = "never"
        "#,
    );
    let status = cmd.status().unwrap();
    assert!(status.success());
}

#[test]
fn run_exits_failure_when_a_process_fails_to_spawn() {
    let (_dir, mut cmd) = run_config(
        r#"
        [[process]]
        name = "ok"
        command = ["/usr/bin/env", "true"]
        restart = "never"

        [[process]]
        name = "missing"
        command = ["/no/such/binary-xyz"]
        restart = "never"
        "#,
    );
    let status = cmd.status().unwrap();
    assert_eq!(status.code(), Some(1));
}

#[test]
fn run_exits_failure_for_missing_config() {
    let dir = tempfile::tempdir().unwrap();
    let status = bin()
        .arg("run")
        .arg(dir.path().join("no-such-config.toml"))
        .arg("--state-file")
        .arg(dir.path().join("state.toml"))
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(1));
}

#[test]
fn no_arguments_is_a_usage_error() {
    let out = bin().output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("Usage:"), "{stderr}");
}

/// The pre-Этап-5 form — a bare config path — is gone by the user's decision,
/// with no compatibility alias. This is that decision as an end-to-end
/// contract.
#[test]
fn bare_config_path_is_a_usage_error() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[[process]]\nname = \"ok\"\ncommand = [\"/usr/bin/env\", \"true\"]\n",
    )
    .unwrap();

    let out = bin().arg(&config_path).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("Usage:"), "{stderr}");
}

#[test]
fn unknown_subcommand_is_a_usage_error() {
    let out = bin().arg("reload").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("Usage:"), "{stderr}");
}

#[test]
fn help_prints_usage_to_stdout_and_exits_zero() {
    let out = bin().arg("--help").output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Usage:"), "{stdout}");
    assert!(out.stderr.is_empty(), "help must not write to stderr");
}

/// A quick smoke test of the "no daemon" path; the full lifecycle lives in
/// `tests/status.rs`.
#[test]
fn status_on_missing_state_file_exits_one() {
    let dir = tempfile::tempdir().unwrap();
    let out = bin()
        .arg("status")
        .arg("--state-file")
        .arg(dir.path().join("absent.toml"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("not running"), "{stderr}");
}
