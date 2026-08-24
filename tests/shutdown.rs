use nix::sys::signal;
use nix::sys::signal::Signal;
use nix::unistd::Pid;
use test_harness::test;
use test_harness::BaseMatcher;
use test_harness::Fs;
use test_harness::GhciWatch;
use test_harness::GhciWatchBuilder;
use test_harness::JsonValue;

/// Test that `ghciwatch` can gracefully shutdown on Ctrl-C.
#[test]
async fn can_shutdown_gracefully() {
    let mut session = GhciWatch::new("tests/data/simple")
        .await
        .expect("ghciwatch starts");
    session
        .wait_until_ready()
        .await
        .expect("ghciwatch loads ghci");

    signal::kill(Pid::from_raw(session.pid() as i32), Signal::SIGINT)
        .expect("Failed to send Ctrl-C to ghciwatch");

    session
        .wait_for_log("^All tasks completed successfully$")
        .await
        .unwrap();

    let status = session.wait_until_exit().await.unwrap();
    assert!(status.success(), "ghciwatch exits successfully");
}

/// Intentional replacement keeps the old output readers alive until its process tree exits.
#[test]
async fn replacement_keeps_old_output_pipe_alive() {
    let mut session = GhciWatchBuilder::new("tests/data/simple")
        .with_args([
            "--restart-glob",
            "src/MyLib.hs",
            "--before-restart-ghci",
            ":m + Control.Concurrent",
            "--before-restart-ghci",
            "forkIO (threadDelay 200000 >> putStrLn \"late shutdown output\")",
            "--before-kill",
            "sleep 1",
        ])
        .start()
        .await
        .expect("ghciwatch starts");
    session
        .wait_until_ready()
        .await
        .expect("ghciwatch loads ghci");
    session.clear_events();
    session
        .fs()
        .touch(session.path("src/MyLib.hs"))
        .await
        .expect("can trigger an intentional replacement");
    session
        .wait_for_startup_log(replacement_completed())
        .await
        .expect("ghciwatch completes the replacement");
    assert!(
        session.assert_logged("resource vanished").is_err(),
        "old GHCi wrote to a pipe whose reader was dropped before shutdown"
    );
}

fn extract_pid(event: &test_harness::Event) -> i32 {
    match event.fields.get("pid").unwrap() {
        JsonValue::Number(pid) => pid,
        value => panic!("pid field has wrong type: {value:?}"),
    }
    .as_i64()
    .expect("pid is i64")
    .try_into()
    .expect("pid is i32")
}

fn replacement_completed() -> BaseMatcher {
    BaseMatcher::message(
        r"((Starting up|Reloading) failed|Finished (starting up|reloading) in \d+\.\d+m?s)$",
    )
}

/// Test that an unexpected GHCi exit gets one automatic replacement after the crash delay.
#[test]
async fn restarts_after_ghci_killed() {
    let mut session = GhciWatchBuilder::new("tests/data/simple")
        .with_startup_timeout(std::time::Duration::from_secs(25))
        .start()
        .await
        .expect("ghciwatch starts");

    let event = session
        .wait_for_startup_log(BaseMatcher::message("^Started ghci$"))
        .await
        .expect("ghciwatch starts ghci");
    let pid = extract_pid(&event);

    session
        .wait_until_ready()
        .await
        .expect("ghciwatch loads ghci");

    signal::kill(Pid::from_raw(pid), Signal::SIGKILL).expect("Failed to kill ghci");

    // No source edit is needed for the first replacement attempt. This is important for crashes
    // caused by implementation or toolchain faults rather than by the most recently compiled file.
    session
        .wait_for_log("ghci exited unexpectedly")
        .await
        .expect("ghciwatch detects unexpected ghci exit");
    session.clear_events();
    session
        .wait_for_startup_log(replacement_completed())
        .await
        .expect("ghciwatch automatically restarts ghci after the crash delay");
}

/// Persistent services keep recovering without requiring an unrelated source edit.
#[test]
async fn restart_on_exit_keeps_replacing_ghci() {
    let mut session = GhciWatchBuilder::new("tests/data/simple")
        .with_startup_timeout(std::time::Duration::from_secs(25))
        .with_arg("--restart-on-exit")
        .start()
        .await
        .expect("ghciwatch starts");

    let event = session
        .wait_for_startup_log(BaseMatcher::message("^Started ghci$"))
        .await
        .expect("ghciwatch starts ghci");
    let pid = extract_pid(&event);
    session
        .wait_until_ready()
        .await
        .expect("ghciwatch loads ghci");

    session.clear_events();
    signal::kill(Pid::from_raw(pid), Signal::SIGKILL).expect("Failed to kill ghci");
    session
        .wait_for_startup_log(replacement_completed())
        .await
        .expect("ghciwatch replaces ghci after the crash delay");
}

