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
