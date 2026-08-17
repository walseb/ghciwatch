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
        .with_arg("--no-auto-reload")
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