/// Changes to any configured watched path are retained while a crashed session is recovering.
#[test]
async fn watched_non_haskell_change_is_retained_during_recovery() {
    let mut session = GhciWatchBuilder::new("tests/data/simple")
        .with_startup_timeout(std::time::Duration::from_secs(25))
        .start()
        .await
        .expect("ghciwatch starts");

    let event = session
        .wait_for_startup_log(BaseMatcher::message("^Started ghci$"))
        .await
        .expect("ghciwatch starts ghci");
    let pid = extract_pid(&event);

    session
        .wait_until_ready()
        .await
        .expect("ghciwatch loads ghci");

    signal::kill(Pid::from_raw(pid), Signal::SIGKILL).expect("Failed to kill ghci");

    session
        .wait_for_log("ghci exited unexpectedly")
        .await
        .expect("ghciwatch detects unexpected ghci exit");

    // A non-Haskell file is still a watched state change. It is merged into the delayed automatic
    // replacement rather than discarded by the normal reload classifier.
    session.clear_events();
    session
        .fs()
        .touch(session.path("src/irrelevant.txt"))
        .await
        .expect("can touch watched non-Haskell file");
    session
        .wait_for_startup_log(replacement_completed())
        .await
        .expect("ghciwatch restarts while retaining the watched change");
}

/// Under `--no-auto-reload`, startup recovery follows every configured watch root rather than
/// only the file named by the compiler diagnostic.
#[test]
async fn handles_upstream_startup_repair_from_configured_watch() {
    let mut session = GhciWatchBuilder::new("tests/data/with-dep")
        .with_startup_timeout(std::time::Duration::from_secs(25))
        .before_start(move |path| {
            // The dependent module remains valid, but no longer exports MyLib's imported name.
            async move {
                Fs::new()
                    .replace(path.join("simple-dep/src/SimpleDep.hs"), "(depFunc)", "()")
                    .await
            }
        })
        .with_args(["--watch", "simple-dep/src", "--no-auto-reload"])
        .start()
        .await
        .expect("ghciwatch starts");

    // Startup fails in MyLib because its upstream dependency no longer exports depFunc.
    session
        .wait_for_startup_log("ghci exited during startup")
        .await
        .expect("ghciwatch detects first startup failure");

    // Clear events so we don't match the first "ghci exited during startup" again.
    session.clear_events();

    // Touching a source file triggers the first restart attempt, which also fails.
    session
        .fs()
        .touch(session.path("src/MyLib.hs"))
        .await
        .expect("can touch source file");

    // The second failure confirms the retry loop re-enters rather than crashing.
    session
        .wait_for_startup_log("ghci exited during startup")
        .await
        .expect("ghciwatch detects second startup failure");

    // The diagnostic points at MyLib's import, but changing the watched upstream module must retry.
    session
        .fs()
        .replace(
            session.path("simple-dep/src/SimpleDep.hs"),
            "()",
            "(depFunc)",
        )
        .await
        .expect("can fix simple-dep");

    // This restart should succeed.
    session
        .wait_for_startup_log(replacement_completed())
        .await
        .expect("ghciwatch restarts ghci after dependency is fixed");
}

/// A source covered by `--watch` triggers startup recovery without a diagnostic-specific watcher.
#[test]
async fn startup_retry_from_configured_watch() {
    let mut session = GhciWatchBuilder::new("tests/data/with-dep")
        .with_startup_timeout(std::time::Duration::from_secs(25))
        .before_start(move |path| async move {
            Fs::new()
                .replace(path.join("src/MyLib.hs"), "\"someFunc\"", "\"someFunc")
                .await
        })
        .start()
        .await
        .expect("ghciwatch starts");

    session
        .wait_for_startup_log("ghci exited during startup")
        .await
        .expect("ghciwatch detects startup failure");
    session.clear_events();

    session
        .fs()
        .replace(session.path("src/MyLib.hs"), "\"someFunc", "\"someFunc\"")
        .await
        .expect("can fix watched source file");

    session
        .wait_for_startup_log(replacement_completed())
        .await
        .expect("ghciwatch retries after a configured watched source changes");
}

