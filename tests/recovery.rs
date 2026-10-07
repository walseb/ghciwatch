use test_harness::test;
use test_harness::BaseMatcher;
use test_harness::GhciWatchBuilder;

/// Missing-module diagnostics have no compilation summary. Skip dependent GHCi hooks,
/// but still compile the fixing edit and deliver shell completion notifications.
#[test]
async fn failed_hook_skips_dependents() {
    let mut session = GhciWatchBuilder::new("tests/data/simple")
        .with_repl_command("ghci -ignore-dot-ghci -fdiagnostics-as-json src/MyLib.hs")
        .with_args([
            "--after-startup-ghci",
            ":m Missing.Hook.Module",
            "--after-startup-ghci",
            ":! touch startup-dependent",
            "--after-startup-shell",
            "touch startup-complete",
            "--before-reload-ghci",
            ":m Missing.Hook.Module",
            "--before-reload-ghci",
            ":! touch reload-dependent",
            "--after-reload-shell",
            "touch reload-complete",
        ])
        .start()
        .await
        .expect("ghciwatch starts");
    session
        .wait_until_ready()
        .await
        .expect("startup completes despite hook error");
    assert!(session.path("startup-complete").exists());
    assert!(!session.path("startup-dependent").exists());
    session.clear_events();
    session
        .fs()
        .append(session.path("src/MyLib.hs"), "\n")
        .await
        .unwrap();
    session
        .wait_for_log(BaseMatcher::reload_completes())
        .await
        .expect("reload completes despite hook error");
    assert!(session.path("reload-complete").exists());
    assert!(!session.path("reload-dependent").exists());
}

/// Restart-only sources remain watched but must never enter the interactive target set.
#[test]
async fn restart_sources_are_not_targets() {
    let mut session = GhciWatchBuilder::new("tests/data/simple")
        .with_repl_command("ghci -ignore-dot-ghci src/MyLib.hs")
        .before_start(|project| async move {
            tokio::fs::create_dir_all(project.join("stable")).await?;
            tokio::fs::write(project.join("stable/Package.hs"), "not valid Haskell\n").await?;
            Ok(())
        })
        .with_args([
            "--watch",
            "stable",
            "--restart-glob",
            "stable/*.hs",
            "--after-startup-shell",
            "touch startup-complete",
        ])
        .start()
        .await
        .expect("ghciwatch starts");
    session
        .wait_until_ready()
        .await
        .expect("startup ignores package source as a target");
    session.clear_events();
    session
        .fs()
        .append(session.path("stable/Package.hs"), "still not Haskell\n")
        .await
        .unwrap();
    session
        .wait_until_restart()
        .await
        .expect("package source still triggers restart");
    session
        .wait_for_log(BaseMatcher::reload_completes())
        .await
        .expect("replacement does not add package source");
    session.clear_events();
    session
        .fs()
        .append(session.path("src/MyLib.hs"), "\n")
        .await
        .unwrap();
    session
        .wait_for_log(BaseMatcher::reload_completes())
        .await
        .expect("ordinary reload still succeeds");
}

/// Startup synchronization must use the replacement command, not a path-based :add.
#[test]
async fn startup_respects_add_replacement() {
    let mut session = GhciWatchBuilder::new("tests/data/simple")
        .with_repl_command("ghci -ignore-dot-ghci src/MyLib.hs")
        .before_start(|project| async move {
            tokio::fs::write(project.join("src/Unmanaged.hs"), "not valid Haskell\n").await?;
            Ok(())
        })
        .with_args(["--replace-auto-add-shell", "touch replaced-add"])
        .start()
        .await
        .expect("ghciwatch starts");
    session
        .wait_until_ready()
        .await
        .expect("initial session loads");
    session.clear_events();
    session
        .fs()
        .append(session.path("my-simple-package.cabal"), "\n")
        .await
        .unwrap();
    session
        .wait_until_restart()
        .await
        .expect("restart reconciles watched sources");
    session
        .wait_for_log(BaseMatcher::reload_completes())
        .await
        .expect("replacement policy prevents unmanaged :add");
    assert!(session.path("replaced-add").exists());
}

/// An initial compilation failure skips startup GHCi hooks. A later successful reload
/// must be able to launch the application without a macro registered by those hooks.
#[test]
async fn reload_cmd_recovers_failed_startup() {
    let mut session = GhciWatchBuilder::new("tests/data/simple")
        .with_repl_command("ghci -ignore-dot-ghci -fdiagnostics-as-json src/MyLib.hs")
        .before_start(|project| async move {
            tokio::fs::write(project.join("src/MyLib.hs"), "module MyLib where\nbroken = missingName\n").await?;
            Ok(())
        })
        .with_args([
            "--after-startup-ghci",
            ":def lr (\\_ -> pure \":! touch application-started\")",
            "--after-reload-ghci",
            ":cmd pure \":! touch application-started\"",
        ])
        .start()
        .await
        .expect("ghciwatch survives failed initial compilation");
    session.wait_until_ready().await.expect("initial load completes");
    assert!(!session.path("application-started").exists());
    session.clear_events();
    session.fs().write(
        session.path("src/MyLib.hs"),
        "module MyLib where\nfixed = ()\n",
    ).await.unwrap();
    session.wait_for_log(BaseMatcher::reload_completes()).await.expect("fix reload completes");
    assert!(session.path("application-started").exists());
}
