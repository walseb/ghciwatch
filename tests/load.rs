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
    let mut session = GhciWatchBuilder::new("tests/data/simple")
        .with_startup_timeout(std::time::Duration::from_secs(20))
        .with_args([
            "--after-reload-ghci",
            ":show targets",
            "--after-reload-shell",
            "touch add-after-reload",
        ])
        .start()
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
    session
        .wait_for_log(
            BaseMatcher::message("Read suppressed stderr line")
                .with_field("line", ".*Top-level binding with no type signature.*"),
        )
        .await
        .expect("named-add diagnostics are captured before authoritative replay");
    session
        .wait_for_log(
            BaseMatcher::message("Running after-reload command")
                .with_field("command", "^:show targets$"),
        )
        .await
        .expect("after-reload GHCi hook runs after adding a module");
    session
        .wait_for_log(
            BaseMatcher::message("Read line")
                .with_field("line", "^(My\\.Module|src/My/Module\\.hs)$"),
        )
        .await
        .expect("after-reload GHCi hook sees the new target");
    session
        .fs()
        .wait_for_path(session.startup_timeout, &session.path("add-after-reload"))
        .await
        .expect("after-reload shell hook runs after adding a module");
}

/// A name derived from an extra search path may not be resolvable in GHCi's current home unit.
/// Such additions must be retried by source path.
#[test]
async fn falls_back_to_path_when_named_add_is_unresolved() {
    let mut session = GhciWatchBuilder::new("tests/data/simple")
        .before_start(|project| async move {
            test_harness::Fs::new()
                .create_dir(project.join("extra"))
                .await
        })
        .with_startup_timeout(std::time::Duration::from_secs(20))
        .with_args(["--watch", "extra", "--extra-module-search-path", "extra"])
        .start()
        .await
        .expect("ghciwatch starts");
    session.wait_until_ready().await.unwrap();

    session
        .fs()
        .write(
            session.path("extra/FallbackModule.hs"),
            "module FallbackModule where\nfallbackValue = ()\n",
        )
        .await
        .unwrap();
    session
        .wait_until_add()
        .await
        .expect("path fallback adds the module");
    session
        .wait_for_log(BaseMatcher::message(
            "Retrying modules unresolved by named :add as source paths:\n",
        ))
        .await
        .expect("unresolved name is retried by path");
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
        .wait_for_log("Reloading failed")
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
        .wait_for_log("All good! Finished reloading")
        .await
        .expect("fixing edit replaces the incomplete session");
}