/// Check that startup failures are handled correctly with `--before-restart-ghci` hooks.
#[test]
async fn handles_repeated_startup_failures_before_restart_ghci_hook() {
    let mut session = GhciWatchBuilder::new("tests/data/with-dep")
        .with_startup_timeout(std::time::Duration::from_secs(25))
        .before_start(move |path| {
            // A version of SimpleDep.hs with an unclosed string literal — cabal will refuse to build it.
            async move {
                Fs::new()
                    .replace(
                        path.join("simple-dep/src/SimpleDep.hs"),
                        "\"depFunc\"",
                        "\"depFunc",
                    )
                    .await
            }
        })
        .with_args([
            "--before-restart-ghci",
            "putStrLn \"hello\"",
            "--before-reload-shell",
            "touch before-reload-shell",
            "--after-reload-shell",
            "touch after-reload-shell",
            "--before-restart-shell",
            "touch before-restart-shell",
            "--after-restart-shell",
            "touch after-restart-shell",
        ])
        .start()
        .await
        .expect("ghciwatch starts");

    // First startup fails because simple-dep won't compile.
    session
        .wait_for_startup_log("ghci exited during startup")
        .await
        .expect("ghciwatch detects first startup failure");

    // Clear events so we don't match the first "ghci exited during startup" again.
    session.clear_events();

    // Touching a source file triggers the first restart attempt, which also fails.
    session
        .fs()
        .touch(session.path("src/MyLib.hs"))
        .await
        .expect("can touch source file");

    // The shell reload hooks bracket the failed replacement even though no GHCi prompt exists.
    session
        .fs()
        .wait_for_path(
            session.startup_timeout,
            &session.path("before-reload-shell"),
        )
        .await
        .expect("before-reload shell hook runs");
    session
        .fs()
        .wait_for_path(session.startup_timeout, &session.path("after-reload-shell"))
        .await
        .expect("after-reload shell hook runs");
    session
        .fs()
        .wait_for_path(
            session.startup_timeout,
            &session.path("before-restart-shell"),
        )
        .await
        .expect("before-restart shell hook runs");
    session
        .fs()
        .wait_for_path(
            session.startup_timeout,
            &session.path("after-restart-shell"),
        )
        .await
        .expect("after-restart shell hook runs");

    // The second failure confirms the retry loop re-enters rather than crashing.
    session
        .wait_for_startup_log("ghci exited during startup")
        .await
        .expect("ghciwatch detects second startup failure");
}

/// Test that when ghci exits unexpectedly during a dispatched reload/restart (not during startup),
/// ghciwatch detects the exit and restarts on the next relevant file change.
///
/// This catches a bug where the dispatch `tokio::select!` did not poll `exited_receiver`, causing
/// the dispatch task to hold the ghci Mutex forever and deadlocking the retry-restart loop.
#[test]
async fn handles_unexpected_exit_during_dispatch() {
    let mut session = GhciWatchBuilder::new("tests/data/with-dep")
        .with_startup_timeout(std::time::Duration::from_secs(25))
        .with_args([
            "--watch",
            "simple-dep",
            "--restart-glob",
            "simple-dep/src/*.hs",
        ])
        .start()
        .await
        .expect("ghciwatch starts");

    // Wait for successful initial startup.
    session
        .wait_until_ready()
        .await
        .expect("ghciwatch loads ghci");

    // Introduce a syntax error in the dependency. Since it matches --restart-glob,
    // this triggers a restart. The restart fails because cabal can't build the broken
    // dependency, causing ghci to exit unexpectedly.
    session
        .fs()
        .replace(
            session.path("simple-dep/src/SimpleDep.hs"),
            "\"depFunc\"",
            "\"depFunc",
        )
        .await
        .expect("can break simple-dep");

    // ghciwatch should detect the unexpected exit (not "during startup").
    session
        .wait_for_log("ghci exited unexpectedly")
        .await
        .expect("ghciwatch detects unexpected exit during dispatch");

    // Fix the syntax error. This also matches --restart-glob, so it triggers a restart attempt.
    session
        .fs()
        .replace(
            session.path("simple-dep/src/SimpleDep.hs"),
            "\"depFunc",
            "\"depFunc\"",
        )
        .await
        .expect("can fix simple-dep");

    // ghciwatch should restart successfully.
    session
        .wait_for_startup_log(replacement_completed())
        .await
        .expect("ghciwatch restarts ghci after fixing the dependency");
}

