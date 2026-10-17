use super::*;
use crate::workspace::Stamp;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

fn init_repo_with(root: &Path, path: &str, content: &str) {
    run_git(root, &["init", "-q"]);
    run_git(root, &["config", "core.autocrlf", "false"]);
    run_git(root, &["config", "user.email", "codeweave@example.test"]);
    run_git(root, &["config", "user.name", "CodeWeave Test"]);
    fs::write(root.join(path), content).unwrap();
    run_git(root, &["add", path]);
    run_git(root, &["commit", "-q", "-m", "baseline"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_restore_waits_while_an_edit_holds_the_mutation_permit() {
    let root = tempdir().unwrap();
    init_repo_with(root.path(), "value.rs", "fn value() -> i32 { 1 }\n");
    let path = root.path().join("value.rs");
    fs::write(&path, "fn value() -> i32 { 2 }\n").unwrap();
    let actor = test_actor(root.path());

    let permit = actor.state.exclusive().await;
    let restore = tokio::spawn({
        let actor = Arc::clone(&actor);
        async move {
            actor
                .git(&json!({"action": "restore", "paths": ["value.rs"], "confirm": true}))
                .await
        }
    });
    // Non-mutating Git actions do not wait for the permit.
    let status = actor.git(&json!({"action": "status"})).await.unwrap();
    assert_eq!(status["status"]["dirty_files"], json!(["value.rs"]));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!restore.is_finished());
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "fn value() -> i32 { 2 }\n"
    );

    drop(permit);
    restore.await.unwrap().unwrap();
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "fn value() -> i32 { 1 }\n"
    );
    assert_eq!(
        actor.state.read().repo_status().dirty_files,
        Vec::<String>::new()
    );
}

fn status_on(branch: &str) -> RepoStatus {
    RepoStatus {
        is_git: true,
        head: "0123456789abcdef".to_owned(),
        branch: branch.to_owned(),
        dirty_files: vec![format!("{branch}.rs")],
        ..RepoStatus::default()
    }
}

/// Drive one site that refreshes the repository status. `round` keeps file names
/// unique so every reconcile and edit sees a real change.
async fn refresh_status_through(actor: &Arc<Workspace>, root: &Path, site: &str, round: usize) {
    match site {
        "reconcile" => {
            let path = root.join(format!("watched_{round}.rs"));
            fs::write(&path, "fn watched() {}\n").unwrap();
            actor.state.ledger().pending_handle().observe([path]);
            actor.summary().unwrap();
        }
        "git status" => {
            let _ = actor.git(&json!({"action": "status"})).await;
        }
        "git preflight" => {
            let _ = actor.git(&json!({"action": "preflight"})).await;
        }
        "edit commit" => {
            actor
                .code_edit(&json!({"changes": [{
                    "kind": "create",
                    "path": format!("edited_{round}.rs"),
                    "content": "fn edited() {}\n"
                }]}))
                .await
                .unwrap();
        }
        "forced refresh" => {
            actor.refresh(true).unwrap();
        }
        other => panic!("unknown site {other}"),
    }
}

#[tokio::test]
async fn failed_git_status_keeps_the_previous_status_and_marks_it_stale_at_every_site() {
    let root = tempdir().unwrap();
    let repository = ScriptedRepository::new(status_on("initial"));
    let actor = test_actor_with_repository(root.path(), Arc::clone(&repository));
    assert_eq!(actor.state.read().repo_status(), &status_on("initial"));

    let sites = [
        "reconcile",
        "git status",
        "git preflight",
        "edit commit",
        "forced refresh",
    ];
    for (index, site) in sites.into_iter().enumerate() {
        let previous = actor.state.read().repo_status().clone();
        repository.fail();
        refresh_status_through(&actor, root.path(), site, index * 2).await;
        {
            let view = actor.state.read();
            assert!(
                view.repo_status_stale(),
                "{site} must mark the status stale"
            );
            assert_eq!(view.repo_status(), &previous, "{site} must keep the status");
        }

        let branch = format!("after-{index}");
        repository.succeed_with(status_on(&branch));
        refresh_status_through(&actor, root.path(), site, index * 2 + 1).await;
        let view = actor.state.read();
        assert!(
            !view.repo_status_stale(),
            "{site} must clear the stale flag"
        );
        assert_eq!(view.repo_status(), &status_on(&branch), "{site}");
    }
}

#[tokio::test]
async fn failed_git_status_is_reported_by_status_and_by_stage_responses() {
    let root = tempdir().unwrap();
    let repository = ScriptedRepository::new(status_on("initial"));
    let actor = test_actor_with_repository(root.path(), Arc::clone(&repository));
    repository.fail();

    let error = actor.git(&json!({"action": "status"})).await.unwrap_err();
    assert_eq!(error.0.code, "GIT_FAILED");
    let staged = actor
        .git(&json!({"action": "stage", "paths": ["initial.rs"]}))
        .await
        .unwrap();
    assert_eq!(staged["repo_status_stale"], true);

    repository.succeed_with(status_on("initial"));
    let staged = actor
        .git(&json!({"action": "stage", "paths": ["initial.rs"]}))
        .await
        .unwrap();
    assert!(staged.get("repo_status_stale").is_none());
}

#[test]
fn readers_never_observe_a_generation_paired_with_another_snapshot() {
    let root = tempdir().unwrap();
    fs::write(root.path().join("value.rs"), "fn value() -> i32 { 0 }\n").unwrap();
    let actor = test_actor(root.path());
    let mut published = HashSet::from([actor.state.read().stamp()]);
    let stop = AtomicBool::new(false);

    let observed = std::thread::scope(|scope| {
        let reader = scope.spawn(|| {
            let mut observed = HashSet::<Stamp>::new();
            while !stop.load(Ordering::Acquire) {
                observed.insert(actor.state.read().stamp());
            }
            observed
        });
        for round in 1..=40 {
            let mut writer = actor.state.writer();
            actor
                .commit_plan(
                    &mut writer,
                    &[PlannedFile {
                        path: "value.rs".to_owned(),
                        before: None,
                        after: Some(format!("fn value() -> i32 {{ {round} }}\n")),
                    }],
                    "stamp-test",
                )
                .unwrap();
            published.insert(writer.read().stamp());
        }
        stop.store(true, Ordering::Release);
        reader.join().unwrap()
    });

    assert_eq!(published.len(), 41);
    let generations: HashSet<u64> = published.iter().map(|stamp| stamp.generation).collect();
    assert_eq!(generations, (1..=41).collect::<HashSet<u64>>());
    let torn: Vec<&Stamp> = observed.difference(&published).collect();
    assert!(torn.is_empty(), "torn stamps observed: {torn:?}");
}

#[tokio::test]
async fn edit_response_reports_the_stamp_the_state_published() {
    let root = tempdir().unwrap();
    let actor = test_actor(root.path());
    let before = actor.state.read().stamp();

    let response = actor
        .code_edit(&json!({"changes": [{
            "kind": "create",
            "path": "created.rs",
            "content": "fn created() {}\n"
        }]}))
        .await
        .unwrap();

    let after = actor.state.read().stamp();
    assert_eq!(after.generation, before.generation + 1);
    assert_ne!(after.snapshot_id, before.snapshot_id);
    assert_eq!(response["generation"], after.generation);
    assert_eq!(response["snapshot_id"], after.snapshot_id.as_str());
}
