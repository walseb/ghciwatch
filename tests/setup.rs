use std::time::Duration;

use test_harness::{BaseMatcher, GhciWatchBuilder};

/// A non-Haskell update retries setup, but time alone (even with restart-on-exit) does not.
#[test_harness::test(current)]
async fn setup_failure_gates_launch_and_retries_on_watched_update() {
    let mut session = GhciWatchBuilder::new("tests/data/simple")
        .with_repl_command("sh -c 'touch launched; exec ghc --interactive -ignore-dot-ghci src/MyLib.hs'")
        .with_args([
            "--restart-on-exit",
            "--error-file", "src/compile.txt",
            "--setup-shell",
            "sh -c 'echo attempt >> attempts; test -f src/ready.txt || { echo setup-stdout; echo setup-stderr >&2; exit 1; }'",
            "--setup-shell", "touch second-setup",
            "--before-startup-shell", "touch before-startup",
        ])
        .before_start(|path| async move {
            test_harness::Fs::new().write(path.join("src/setup-input.txt"), "unchanged").await
        })
        .start().await.unwrap();
    session
        .wait_for_log(BaseMatcher::message("Waiting for a watched file"))
        .await
        .unwrap();
    let errors = session
        .fs()
        .read(session.path("src/compile.txt"))
        .await
        .unwrap();
    assert!(errors.contains("setup-stdout"));
    assert!(errors.contains("setup-stderr"));
    assert!(!session.path("launched").exists());
    assert!(!session.path("second-setup").exists());
    assert!(!session.path("before-startup").exists());

    // Setup uses the same content-only rule, even for non-Haskell files.
    session
        .fs()
        .touch(session.path("src/setup-input.txt"))
        .await
        .unwrap();
    session
        .fs()
        .write(session.path("src/setup-input.txt"), "unchanged")
        .await
        .unwrap();
    // Covers the normal crash-retry delay and several poll/debounce cycles. Writing the
    // error file inside the watch root must not produce a self-sustaining retry loop.
    tokio::time::sleep(Duration::from_secs(12)).await;
    assert_eq!(
        session.fs().read(session.path("attempts")).await.unwrap(),
        "attempt\n"
    );
    session
        .fs()
        .write(session.path("src/ready.txt"), "ready")
        .await
        .unwrap();
    session.wait_until_ready().await.unwrap();
    assert!(session.path("launched").exists());
    assert!(session.path("second-setup").exists());
    assert!(session.path("before-startup").exists());
    assert!(!session
        .fs()
        .read(session.path("src/compile.txt"))
        .await
        .unwrap()
        .contains("setup-stderr"));

    // Every full restart must pass setup again, not just the first launch.
    session.clear_events();
    session
        .fs()
        .write(session.path("src/ready.txt"), "")
        .await
        .unwrap();
    session
        .fs()
        .remove(session.path("src/ready.txt"))
        .await
        .unwrap();
    session.fs().remove(session.path("launched")).await.unwrap();
    session
        .fs()
        .append(session.path("my-simple-package.cabal"), "\n")
        .await
        .unwrap();
    session
        .wait_for_log(BaseMatcher::message("Waiting for a watched file"))
        .await
        .unwrap();
    assert!(!session.path("launched").exists());
    session
        .fs()
        .write(session.path("src/ready.txt"), "ready again")
        .await
        .unwrap();
    session
        .fs()
        .wait_for_path(session.startup_timeout, &session.path("launched"))
        .await
        .unwrap();
}

#[test_harness::test(current)]
async fn setup_spawn_failure_is_written_to_error_file() {
    let mut session = GhciWatchBuilder::new("tests/data/simple")
        .with_repl_command("sh -c 'touch launched; exec ghc --interactive -ignore-dot-ghci'")
        .with_args([
            "--setup-shell",
            "./missing-setup-command",
            "--error-file",
            "compile.txt",
        ])
        .start()
        .await
        .unwrap();
    session
        .wait_for_log(BaseMatcher::message("Waiting for a watched file"))
        .await
        .unwrap();
    let errors = session
        .fs()
        .read(session.path("compile.txt"))
        .await
        .unwrap();
    assert!(errors.contains("missing-setup-command"));
    assert!(!session.path("launched").exists());
}
