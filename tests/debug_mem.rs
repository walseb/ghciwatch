//! Finite diagnostic runs use the unversioned GHC selected in the current environment.
#![cfg(target_os = "linux")]

use std::time::Duration;

struct TestDir(std::path::PathBuf);
impl TestDir {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "ghciwatch-debug-mem-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}
impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn run(source: &str, extra: &[&str]) -> (std::process::Output, String) {
    run_command(source, "ghci -ignore-dot-ghci Example.hs", extra).await
}

async fn run_command(
    source: &str,
    command: &str,
    extra: &[&str],
) -> (std::process::Output, String) {
    let dir = TestDir::new();
    std::fs::write(dir.path().join("Example.hs"), source).unwrap();
    let log = dir.path().join("diagnostic.log");
    let output = tokio::time::timeout(
        Duration::from_secs(60),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_ghciwatch"))
            .current_dir(dir.path())
            .args(["--command", command, "--debug-mem", "2", "--debug-mem-log"])
            .arg(&log)
            .args(extra)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("diagnostic run must exit, not wait for a fixing edit")
    .unwrap();
    (output, std::fs::read_to_string(log).unwrap())
}

#[tokio::test]
async fn recompiles_unchanged_source_and_persists_samples() {
    let (output, log) = run("module Example where\nx = 1\n", &[]).await;
    assert!(output.status.success(), "{log}");
    assert_eq!(log.matches("Compiling Example").count(), 3, "{log}");
    for iteration in 0..=2 {
        assert!(log.contains(&format!("DEBUG-MEM {iteration}/2:")), "{log}");
    }
}

#[tokio::test]
async fn failed_startup_has_log_but_no_sample() {
    let (output, log) = run("module Example where\nx = missingName\n", &[]).await;
    assert!(!output.status.success(), "{log}");
    assert!(log.contains("missingName"), "{log}");
    assert!(!log.contains("DEBUG-MEM"), "{log}");
}

#[tokio::test]
async fn failed_reload_stops_without_sampling_that_iteration() {
    let (output, log) = run(
        "module Example where\nx = 1\n",
        &[
            "--before-reload-shell",
            "sh -c \"printf 'module Example where\\nx = missingName\\n' > Example.hs\"",
        ],
    )
    .await;
    assert!(!output.status.success(), "{log}");
    assert!(log.contains("DEBUG-MEM 0/2:"), "{log}");
    assert!(!log.contains("DEBUG-MEM 1/2:"), "{log}");
    assert!(!log.contains("DEBUG-MEM 2/2:"), "{log}");
}

#[tokio::test]
async fn failed_test_hook_is_fatal_and_captured() {
    let (output, log) = run(
        "module Example where\nx = 1\n",
        &["--test-shell", "sh -c 'echo test-failure >&2; exit 1'"],
    )
    .await;
    assert!(!output.status.success(), "{log}");
    assert!(log.contains("test-failure"), "{log}");
    assert!(!log.contains("DEBUG-MEM"), "{log}");
}

#[tokio::test]
async fn failed_setup_does_not_wait_for_edits() {
    let (output, log) = run(
        "module Example where\nx = 1\n",
        &["--setup-shell", "sh -c 'echo setup-failure >&2; exit 1'"],
    )
    .await;
    assert!(!output.status.success(), "{log}");
    assert!(log.contains("setup-failure"), "{log}");
    assert!(!log.contains("DEBUG-MEM"), "{log}");
}

#[tokio::test]
async fn failed_command_does_not_retry() {
    let (output, log) = run_command("", "sh -c 'echo cabal-failure >&2; exit 1'", &[]).await;
    assert!(!output.status.success(), "{log}");
    assert!(log.contains("cabal-failure"), "{log}");
    assert!(!log.contains("DEBUG-MEM"), "{log}");
}

#[tokio::test]
async fn failed_ghci_test_is_fatal() {
    for command in ["missingTestName", "error \"test-exception\""] {
        let (output, log) = run("module Example where\nx = 1\n", &["--test-ghci", command]).await;
        assert!(!output.status.success(), "{log}");
        assert!(!log.contains("DEBUG-MEM"), "{log}");
    }
}

#[tokio::test]
async fn failed_before_reload_hook_does_not_compile_or_sample() {
    let (output, log) = run(
        "module Example where\nx = 1\n",
        &["--before-reload-shell", "false"],
    )
    .await;
    assert!(!output.status.success(), "{log}");
    assert_eq!(log.matches("Compiling Example").count(), 1, "{log}");
    assert!(!log.contains("DEBUG-MEM 1/2:"), "{log}");
}
