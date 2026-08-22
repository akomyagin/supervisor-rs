//! Integration tests for process spawning (Этап 1): real short-lived child
//! processes via `/usr/bin/env` for portability.

use std::collections::BTreeMap;

use supervisor_rs::config::{ProcessConfig, RestartPolicy, DEFAULT_STOP_GRACE_SECS};
use supervisor_rs::process;

fn cfg(name: &str, command: &[&str]) -> ProcessConfig {
    ProcessConfig {
        name: name.to_string(),
        command: command.iter().map(|s| s.to_string()).collect(),
        workdir: None,
        env: None,
        restart: RestartPolicy::Never,
        stop_grace_secs: DEFAULT_STOP_GRACE_SECS,
        health_check: None,
        log: None,
    }
}

#[test]
fn spawns_short_lived_process_and_waits_exit_zero() {
    let cfg = cfg("true", &["/usr/bin/env", "true"]);
    let mut child = process::spawn(&cfg).unwrap().child;
    let status = child.wait().unwrap();
    assert!(status.success());
}

#[test]
fn spawns_failing_process_and_observes_nonzero_exit() {
    let cfg = cfg("false", &["/usr/bin/env", "false"]);
    let mut child = process::spawn(&cfg).unwrap().child;
    let status = child.wait().unwrap();
    assert!(!status.success());
    assert_eq!(status.code(), Some(1));
}

#[test]
fn spawn_applies_env() {
    let mut config = cfg(
        "env-check",
        &["/usr/bin/env", "sh", "-c", "test \"$SUP_TEST\" = ok"],
    );
    config.env = Some(BTreeMap::from([("SUP_TEST".to_string(), "ok".to_string())]));
    let mut child = process::spawn(&config).unwrap().child;
    let status = child.wait().unwrap();
    assert!(status.success());
}
