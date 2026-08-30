use std::time::Duration;

use indoc::indoc;
use test_harness::test;
use test_harness::BaseMatcher;
use test_harness::GhciWatchBuilder;

/// A replacement command stands in for `:add` once, while later edits to the intentionally
/// non-target source use ordinary `:reload`.
#[test]
async fn replaces_auto_add_and_then_reloads_known_source() {
    let mut session = GhciWatchBuilder::new("tests/data/simple")
        .with_args([
            "--replace-auto-add-shell",
            "sh -c 'echo replace >> replace-auto-add.log'",
        ])
        .with_startup_timeout(Duration::from_secs(25))
        .start()
        .await
        .expect("ghciwatch starts");

    session.wait_until_ready().await.unwrap();
    session
        .fs()
        .write(
            session.path("src/MyModule.hs"),
            indoc!(
                r#"
                module MyModule where

                value :: Int
                value = 1
                "#
            ),
        )
        .await
        .expect("can create a module");

    session
        .wait_for_log(BaseMatcher::message(
            "Running replacement for automatic module addition",
        ))
        .await
        .expect("replacement command runs for a new non-target source");
    session
        .wait_for_log(BaseMatcher::reload_completes())
        .await
        .expect("replacement is followed by reload completion");

    session.clear_events();
    session
        .fs()
        .replace(session.path("src/MyModule.hs"), "value = 1", "value = 2")
        .await
        .expect("can modify the intentionally non-target source");
    session
        .wait_for_log(BaseMatcher::reload_completes())
        .await
        .expect("later modification reloads");

    let replacements = session
        .fs()
        .read(session.path("replace-auto-add.log"))
        .await
        .unwrap();
    assert_eq!(
        replacements, "replace\n",
        "replacement runs only for addition"
    );
}

/// A replacement generator can modify an already watched aggregate while handling a new module.
/// The queued aggregate notification starts another lifecycle attempt, whose before-hook removes the
/// error file; the follow-up attempt must publish the file again even when its work is redundant.
#[test]
async fn replacement_generated_follow_up_republishes_error_file() {
    let mut session = GhciWatchBuilder::new("tests/data/simple")
        .with_args([
            "--error-file",
            "compile.txt",
            "--before-reload-shell",
            "rm -f compile.txt",
            "--replace-auto-add-shell",
            r#"sh -c 'printf "\n-- generated aggregate update\n" >> src/MyLib.hs'"#,
            "--after-reload-shell",
            r#"sh -c 'echo after >> after-reload.log; if test "$(wc -l < after-reload.log)" -ge 2 && test -e compile.txt; then touch follow-up-complete; fi'"#,
        ])
        .with_startup_timeout(Duration::from_secs(25))
        .start()
        .await
        .expect("ghciwatch starts");

    session.wait_until_ready().await.unwrap();
    session
        .fs()
        .write(
            session.path("src/MyGeneratedModule.hs"),
            "module MyGeneratedModule where\ngeneratedValue = ()\n",
        )
        .await
        .expect("can create a module");

    session
        .fs()
        .wait_for_path(
            session.startup_timeout,
            &session.path("follow-up-complete"),
        )
        .await
        .expect("replacement-generated follow-up republishes compile.txt");
    assert!(
        session.path("compile.txt").exists(),
        "compile.txt remains present after the follow-up lifecycle completes"
    );
}
