use super::*;

/// Publish a repository status with these dirty files through the state writer.
fn set_dirty_files(actor: &Workspace, dirty_files: Vec<String>) {
    actor
        .state
        .writer()
        .update(|state| {
            state.apply_repo_status(Ok(RepoStatus {
                dirty_files,
                ..RepoStatus::default()
            }))
        })
        .unwrap();
}

#[tokio::test]
async fn stale_snapshot_rebases_when_file_hash_is_current() {
    let root = tempdir().unwrap();
    let original = "fn value() -> i32 { 1 }\n";
    fs::write(root.path().join("value.rs"), original).unwrap();
    let actor = test_actor(root.path());
    let old_snapshot = actor.state.read().stamp().snapshot_id;
    fs::write(root.path().join("unrelated.rs"), "fn unrelated() {}\n").unwrap();
    actor.refresh(true).unwrap();
    let result = actor
        .code_edit(edit_request(&json!({
            "snapshot_id": old_snapshot,
            "preview": true,
            "changes": [{
                "kind": "replace",
                "path": "value.rs",
                "old_text": "{ 1 }",
                "new_text": "{ 2 }",
                "expected_hash": content_hash(original)
            }]
        })))
        .await
        .unwrap();
    assert_eq!(result["preview"], true);
    assert!(result["snapshot_rebased_from"].is_string());
}

#[test]
fn summary_caps_large_instruction_files() {
    let root = tempdir().unwrap();
    let big = format!("{}étail", "a".repeat(4_095));
    assert!(big.len() > 4_096);
    fs::write(root.path().join("AGENTS.md"), &big).unwrap();
    fs::write(root.path().join("CLAUDE.md"), "short and sweet\n").unwrap();
    let actor = test_actor(root.path());
    let summary = actor.summary().unwrap();
    let instructions = summary["instructions"].as_array().unwrap();

    let agents = instructions
        .iter()
        .find(|entry| entry["path"] == "AGENTS.md")
        .unwrap();
    assert_eq!(agents["content_truncated"], true);
    assert_eq!(agents["content_bytes"], big.len());
    assert!(agents["content"].as_str().unwrap().len() <= 4_096);
    assert_eq!(agents["content"].as_str().unwrap().len(), 4_095);

    let claude = instructions
        .iter()
        .find(|entry| entry["path"] == "CLAUDE.md")
        .unwrap();
    assert!(claude.get("content_truncated").is_none());
    assert_eq!(claude["content"], "short and sweet\n");
}

#[test]
fn dirty_ownership_tracks_only_current_dirty_mcp_paths() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("still_dirty.rs"), "fn dirty() {}\n").unwrap();
    fs::write(root.path().join("clean_now.rs"), "fn clean() {}\n").unwrap();
    let actor = test_actor(root.path());
    let generation = actor.state.read().generation();
    actor.state.ledger().record(&[
        mutation("still_dirty.rs", "mcp_edit", generation),
        mutation("clean_now.rs", "mcp_edit", generation),
    ]);
    set_dirty_files(&actor, vec!["still_dirty.rs".to_owned()]);

    let summary = actor.summary().unwrap();
    let changed = summary["dirty_ownership"]["changed_by_mcp"]
        .as_array()
        .unwrap();

    assert_eq!(changed.len(), 1);
    assert_eq!(changed[0], "still_dirty.rs");
}

#[test]
fn changes_treats_since_generation_as_exclusive() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("value.rs"), "fn value() {}\n").unwrap();
    let actor = test_actor(root.path());
    actor
        .state
        .ledger()
        .record(&[mutation("value.rs", "mcp_edit", 7)]);

    let after_six = actor.changes(6, None, 200).unwrap();
    assert_eq!(after_six["mutations"].as_array().unwrap().len(), 1);

    let after_seven = actor.changes(7, None, 200).unwrap();
    assert!(after_seven["mutations"].as_array().unwrap().is_empty());
}

#[test]
fn changes_are_shared_across_clients() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("value.rs"), "fn value() {}\n").unwrap();
    let actor = test_actor(root.path());
    let generation = actor.state.read().generation();
    for client in ["client-a", "client-b"] {
        actor
            .state
            .ledger()
            .record(&[mutation(&format!("{client}.rs"), "mcp_edit", generation)]);
    }

    let result = actor.changes(0, None, 200).unwrap();
    let mutations = result["mutations"].as_array().unwrap();

    assert_eq!(mutations.len(), 2);
    assert!(mutations.iter().any(|item| item["path"] == "client-a.rs"));
    assert!(mutations.iter().any(|item| item["path"] == "client-b.rs"));
}

#[test]
fn changed_paths_are_filtered_and_capped() {
    let mut paths: HashSet<String> = (0..150)
        .map(|index| format!("src/file_{index}.rs"))
        .collect();
    paths.insert("core/target-audit/release/app.exe".to_owned());
    let summary = summarize_changed_paths(paths);
    assert_eq!(summary.paths.len(), MAX_OBSERVED_CHANGED_PATHS);
    assert_eq!(summary.count, 150);
    assert!(summary.truncated);
    assert_eq!(summary.groups.len(), 1);
    assert_eq!(summary.groups[0].path, "src");
    assert_eq!(summary.groups[0].count, 150);
}

#[test]
fn changed_path_groups_reserve_slot_for_other_bucket() {
    let paths: HashSet<String> = (0..(MAX_CHANGED_PATH_GROUPS + 5))
        .map(|index| format!("dir_{index}/file.rs"))
        .collect();

    let summary = summarize_changed_paths(paths);

    assert_eq!(summary.groups.len(), MAX_CHANGED_PATH_GROUPS);
    assert_eq!(summary.groups.last().unwrap().path, "(other)");
    assert_eq!(summary.groups.last().unwrap().count, 6);
}

#[test]
fn workspace_summary_caps_and_groups_large_change_sets() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("value.rs"), "fn value() {}\n").unwrap();
    let actor = test_actor(root.path());
    let external = (0..45)
        .map(|index| format!("backend/artifacts/result_{index}.json"))
        .chain((0..5).map(|index| format!("src/feature_{index}.rs")))
        .map(|path| mutation(&path, "external", 1))
        .collect::<Vec<_>>();
    actor.state.ledger().record_external(&external);
    set_dirty_files(
        &actor,
        (0..50)
            .map(|index| {
                if index < 45 {
                    format!("backend/artifacts/result_{index}.json")
                } else {
                    format!("src/feature_{}.rs", index - 45)
                }
            })
            .collect(),
    );

    let summary = actor.summary().unwrap();

    assert_eq!(
        summary["dirty_ownership"]["observed_external"]
            .as_array()
            .unwrap()
            .len(),
        MAX_OBSERVED_CHANGED_PATHS
    );
    assert_eq!(
        summary["dirty_ownership"]["counts"]["observed_external"],
        50
    );
    assert_eq!(
        summary["dirty_ownership"]["groups"]["observed_external"][0]["path"],
        "backend/artifacts"
    );
    assert_eq!(
        summary["dirty_ownership"]["groups"]["observed_external"][0]["count"],
        45
    );
    assert_eq!(
        summary["repository"]["dirty_files"]
            .as_array()
            .unwrap()
            .len(),
        MAX_OBSERVED_CHANGED_PATHS
    );
    assert_eq!(summary["repository"]["dirty_file_count"], 50);
    assert_eq!(summary["repository"]["dirty_files_truncated"], true);
}
