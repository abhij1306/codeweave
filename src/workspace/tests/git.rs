use super::*;

#[test]
fn git_diff_continuation_preserves_the_original_scope() {
    let root = tempdir().unwrap();
    run_git(root.path(), &["init", "-q"]);
    run_git(
        root.path(),
        &["config", "user.email", "codeweave@example.test"],
    );
    run_git(root.path(), &["config", "user.name", "CodeWeave Test"]);

    let base = (0..30)
        .map(|index| format!("line-{index:02}-{}", "x".repeat(80)))
        .collect::<Vec<_>>();
    fs::write(root.path().join("a.rs"), format!("{}\n", base.join("\n"))).unwrap();
    fs::write(root.path().join("b.rs"), format!("{}\n", base.join("\n"))).unwrap();
    run_git(root.path(), &["add", "a.rs", "b.rs"]);
    run_git(root.path(), &["commit", "-q", "-m", "baseline"]);

    let mut changed_a = base.clone();
    changed_a[1] = format!("changed-near-start-{}", "y".repeat(80));
    changed_a[25] = format!("changed-near-end-{}", "z".repeat(80));
    fs::write(
        root.path().join("a.rs"),
        format!("{}\n", changed_a.join("\n")),
    )
    .unwrap();
    let mut changed_b = base.clone();
    changed_b[1] = format!("unrelated-change-{}", "q".repeat(80));
    fs::write(
        root.path().join("b.rs"),
        format!("{}\n", changed_b.join("\n")),
    )
    .unwrap();

    let actor = test_actor(root.path());
    let first = actor
        .git(&json!({
            "action": "diff",
            "paths": ["a.rs"],
            "max_chars": 1_200
        }))
        .unwrap();
    assert_eq!(first["truncated"], true);
    assert!(!first["hunks"].as_array().unwrap().is_empty());
    assert!(first["hunks"]
        .as_array()
        .unwrap()
        .iter()
        .all(|hunk| hunk["path"] == "a.rs"));
    let continuation = first["continuation"].as_str().unwrap();

    let second = actor
        .git(&json!({
            "action": "diff",
            "continuation": continuation,
            "max_chars": 5_000
        }))
        .unwrap();
    assert!(second["hunks"]
        .as_array()
        .unwrap()
        .iter()
        .all(|hunk| hunk["path"] == "a.rs"));
    assert!(!second["output"].as_str().unwrap().contains("b.rs"));
    assert_eq!(second["scope"]["paths"], json!(["a.rs"]));

    let error = actor
        .git(&json!({
            "action": "diff",
            "continuation": continuation,
            "paths": ["b.rs"]
        }))
        .unwrap_err();
    assert_eq!(error.0.code, "CONTINUATION_SCOPE_MISMATCH");
}

#[test]
fn git_diff_pagination_advances_past_oversized_hunks() {
    let root = tempdir().unwrap();
    run_git(root.path(), &["init", "-q"]);
    run_git(
        root.path(),
        &["config", "user.email", "codeweave@example.test"],
    );
    run_git(root.path(), &["config", "user.name", "CodeWeave Test"]);

    let base = (0..40)
        .map(|index| format!("line-{index:02}"))
        .collect::<Vec<_>>();
    fs::write(
        root.path().join("large.rs"),
        format!("{}\n", base.join("\n")),
    )
    .unwrap();
    run_git(root.path(), &["add", "large.rs"]);
    run_git(root.path(), &["commit", "-q", "-m", "baseline"]);

    let mut changed = base;
    changed[1] = format!("oversized-{}", "x".repeat(4_000));
    changed[35] = "later-change".to_owned();
    fs::write(
        root.path().join("large.rs"),
        format!("{}\n", changed.join("\n")),
    )
    .unwrap();

    let actor = test_actor(root.path());
    let first = actor
        .git(&json!({"action": "diff", "paths": ["large.rs"], "max_chars": 100}))
        .unwrap();
    assert_eq!(first["hunks"].as_array().unwrap().len(), 1);
    assert!(first["output"].as_str().unwrap().len() > 100);
    let first_id = first["hunks"][0]["id"].as_str().unwrap();
    let continuation = first["continuation"].as_str().unwrap();

    let second = actor
        .git(&json!({"action": "diff", "continuation": continuation, "max_chars": 100}))
        .unwrap();
    assert_eq!(second["hunks"].as_array().unwrap().len(), 1);
    assert_ne!(second["hunks"][0]["id"].as_str().unwrap(), first_id);
    assert!(second["output"].as_str().unwrap().contains("later-change"));
    assert!(second["continuation"].is_null());
}

#[test]
fn push_target_defaults_to_current_branch_and_rejects_git_syntax() {
    assert_eq!(
        validated_push_target(&json!({}), "feature/current").unwrap(),
        ("origin".to_owned(), "feature/current".to_owned())
    );
    assert_eq!(
        validated_push_target(
            &json!({"remote": "upstream", "branch": "feature/explicit"}),
            "feature/current"
        )
        .unwrap(),
        ("upstream".to_owned(), "feature/explicit".to_owned())
    );

    for params in [
        json!({"remote": "--mirror"}),
        json!({"remote": "https://example.com/repository.git"}),
        json!({"branch": ":main"}),
        json!({"branch": "+main"}),
        json!({"branch": "main~1"}),
    ] {
        assert!(validated_push_target(&params, "main").is_err());
    }
    assert!(validated_push_target(&json!({}), "").is_err());
}
