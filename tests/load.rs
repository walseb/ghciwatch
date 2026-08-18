use indoc::indoc;

use test_harness::test;
use test_harness::BaseMatcher;
use test_harness::GhciWatch;
use test_harness::GhciWatchBuilder;

/// Test that `ghciwatch` can start up `ghci` and load a session.
#[test]
async fn can_load() {
    let mut session = GhciWatch::new("tests/data/simple")
        .await
        .expect("ghciwatch starts");
    session
        .wait_until_ready()
        .await
        .expect("ghciwatch loads ghci");
}

/// Test that `ghciwatch` can load new modules.
#[test]
async fn can_load_new_module() {
    let mut session = GhciWatch::new("tests/data/simple")
        .await
        .expect("ghciwatch starts");
    session
        .wait_until_ready()
        .await
        .expect("ghciwatch loads ghci");
    session
        .fs()
        .write(
            session.path("src/My/Module.hs"),
            indoc!(
                "module My.Module (myIdent) where
            myIdent :: ()
            myIdent = ()
            "
            ),
        )
        .await
        .unwrap();
    session
        .wait_until_add()
        .await
        .expect("ghciwatch loads new modules");
}

/// Package-managed sessions can rebuild their component graph instead of path-adding modules.
#[test]
async fn can_restart_for_new_module() {
    let mut session = GhciWatchBuilder::new("tests/data/simple")
        .with_arg("--restart-on-add")
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
        .write(
            session.path("src/NewPackageModule.hs"),
            "module NewPackageModule where\nnewValue = ()\n",
        )
        .await
        .unwrap();
    session
        .wait_for_log(BaseMatcher::message("Restarting ghci:\\n"))
        .await
        .expect("ghciwatch restarts for a new module");
}

/// A replacement can return to its prompt after a home-module error. Such a partial session must
/// restart on the fixing edit even when ordinary automatic reloads are disabled.
#[test]
async fn failed_add_restart_recovers_with_no_auto_reload() {
    let mut session = GhciWatchBuilder::new("tests/data/simple")
        .with_args(["--restart-on-add", "--no-auto-reload"])
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
        .replace(
            session.path("src/MyLib.hs"),
            "example = \"example\"",
            "example = missingName",
        )
        .await
        .expect("can break a home module");
    session
        .fs()
        .write(
            session.path("src/NewPackageModule.hs"),
            "module NewPackageModule where\nnewValue = ()\n",
        )
        .await
        .expect("can add a module");
    session
        .wait_for_log("Restarting failed")
        .await
        .expect("replacement reaches its prompt with a compilation failure");

    session.clear_events();
    session
        .fs()
        .replace(
            session.path("src/MyLib.hs"),
            "example = missingName",
            "example = \"example\"",
        )
        .await
        .expect("can fix the home module");
    session
        .wait_for_log("All good! Finished restarting")
        .await
        .expect("fixing edit replaces the incomplete session");
}
