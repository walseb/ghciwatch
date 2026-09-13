use std::time::Duration;

use test_harness::{BaseMatcher, Fs, GhciWatchBuilder};

#[test_harness::test(current)]
async fn identical_saves_do_not_reload_or_restart_even_after_startup() {
    let mut session = GhciWatchBuilder::new("tests/data/simple")
        .with_repl_command("ghc --interactive -ignore-dot-ghci src/MyLib.hs")
        .with_args([
            "--reload-glob",
            "**/*.txt",
            "--before-startup-shell",
            "sh -c 'echo launch >> launches'",
            "--after-reload-shell",
            "sh -c 'echo reload >> reloads'",
        ])
        .before_start(
            |path| async move { Fs::new().write(path.join("src/config.txt"), "one").await },
        )
        .start()
        .await
        .unwrap();
    session.wait_until_ready().await.unwrap();

    for name in ["src/MyLib.hs", "src/config.txt", "my-simple-package.cabal"] {
        let path = session.path(name);
        let original = session.fs().read(&path).await.unwrap();
        session.fs().touch(&path).await.unwrap();
        session.fs().write(&path, &original).await.unwrap();
        // Atomic replacement with identical bytes must also be ignored.
        let temporary = session.path("replacement");
        session.fs().write(&temporary, &original).await.unwrap();
        tokio::fs::rename(temporary, &path).await.unwrap();
    }
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(!session.path("reloads").exists());
    assert_eq!(
        session.fs().read(session.path("launches")).await.unwrap(),
        "launch\n"
    );

    session.clear_events();
    session
        .fs()
        .append(session.path("src/MyLib.hs"), "\n-- real edit\n")
        .await
        .unwrap();
    session
        .wait_for_log(BaseMatcher::reload_completes())
        .await
        .unwrap();
    assert_eq!(
        session.fs().read(session.path("reloads")).await.unwrap(),
        "reload\n"
    );

    session.clear_events();
    session
        .fs()
        .write(session.path("src/config.txt"), "two")
        .await
        .unwrap();
    session
        .wait_for_log(BaseMatcher::reload_completes())
        .await
        .unwrap();
    assert_eq!(
        session.fs().read(session.path("reloads")).await.unwrap(),
        "reload\nreload\n"
    );

    session.clear_events();
    session
        .fs()
        .append(session.path("my-simple-package.cabal"), "\n")
        .await
        .unwrap();
    session
        .wait_for_log(BaseMatcher::reload_completes())
        .await
        .unwrap();
    assert_eq!(
        session.fs().read(session.path("launches")).await.unwrap(),
        "launch\nlaunch\n"
    );
}