/// Regression test: after a successful startup, a dependency change triggers a restart that fails
/// due to a compilation error, then fixing the error should trigger another restart.
#[test]
async fn restart_after_failed_restart_on_dep_fix() {
    let mut session = GhciWatchBuilder::new("tests/data/with-dep")
        .with_startup_timeout(std::time::Duration::from_secs(25))
        .with_args([
            "--watch",
            "simple-dep",
            "--restart-glob",
            "simple-dep/src/*.hs",
        ])
        .start()
        .await
        .expect("ghciwatch starts");

    // Wait for successful initial startup.
    session
        .wait_until_ready()
        .await
        .expect("ghciwatch loads ghci");

    // Introduce a syntax error in the dependency. Since it matches --restart-glob,
    // this triggers a restart. The restart fails because cabal can't build the broken
    // dependency, causing ghci to exit unexpectedly.
    session
        .fs()
        .replace(
            session.path("simple-dep/src/SimpleDep.hs"),
            "\"depFunc\"",
            "\"depFunc",
        )
        .await
        .expect("can break simple-dep");

    // ghciwatch should detect the unexpected exit.
    session
        .wait_for_log("ghci exited unexpectedly")
        .await
        .expect("ghciwatch detects unexpected exit during restart");

    // Fix the syntax error. This also matches --restart-glob, so it should trigger a restart.
    session.clear_events();
    session
        .fs()
        .replace(
            session.path("simple-dep/src/SimpleDep.hs"),
            "\"depFunc",
            "\"depFunc\"",
        )
        .await
        .expect("can fix simple-dep");

    session
        .wait_for_log(replacement_completed())
        .await
        .expect("ghciwatch restarts ghci after fixing the dependency");
}

/// Ghciwatch should not become an orphan when the process which launched it exits.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn exits_when_parent_process_exits() {
    use std::time::Duration;

    let test_dir = std::env::temp_dir().join(format!(
        "ghciwatch-parent-exit-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&test_dir).expect("can create parent-exit test directory");
    let ready_path = test_dir.join("ready");
    let pid_path = test_dir.join("pid");
    let output_path = test_dir.join("output");

    // Keep the short-lived shell alive until ghciwatch has entered startup. Its exit then reparents
    // ghciwatch and should trigger the normal shutdown manager.
    let script = r#"
        "$GHCIWATCH" \
          --before-startup-shell "touch $READY" \
          --command "sleep 300" \
          >"$OUTPUT" 2>&1 &
        child=$!
        printf '%s\n' "$child" >"$PID_FILE"
        i=0
        while [ ! -e "$READY" ]; do
          if ! kill -0 "$child" 2>/dev/null; then exit 2; fi
          i=$((i + 1))
          if [ "$i" -gt 200 ]; then exit 3; fi
          sleep 0.05
        done
    "#;
    let status = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(script)
        .env("GHCIWATCH", env!("CARGO_BIN_EXE_ghciwatch"))
        .env("READY", &ready_path)
        .env("PID_FILE", &pid_path)
        .env("OUTPUT", &output_path)
        .status()
        .await
        .expect("can run the temporary parent process");
    assert!(status.success(), "temporary parent failed: {status}");

    let pid = std::fs::read_to_string(&pid_path)
        .expect("parent records ghciwatch PID")
        .trim()
        .parse::<i32>()
        .expect("ghciwatch PID is numeric");
    let exited = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"));
            let alive = stat
                .ok()
                .and_then(|stat| stat.rsplit_once(") ").map(|(_, fields)| fields.to_owned()))
                .and_then(|fields| fields.chars().next())
                .is_some_and(|state| state != 'Z' && state != 'X');
            if !alive {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    if exited.is_err() {
        let _ = signal::kill(Pid::from_raw(pid), Signal::SIGKILL);
    }
    assert!(
        exited.is_ok(),
        "ghciwatch remained alive after its parent exited; output:\n{}",
        std::fs::read_to_string(&output_path).unwrap_or_default()
    );

    let _ = std::fs::remove_dir_all(test_dir);
}
