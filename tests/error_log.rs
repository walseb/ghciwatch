use expect_test::expect;
use indoc::indoc;

use test_harness::test;
use test_harness::BaseMatcher;
use test_harness::Fs;
use test_harness::GhcVersion;
use test_harness::GhciWatchBuilder;

/// Test that `ghciwatch --errors ...` can write the error log.
#[test]
async fn can_write_error_log() {
    let error_path = "ghcid.txt";
    let mut session = GhciWatchBuilder::new("tests/data/simple")
        .with_args(["--errors", error_path])
        .start()
        .await
        .expect("ghciwatch starts");
    let error_path = session.path(error_path);
    session
        .wait_until_ready()
        .await
        .expect("ghciwatch loads ghci");
    let error_contents = session
        .fs()
        .read(&error_path)
        .await
        .expect("ghciwatch writes ghcid.txt");
    expect![[r#"
        All good (1 module)
    "#]]
    .assert_eq(&error_contents);
}

/// Recursive-module diagnostics emitted immediately on stderr are preserved in `--error-file`.
#[test]
async fn can_write_error_log_recursive_module_errors() {
    let error_path = "ghcid.txt";
    let mut session = GhciWatchBuilder::new("tests/data/simple")
        .with_args(["--error-file", error_path])
        .start()
        .await
        .expect("ghciwatch starts");
    let error_path = session.path(error_path);
    session
        .wait_until_ready()
        .await
        .expect("ghciwatch loads ghci");

    session
        .fs()
        .replace(
            session.path("src/MyLib.hs"),
            "module MyLib (example) where",
            "module MyLib (example) where\n\nimport MyLib",
        )
        .await
        .expect("can make MyLib import itself");

    session
        .wait_for_log(BaseMatcher::span_close().in_leaf_spans(["error_log_write"]))
        .await
        .expect("ghciwatch writes ghcid.txt");
    session
        .wait_for_log(BaseMatcher::reload_completes())
        .await
        .expect("ghciwatch finishes reloading");

    let error_contents = session
        .fs()
        .read(&error_path)
        .await
        .expect("ghciwatch writes ghcid.txt");
    assert!(
        error_contents.contains("src/MyLib.hs: error:")
            && error_contents.contains("imports itself"),
        "recursive-module diagnostic missing from error log: {error_contents:?}"
    );
}

/// Test that `ghciwatch --errors ...` can write the error log with `--repl-no-load`.
#[test]
async fn can_write_error_log_repl_no_load() {
    let error_path = "ghcid.txt";
    let mut session = GhciWatchBuilder::new("tests/data/simple")
        .with_args(["--errors", error_path])
        .with_cabal_arg("--repl-no-load")
        .start()
        .await
        .expect("ghciwatch starts");
    let error_path = session.path(error_path);
    session
        .wait_until_ready()
        .await
        .expect("ghciwatch loads ghci");
    let error_contents = session
        .fs()
        .read(&error_path)
        .await
        .expect("ghciwatch writes ghcid.txt");
    expect![[r#"
        All good (0 modules)
    "#]]
    .assert_eq(&error_contents);
}

/// Test that `ghciwatch --errors ...` can write compilation errors.
/// Then, test that it can reload when modules are changed and will correctly rewrite the error log
/// once it's fixed.
#[test]
async fn can_write_error_log_compilation_errors() {
    let error_path = "ghcid.txt";
    let mut session = GhciWatchBuilder::new("tests/data/simple")
        .with_args(["--errors", error_path])
        .start()
        .await
        .expect("ghciwatch starts");
    let error_path = session.path(error_path);
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
        .wait_for_log(BaseMatcher::span_close().in_leaf_spans(["error_log_write"]))
        .await
        .expect("ghciwatch writes ghcid.txt");

    session
        .wait_for_log(BaseMatcher::reload_completes())
        .await
        .expect("ghciwatch finishes reloading");

    let error_contents = session
        .fs()
        .read(&error_path)
        .await
        .expect("ghciwatch writes ghcid.txt");

    expect![[r#"
            src/My/Module.hs:3:11: error: [GHC-83865]
                * Couldn't match type `[Char]' with `()'
                  Expected: ()
                    Actual: String
                * In the expression: "Uh oh!"
                  In an equation for `myIdent': myIdent = "Uh oh!"
              |
            3 | myIdent = "Uh oh!"
              |           ^^^^^^^^
        "#]]
    .assert_eq(&error_contents);

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
        .wait_for_log(BaseMatcher::span_close().in_leaf_spans(["error_log_write"]))
        .await
        .expect("ghciwatch writes ghcid.txt");

    let error_contents = session
        .fs()
        .read(&error_path)
        .await
        .expect("ghciwatch writes ghcid.txt");

    expect![[r#"
        All good (2 modules)
    "#]]
    .assert_eq(&error_contents);
}

/// Test that `ghciwatch --errors ...` preserves paths emitted by GHC.
#[test]
async fn preserves_error_log_paths() {
    let error_path = "ghcid.txt";
    let mut session = GhciWatchBuilder::new("tests/data/with-dep")
        .with_args(["--errors", error_path, "--watch", "simple-dep/src"])
        .with_cabal_target("simple-dep")
        .start()
        .await
        .expect("ghciwatch starts");
    let error_path = session.path(error_path);
    session
        .wait_until_ready()
        .await
        .expect("ghciwatch loads ghci");

    session
        .fs()
        .replace(
            session.path("simple-dep/src/SimpleDep.hs"),
            "\"depFunc\"",
            "\"depFunc",
        )
        .await
        .expect("can break simple-dep");

    session
        .wait_for_log(BaseMatcher::span_close().in_leaf_spans(["error_log_write"]))
        .await
        .expect("ghciwatch writes ghcid.txt");

    session
        .wait_for_log(BaseMatcher::reload_completes())
        .await
        .expect("ghciwatch finishes reloading");

    let error_contents = session
        .fs()
        .read(&error_path)
        .await
        .expect("ghciwatch writes ghcid.txt");

    // GHCi's working directory is the package, so GHC emits paths relative to it.
    let expected = match session.ghc_version() {
        GhcVersion::Ghc96 | GhcVersion::Ghc98 | GhcVersion::Ghc910 => expect![[r#"
            src/SimpleDep.hs:4:28: error: [GHC-21231]
                lexical error in string/character literal at character '\n'
              |
            4 | depFunc = putStrLn "depFunc
              |                            ^
        "#]],
        GhcVersion::Ghc912 => expect![[r#"
            src/SimpleDep.hs:4:20: error: [GHC-21231]
                lexical error at character '\n'
              |
            4 | depFunc = putStrLn "depFunc
              |                    ^^^^^^^^
        "#]],
    };

    expected.assert_eq(&error_contents);
}

/// Diagnostics written only to stderr are captured when the command exits before GHCi boots.
#[test]
async fn error_log_pre_ghci_stderr_failure() {
    let error_path = "ghcid.txt";
    let command = r#"sh -c 'printf "%s\n" "src/Early.hs:1:1: error:" "    plugin failed before GHCi startup" >&2; exit 1'"#;
    let mut session = GhciWatchBuilder::new("tests/data/simple")
        .with_args(["--errors", error_path])
        .with_repl_command(command)
        .start()
        .await
        .expect("ghciwatch starts");
    let error_path = session.path(error_path);

    session
        .wait_for_startup_log("ghci exited during startup")
        .await
        .expect("ghciwatch detects the pre-GHCi startup failure");

    let error_contents = session
        .fs()
        .read(&error_path)
        .await
        .expect("ghciwatch writes ghcid.txt");
    expect![[r#"
        src/Early.hs:1:1: error:
            plugin failed before GHCi startup
    "#]]
    .assert_eq(&error_contents);
}

/// Plain Cabal failures before the GHCi banner become errors and are visible to startup hooks.
#[test]
async fn error_log_pre_ghci_plain_failure_runs_hook() {
    let error_path = "ghcid.txt";
    let command = r#"sh -c 'printf "%s\n" "Error: [Cabal-7125]" "configure failed before GHCi startup" >&2; exit 1'"#;
    let mut session = GhciWatchBuilder::new("tests/data/simple")
        .with_args([
            "--errors",
            error_path,
            "--after-startup-shell",
            "sh -c 'grep -q Cabal-7125 ghcid.txt && touch early-startup-hook'",
        ])
        .with_repl_command(command)
        .start()
        .await
        .expect("ghciwatch starts");

    session
        .wait_for_startup_log("ghci exited during startup")
        .await
        .expect("ghciwatch detects the plain pre-GHCi failure");
    session
        .fs()
        .wait_for_path(
            session.startup_timeout,
            &session.path("early-startup-hook"),
        )
        .await
        .expect("after-startup hook observes the early error log");

    let error_contents = session
        .fs()
        .read(session.path(error_path))
        .await
        .expect("ghciwatch writes the plain startup failure");
    assert!(
        error_contents.contains("<no location info>: error:")
            && error_contents.contains("Cabal-7125")
            && error_contents.contains("configure failed before GHCi startup"),
        "plain startup diagnostic missing from error log: {error_contents:?}"
    );
}

/// A restart-time Cabal build failure updates the error file before exit handling continues.
#[test]
async fn error_log_restart_failure_before_ghci() {
    let error_path = "ghcid.txt";
    let mut session = GhciWatchBuilder::new("tests/data/with-dep")
        .with_args([
            "--errors",
            error_path,
            "--watch",
            "simple-dep/src",
            "--restart-glob",
            "simple-dep/src/*.hs",
        ])
        .start()
        .await
        .expect("ghciwatch starts");
    let error_path = session.path(error_path);
    session
        .wait_until_ready()
        .await
        .expect("ghciwatch loads ghci");

    session
        .fs()
        .replace(
            session.path("simple-dep/src/SimpleDep.hs"),
            "\"depFunc\"",
            "\"depFunc",
        )
        .await
        .expect("can break the dependency before restart");
    session
        .wait_for_log("ghci exited unexpectedly")
        .await
        .expect("ghciwatch detects the failed restart");

    let error_contents = session
        .fs()
        .read(&error_path)
        .await
        .expect("ghciwatch updates ghcid.txt after the failed restart");
    assert!(
        error_contents.contains("src/SimpleDep.hs:4:") && error_contents.contains("lexical error"),
        "restart diagnostic missing from error log: {error_contents:?}"
    );
}

#[test]
async fn error_log_startup_failure() {
    let error_path = "ghcid.txt";
    let mut session = GhciWatchBuilder::new("tests/data/with-dep")
        .with_args(["--errors", error_path])
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
        .start()
        .await
        .expect("ghciwatch starts");
    let error_path = session.path(error_path);

    // First startup fails because simple-dep won't compile.
    // NB: This message means that ghciwatch didn't crash!
    session
        .wait_for_startup_log("ghci exited during startup")
        .await
        .expect("ghciwatch detects first startup failure");

    let error_contents = session
        .fs()
        .read(&error_path)
        .await
        .expect("ghciwatch writes ghcid.txt");

    // We don't have access to the package's directory here so we can't fix these paths!
    // These _should_ be like `simple-dep/src/SimpleDep.hs` but GHC doesn't emit them relative to
    // the invocation so users are just Fucked.
    let expected = match session.ghc_version() {
        GhcVersion::Ghc96 | GhcVersion::Ghc98 | GhcVersion::Ghc910 => expect![[r#"
            src/SimpleDep.hs:4:28: error: [GHC-21231]
                lexical error in string/character literal at character '\n'
              |
            4 | depFunc = putStrLn "depFunc
              |                            ^
        "#]],
        GhcVersion::Ghc912 => expect![[r#"
            src/SimpleDep.hs:4:20: error: [GHC-21231]
                lexical error at character '\n'
              |
            4 | depFunc = putStrLn "depFunc
              |                    ^^^^^^^^
        "#]],
    };

    expected.assert_eq(&error_contents);
}
