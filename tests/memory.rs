use test_harness::{test, BaseMatcher, GhciWatchBuilder};

#[test]
async fn records_startup_failure_and_recovery_without_stale_rts() {
    // Exercise the stdout protocol through real lifecycle hooks, not a separate query.
    let payload = serde_json::json!({
        "pid": 1, "gcs": 3, "major_gcs": 1, "last_gc_generation": 0,
        "live_bytes": 1024, "heap_bytes": 4096, "allocated_bytes": 8192,
        "gc_allocated_bytes": 2048, "max_live_bytes": 1024,
        "gc_cpu_ns": 10, "gc_elapsed_ns": 20,
        "process_rss_bytes": 8192, "process_swap_bytes": 0, "process_lazy_free_bytes": 1024,
    });
    let hook = format!("putStrLn {:?}", format!("GHCIWATCH_MEMORY {payload}"));
    let mut session = GhciWatchBuilder::new("tests/data/simple")
        .with_args([
            "--memory-file",
            "memory.txt",
            "--after-startup-ghci",
            &hook,
            "--after-reload-ghci",
            &hook,
        ])
        .start()
        .await
        .unwrap();
    session.wait_until_ready().await.unwrap();
    let source = session.path("src/MyLib.hs");
    let original = session.fs().read(&source).await.unwrap();
    session
        .fs()
        .append(&source, "\nmemoryProbe =\n")
        .await
        .unwrap();
    session.wait_until_reload().await.unwrap();
    session
        .wait_for_log(BaseMatcher::message("Reloading failed"))
        .await
        .unwrap();
    session.fs().write(&source, original).await.unwrap();
    session.wait_until_reload().await.unwrap();
    session
        .wait_for_log(BaseMatcher::reload_completes())
        .await
        .unwrap();

    let contents = session.fs().read(session.path("memory.txt")).await.unwrap();
    let records: Vec<serde_json::Value> = contents
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(records.len(), 3);
    for (generation, record) in records.iter().enumerate() {
        assert_eq!(record["generation"], generation);
        assert_eq!(record["session"], records[0]["session"]);
    }
    assert_eq!(records[0]["rts"], payload);
    for record in &records {
        assert_eq!(record["version"], 2);
        assert!(record["workload"]["duration_seconds"].as_f64().unwrap() > 0.0);
        assert!(
            record["workload"]["module_compile_attempts"]
                .as_u64()
                .unwrap()
                > 0
        );
        // This direct-GHC fixture has no Cabal pair or RTS -S file: unavailable is explicit.
        assert!(record["major_gc"].is_null());
        assert!(record["major_gc_error"].is_string());
    }
    assert!(records[0]["workload"]["changed_paths"]
        .as_array()
        .unwrap()
        .is_empty());
    for record in &records[1..] {
        assert!(record["workload"]["changed_paths"]
            .as_array()
            .unwrap()
            .iter()
            .any(|path| path.as_str().unwrap().ends_with("src/MyLib.hs")));
    }
    assert_eq!(records[0]["rts_status"], "minor_gc_inconclusive");
    assert_eq!(records[1]["outcome"], "failed");
    assert!(records[1]["rts"].is_null());
    assert_eq!(records[1]["rts_status"], "unavailable");
    assert_eq!(records[2]["outcome"], "success");
    assert_eq!(records[2]["rts"], payload);
}
