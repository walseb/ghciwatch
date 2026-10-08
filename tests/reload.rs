use indoc::indoc;

use test_harness::test;
use test_harness::BaseMatcher;
use test_harness::GhciWatch;
use test_harness::GhciWatchBuilder;

/// Test that `ghciwatch` can start up and then reload on changes.
#[test]
async fn can_reload() {
    let mut session = GhciWatch::new("tests/data/simple")
        .await
        .expect("ghciwatch starts");
    session
        .wait_until_ready()
        .await
        .expect("ghciwatch loads ghci");
    session
        .fs()
        .append(
            session.path("src/MyLib.hs"),
            indoc!(
                "

            hello = 1 :: Integer

            "
            ),
        )
        .await
        .unwrap();
    session
        .wait_until_reload()
        .await
        .expect("ghciwatch reloads on changes");
    session
        .wait_for_log(BaseMatcher::reload_completes())
        .await
        .expect("ghciwatch finishes reloading");
}

/// `--no-auto-reload` keeps watched targets synchronized without reloading ordinary edits.
#[test]
async fn can_synchronize_targets_without_auto_reload() {
    let mut session = GhciWatchBuilder::new("tests/data/simple")
        .with_args([
            "--no-auto-reload",
            "--test-ghci",
            "putStrLn \"test action ran\"",
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
        .append(session.path("src/MyLib.hs"), "\nordinaryEdit = ()\n")
        .await
        .unwrap();
    session
        .wait_for_log(BaseMatcher::message("Finished dispatching ghci event"))
        .await
        .expect("ghciwatch processes the edit");
    assert!(
        session.assert_logged(BaseMatcher::ghci_reload()).is_err(),
        "ghciwatch must not issue :reload for an ordinary edit"
    );
    assert!(
        session
            .assert_logged(BaseMatcher::message("test action ran"))
            .is_err(),
        "ghciwatch must not run test actions for a suppressed edit"
    );

    let new_module = session.path("src/NewModule.hs");
    session
        .fs()
        .write(&new_module, "module NewModule where\nnewValue = ()\n")
        .await
        .unwrap();
    session
        .wait_until_add()
        .await
        .expect("ghciwatch still adds new watched modules");
    session
        .wait_for_log(BaseMatcher::reload_completes())
        .await
        .expect("ghciwatch finishes adding the new module");

    session.fs().remove(new_module).await.unwrap();
    session
        .wait_for_log(BaseMatcher::ghci_remove())
        .await
        .expect("ghciwatch still removes deleted watched modules");
    session
        .wait_for_log(BaseMatcher::reload_completes())
        .await
        .expect("ghciwatch finishes removing the deleted module");
}

/// A superseded compilation must not lose its test hook when --no-auto-reload makes the
/// mandatory follow-up a no-op. Ordinary no-op edits must still leave the hook alone.
#[test(current)]
async fn superseded_test_hook_runs_on_no_op_follow_up() {
    let mut session = GhciWatchBuilder::new("tests/data/simple")
        .with_args([
            "--no-auto-reload",
            "--no-interrupt-reloads",
            "--before-reload-ghci",
            ":! touch compilation-started; sleep 2",
            "--test-ghci",
            ":! echo x >> hook-count",
        ])
        .with_startup_timeout(std::time::Duration::from_secs(25))
        .start()
        .await
        .expect("ghciwatch starts");
    session
        .wait_until_ready()
        .await
        .expect("ghciwatch is ready");
    let count = session.path("hook-count");
    session
        .fs()
        .remove(&count)
        .await
        .expect("can reset startup hook count");

    let source = session.path("src/NewModule.hs");
    session
        .fs()
        .write(&source, "module NewModule where\nnewValue = ()\n")
        .await
        .expect("can trigger compilation");
    session
        .fs()
        .wait_for_path(
            session.startup_timeout,
            &session.path("compilation-started"),
        )
        .await
        .expect("compilation reaches its delayed hook");
    session
        .fs()
        .append(&source, "anotherValue = ()\n")
        .await
        .expect("can supersede compilation");
    session
        .wait_for_log(BaseMatcher::message(
            "Compilation finished for a superseded source snapshot",
        ))
        .await
        .expect("the compilation is superseded");
    session
        .fs()
        .wait_for_path(session.startup_timeout, &count)
        .await
        .expect("the no-op follow-up runs the deferred test hook");
    assert_eq!(session.fs().read(&count).await.unwrap(), "x\n");

    session.clear_events();
    session
        .fs()
        .append(&source, "lastEdit = ()\n")
        .await
        .unwrap();
    session
        .wait_for_log(BaseMatcher::message("Finished dispatching ghci event"))
        .await
        .expect("ordinary edit is processed");
    assert_eq!(session.fs().read(&count).await.unwrap(), "x\n");
}

/// Test that `ghciwatch` can reload a module that fails to compile.
#[test]
async fn can_reload_after_error() {
    let mut session = GhciWatch::new("tests/data/simple")
        .await
        .expect("ghciwatch starts");
    session
        .wait_until_ready()
        .await
        .expect("ghciwatch loads ghci");
    let new_module = session.path("src/My/Module.hs");

    session
        .fs()
        .write(
            &new_module,
            indoc!(
                "module My.Module (myIdent) where
            myIdent :: ()
            myIdent = \"Uh oh!\"
            "
            ),
        )
        .await
        .unwrap();
    session
        .wait_until_add()
        .await
        .expect("ghciwatch loads new modules");
    session
        .wait_for_log(BaseMatcher::compilation_failed())
        .await
        .unwrap();

    session
        .fs()
        .replace(&new_module, "myIdent = \"Uh oh!\"", "myIdent = ()")
        .await
        .unwrap();

    session
        .wait_until_reload()
        .await
        .expect("ghciwatch reloads on changes");
    session
        .wait_for_log(BaseMatcher::compilation_succeeded())
        .await
        .unwrap();
}

/// Startup source errors leave a usable GHCi prompt, so fixing them should reload the existing
/// package session rather than paying for a complete Cabal restart after every edit.
#[test]
async fn startup_compilation_errors_recover_by_reload() {
    let mut session = GhciWatchBuilder::new("tests/data/simple")
        .before_start(|project| async move {
            test_harness::Fs::new()
                .replace(
                    project.join("src/MyLib.hs"),
                    "example = \"example\"",
                    "example = missingAtStartup",
                )
                .await
        })
        .with_startup_timeout(std::time::Duration::from_secs(20))
        .start()
        .await
        .expect("ghciwatch starts");

    session
        .wait_for_startup_log("Starting up failed")
        .await
        .expect("startup reaches a GHCi prompt with compilation errors");
    session.clear_events();
    session
        .fs()
        .replace(
            session.path("src/MyLib.hs"),
            "example = missingAtStartup",
            "example = \"example\"",
        )
        .await
        .expect("can fix the startup error");
    session
        .wait_for_log("All good! Finished reloading")
        .await
        .expect("fixing edit reloads successfully");
    assert!(
        session
            .assert_logged(BaseMatcher::message("Restarting ghci:\\n"))
            .is_err(),
        "a usable startup session must not restart for an ordinary fixing edit"
    );
}
