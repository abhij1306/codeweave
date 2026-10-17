use super::*;
use crate::error::ErrorCode;

#[tokio::test]
async fn git_diff_continuation_preserves_the_original_scope() {
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
        .git(git_action(&json!({
            "action": "diff",
            "paths": ["a.rs"],
            "max_chars": 1_200
        })))
        .await
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
        .git(git_action(&json!({
            "action": "diff",
            "continuation": continuation,
            "max_chars": 5_000
        })))
        .await
        .unwrap();
    assert!(second["hunks"]
        .as_array()
        .unwrap()
        .iter()
        .all(|hunk| hunk["path"] == "a.rs"));
    assert!(!second["output"].as_str().unwrap().contains("b.rs"));
    assert_eq!(second["scope"]["paths"], json!(["a.rs"]));

    let error = actor
        .git(git_action(&json!({
            "action": "diff",
            "continuation": continuation,
            "paths": ["b.rs"]
        })))
        .await
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::ContinuationScopeMismatch);
}

#[tokio::test]
async fn git_diff_pagination_advances_past_oversized_hunks() {
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
        .git(git_action(
            &json!({"action": "diff", "paths": ["large.rs"], "max_chars": 100}),
        ))
        .await
        .unwrap();
    assert_eq!(first["hunks"].as_array().unwrap().len(), 1);
    assert!(first["output"].as_str().unwrap().len() > 100);
    let first_id = first["hunks"][0]["id"].as_str().unwrap();
    let continuation = first["continuation"].as_str().unwrap();

    let second = actor
        .git(git_action(
            &json!({"action": "diff", "continuation": continuation, "max_chars": 100}),
        ))
        .await
        .unwrap();
    assert_eq!(second["hunks"].as_array().unwrap().len(), 1);
    assert_ne!(second["hunks"][0]["id"].as_str().unwrap(), first_id);
    assert!(second["output"].as_str().unwrap().contains("later-change"));
    assert!(second["continuation"].is_null());
}

#[test]
fn push_target_defaults_to_current_branch_and_rejects_git_syntax() {
    assert_eq!(
        validated_push_target(&push_request(&json!({})), "feature/current").unwrap(),
        ("origin".to_owned(), "feature/current".to_owned())
    );
    assert_eq!(
        validated_push_target(
            &push_request(&json!({"remote": "upstream", "branch": "feature/explicit"})),
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
        assert!(validated_push_target(&push_request(&params), "main").is_err());
    }
    assert!(validated_push_target(&push_request(&json!({})), "").is_err());
}

const SCRIPTED_HEADER: &str = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n";
const SCRIPTED_DIFF: &str = concat!(
    "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n",
    "@@ -2 +2 @@\n-two\n+TWO\n",
    "@@ -20 +20 @@\n-twenty\n+TWENTY\n",
    "@@ -40 +40 @@\n-forty\n+FORTY\n",
);

/// A workspace whose repository answers every diff with [`SCRIPTED_DIFF`] and
/// whose `a.rs` declares `twenty` on lines 19-21.
fn scripted_diff_workspace(root: &Path) -> (Arc<Workspace>, Arc<ScriptedRepository>) {
    let mut source = (1..=18)
        .map(|line| format!("// line {line}\n"))
        .collect::<String>();
    source.push_str("fn twenty() {\n    let _ = 20;\n}\n");
    fs::write(root.join("a.rs"), source).unwrap();
    let repository = ScriptedRepository::new(RepoStatus {
        is_git: true,
        ..RepoStatus::default()
    });
    repository.diff_with(SCRIPTED_DIFF);
    let actor = test_actor_with_repository(root, Arc::clone(&repository));
    (actor, repository)
}

fn new_start_lines(response: &Value) -> Vec<u64> {
    response["hunks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hunk| hunk["new"]["start_line"].as_u64().unwrap())
        .collect()
}

#[tokio::test]
async fn git_diff_line_focus_returns_only_overlapping_hunks() {
    let root = tempdir().unwrap();
    let (actor, _) = scripted_diff_workspace(root.path());

    let focused = actor
        .git(git_action(&json!({
            "action": "diff", "paths": ["a.rs"], "start_line": 18, "end_line": 22
        })))
        .await
        .unwrap();

    assert_eq!(new_start_lines(&focused), [20]);
    assert_eq!(
        focused["output"],
        format!("{SCRIPTED_HEADER}@@ -20 +20 @@\n-twenty\n+TWENTY\n")
    );
    assert_eq!(focused["truncated"], false);
}

#[tokio::test]
async fn git_diff_hunk_ids_select_hunks_from_an_earlier_response() {
    let root = tempdir().unwrap();
    let (actor, _) = scripted_diff_workspace(root.path());
    let all = actor
        .git(git_action(&json!({"action": "diff", "paths": ["a.rs"]})))
        .await
        .unwrap();
    assert_eq!(new_start_lines(&all), [2, 20, 40]);
    let last = all["hunks"][2]["id"].clone();

    let selected = actor
        .git(git_action(
            &json!({"action": "diff", "paths": ["a.rs"], "hunk_ids": [last]}),
        ))
        .await
        .unwrap();

    assert_eq!(new_start_lines(&selected), [40]);
}

#[tokio::test]
async fn git_diff_symbol_focus_uses_the_indexed_declaration_range() {
    let root = tempdir().unwrap();
    let (actor, _) = scripted_diff_workspace(root.path());

    let focused = actor
        .git(git_action(
            &json!({"action": "diff", "paths": ["a.rs"], "symbol": "twenty"}),
        ))
        .await
        .unwrap();
    assert_eq!(new_start_lines(&focused), [20]);

    let unknown = actor
        .git(git_action(
            &json!({"action": "diff", "paths": ["a.rs"], "symbol": "missing"}),
        ))
        .await
        .unwrap_err();
    assert_eq!(unknown.code(), ErrorCode::SymbolNotFound);

    let without_path = actor
        .git(git_action(&json!({"action": "diff", "symbol": "twenty"})))
        .await
        .unwrap_err();
    assert_eq!(without_path.code(), ErrorCode::InvalidArgument);
    assert_eq!(
        without_path.message(),
        "git_diff symbol focus requires exactly one path"
    );
}

#[tokio::test]
async fn git_diff_pages_replay_the_original_scope_until_the_snapshot_moves() {
    let root = tempdir().unwrap();
    let (actor, repository) = scripted_diff_workspace(root.path());

    let first = actor
        .git(git_action(
            &json!({"action": "diff", "paths": ["a.rs"], "max_chars": 80}),
        ))
        .await
        .unwrap();
    assert_eq!(new_start_lines(&first), [2]);
    let second = actor
        .git(git_action(&json!({
            "action": "diff", "continuation": first["continuation"], "max_chars": 80
        })))
        .await
        .unwrap();
    assert_eq!(new_start_lines(&second), [20]);
    assert_eq!(second["truncated"], true);
    assert_eq!(
        repository.diff_requests(),
        [
            (false, vec!["a.rs".to_owned()]),
            (false, vec!["a.rs".to_owned()])
        ]
    );

    fs::write(root.path().join("b.rs"), "fn b() {}\n").unwrap();
    actor.refresh(true).unwrap();
    let stale = actor
        .git(git_action(&json!({
            "action": "diff", "continuation": second["continuation"], "max_chars": 80
        })))
        .await
        .unwrap_err();
    assert_eq!(stale.code(), ErrorCode::StaleContinuation);
}
