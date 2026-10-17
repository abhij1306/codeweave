use super::launch::fake::FakeLauncher;
use super::*;
use crate::model::BashConfig;
use crate::process_runtime::fake::FakeReaction;
use crate::test_bash_executable;
use std::fs;
use tempfile::tempdir;

fn policy() -> PolicyConfig {
    PolicyConfig {
        max_file_bytes: 1_000_000,
        max_context_chars: 50_000,
        max_search_results: 100,
        bash: BashConfig {
            executable: test_bash_executable(),
            default_timeout_ms: 5_000,
            foreground_budget_ms: 4_000,
            max_timeout_ms: 10_000,
            max_output_chars: 30_000,
        },
    }
}

fn policy_with_executable(executable: String) -> PolicyConfig {
    let mut policy = policy();
    policy.bash.executable = executable;
    policy
}

#[tokio::test]
async fn explicit_invalid_bash_path_reports_unavailable_before_starting() {
    let root = tempdir().unwrap();
    let elsewhere = tempdir().unwrap();
    let missing = elsewhere.path().join("not-a-bash.exe");
    let supervisor = BashSupervisor::new(policy_with_executable(
        missing.to_string_lossy().into_owned(),
    ))
    .unwrap();
    let readiness = supervisor.readiness();
    assert!(readiness.configured);
    assert_eq!(readiness.readiness, "unavailable");
    assert_eq!(readiness.resolved_executable, None);

    let error = supervisor
        .start(
            root.path(),
            StartRequest {
                command: "printf test".to_owned(),
                cwd: None,
                background: None,
                timeout_ms: None,
            },
        )
        .await
        .unwrap_err();

    assert_eq!(error.0.code, "BASH_UNAVAILABLE");
    assert!(supervisor.runs.records().is_empty());
}

#[test]
fn non_default_relative_bash_name_fails_closed_after_probe_failure() {
    let supervisor =
        BashSupervisor::new(policy_with_executable("missing-codeweave-bash".to_owned())).unwrap();

    let readiness = supervisor.readiness();

    assert_eq!(readiness.readiness, "unavailable");
    assert_eq!(readiness.resolved_executable, None);
}

#[tokio::test]
async fn cwd_must_exist_inside_workspace() {
    let root = tempdir().unwrap();
    let supervisor = BashSupervisor::new(policy()).unwrap();

    let missing = supervisor
        .start(
            root.path(),
            StartRequest {
                command: "printf test".to_owned(),
                cwd: Some("missing".to_owned()),
                background: None,
                timeout_ms: None,
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(
        missing.0.code.as_str(),
        "PATH_NOT_FOUND" | "INVALID_CWD"
    ));

    let escaped = supervisor
        .start(
            root.path(),
            StartRequest {
                command: "printf test".to_owned(),
                cwd: Some("../outside".to_owned()),
                background: None,
                timeout_ms: None,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(escaped.0.code, "OUTSIDE_ROOT");
}

#[tokio::test]
async fn timeout_above_policy_maximum_is_rejected() {
    let root = tempdir().unwrap();
    let supervisor = BashSupervisor::new(policy()).unwrap();
    let error = supervisor
        .start(
            root.path(),
            StartRequest {
                command: "printf test".to_owned(),
                cwd: None,
                background: None,
                timeout_ms: Some(10_001),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error.0.code, "INVALID_TIMEOUT");
}

#[tokio::test]
async fn foreground_commands_capture_stdout_stderr_and_failures() {
    let root = tempdir().unwrap();
    let supervisor = BashSupervisor::new(policy()).unwrap();
    let succeeded = supervisor
        .start(
            root.path(),
            StartRequest {
                command: "printf stdout; printf stderr >&2".to_owned(),
                cwd: None,
                background: None,
                timeout_ms: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(succeeded["status"], "succeeded");
    assert_eq!(succeeded["exit_code"], 0);
    assert!(succeeded["output"].as_str().unwrap().contains("stdout"));
    assert!(succeeded["output"].as_str().unwrap().contains("stderr"));

    let failed = supervisor
        .start(
            root.path(),
            StartRequest {
                command: "printf failed >&2; exit 7".to_owned(),
                cwd: None,
                background: None,
                timeout_ms: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(failed["status"], "failed");
    assert_eq!(failed["exit_code"], 7);
}

#[tokio::test]
async fn foreground_process_preserves_quotes_and_non_utf8_output() {
    let root = tempdir().unwrap();
    let supervisor = BashSupervisor::new(policy()).unwrap();

    let quoted = supervisor
        .start(
            root.path(),
            StartRequest {
                command: "printf \"first's\"".to_owned(),
                cwd: None,
                background: None,
                timeout_ms: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(quoted["status"], "succeeded");
    assert!(quoted["output"].as_str().unwrap().contains("first's"));

    let binary = supervisor
        .start(
            root.path(),
            StartRequest {
                command: r"printf '\377'".to_owned(),
                cwd: None,
                background: None,
                timeout_ms: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(binary["status"], "succeeded");
    assert!(!binary["output"].as_str().unwrap().is_empty());
}

#[cfg(windows)]
#[tokio::test]
async fn foreground_wsl_bash_maps_windows_working_directory() {
    let wsl_bash = PathBuf::from(std::env::var_os("WINDIR").unwrap_or_default())
        .join("System32")
        .join("bash.exe");
    if !wsl_bash.is_file()
        || !std::process::Command::new(&wsl_bash)
            .args(["-c", "true"])
            .status()
            .is_ok_and(|status| status.success())
    {
        return;
    }

    let root = std::env::current_dir().unwrap().canonicalize().unwrap();
    let mut wsl_policy = policy();
    wsl_policy.bash.executable = wsl_bash.to_string_lossy().into_owned();
    let supervisor = BashSupervisor::new(wsl_policy).unwrap();
    let result = supervisor
        .start(
            &root,
            StartRequest {
                command: "pwd".to_owned(),
                cwd: None,
                background: None,
                timeout_ms: None,
            },
        )
        .await
        .unwrap();

    assert_eq!(result["status"], "succeeded");
    assert!(result["output"]
        .as_str()
        .unwrap()
        .trim()
        .ends_with("/Projects/codeweave"));
}

#[tokio::test]
async fn background_commands_can_be_polled_paged_and_cancelled() {
    let root = tempdir().unwrap();
    let supervisor = BashSupervisor::new(policy()).unwrap();
    let started = supervisor
        .start(
            root.path(),
            StartRequest {
                command: "echo started; sleep 30".to_owned(),
                cwd: None,
                background: Some(true),
                timeout_ms: None,
            },
        )
        .await
        .unwrap();
    let run_id = started["run_id"].as_str().unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(matches!(
        supervisor.status(run_id).unwrap()["status"]
            .as_str()
            .unwrap(),
        "queued" | "running"
    ));
    let mut output = supervisor
        .output_stream(run_id, None, Some("combined"))
        .unwrap();
    for _ in 0..50 {
        if output["output"].as_str().unwrap().contains("started") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        output = supervisor
            .output_stream(run_id, None, Some("combined"))
            .unwrap();
    }
    assert!(output["output"].as_str().unwrap().contains("started"));
    assert_eq!(supervisor.cancel(run_id).unwrap()["status"], "cancelling");

    for _ in 0..50 {
        if supervisor.status(run_id).unwrap()["ended_at"].is_string() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(supervisor.status(run_id).unwrap()["status"], "cancelled");
}

#[tokio::test]
async fn queued_command_waits_for_the_active_run() {
    let root = tempdir().unwrap();
    let supervisor = BashSupervisor::new(policy()).unwrap();
    let active = supervisor
        .start(
            root.path(),
            StartRequest {
                command: "sleep 1".to_owned(),
                cwd: None,
                background: Some(true),
                timeout_ms: Some(5_000),
            },
        )
        .await
        .unwrap();
    let active_run_id = active["run_id"].as_str().unwrap();
    let queued = supervisor
        .queue(
            root.path(),
            StartRequest {
                command: "printf queued > queued.txt".to_owned(),
                cwd: None,
                background: Some(true),
                timeout_ms: Some(5_000),
            },
        )
        .await
        .unwrap();
    let queued_run_id = queued["run_id"].as_str().unwrap();

    assert_eq!(queued["status"], "queued");
    assert_eq!(queued["queued"], true);
    assert_eq!(
        supervisor.active_run_view().unwrap()["run_id"],
        active_run_id
    );

    let mut terminal = supervisor.status(queued_run_id).unwrap();
    for _ in 0..200 {
        if terminal["ended_at"].is_string() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        terminal = supervisor.status(queued_run_id).unwrap();
    }
    assert_eq!(terminal["status"], "succeeded");
    assert_eq!(
        fs::read_to_string(root.path().join("queued.txt")).unwrap(),
        "queued"
    );
}
#[tokio::test]
async fn client_abort_keeps_the_detached_run_alive() {
    // Simulates ChatGPT aborting the HTTP request: the request future is
    // dropped mid-command. The detached execution task must keep running,
    // the permit must free once it finishes, and the next warm run must be
    // clean (not interleaved with the abandoned command's output).
    let root = tempdir().unwrap();
    let mut abort_policy = policy();
    abort_policy.bash.foreground_budget_ms = 20_000;
    abort_policy.bash.default_timeout_ms = 10_000;
    let supervisor = Arc::new(BashSupervisor::new(abort_policy).unwrap());

    // Start a slow command on a task we then abort, mirroring a dropped
    // request future.
    let bg = supervisor.clone();
    let root_path = root.path().to_path_buf();
    let request_task = tokio::spawn(async move {
        bg.start(
            &root_path,
            StartRequest {
                command: "echo abandoned; sleep 5".to_owned(),
                cwd: None,
                background: None,
                timeout_ms: None,
            },
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    request_task.abort();
    let _ = request_task.await;

    // The detached command still owns the only execution slot.
    let retry = supervisor
        .start(
            root.path(),
            StartRequest {
                command: "echo abandoned; sleep 5".to_owned(),
                cwd: None,
                background: None,
                timeout_ms: None,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(retry.0.code, "RUN_BUSY");

    // Wait for the abandoned command to finish and free the permit.
    for _ in 0..100 {
        if supervisor.running_count() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(supervisor.running_count(), 0);

    // The next process is clean: only its own output, no leakage.
    let fresh = supervisor
        .start(
            root.path(),
            StartRequest {
                command: "printf fresh-output".to_owned(),
                cwd: None,
                background: None,
                timeout_ms: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(fresh["status"], "succeeded");
    let output = fresh["output"].as_str().unwrap();
    assert!(output.contains("fresh-output"));
    assert!(!output.contains("abandoned"));
}

#[tokio::test]
async fn foreground_budget_auto_promotes_long_command() {
    let root = tempdir().unwrap();
    let mut promote_policy = policy();
    promote_policy.bash.foreground_budget_ms = 200;
    promote_policy.bash.default_timeout_ms = 10_000;
    let supervisor = BashSupervisor::new(promote_policy).unwrap();
    let result = supervisor
        .start(
            root.path(),
            StartRequest {
                command: "echo warming; sleep 30".to_owned(),
                cwd: None,
                background: None,
                timeout_ms: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(result["status"], "running");
    assert_eq!(result["detached"], true);
    assert_eq!(result["reason"], "foreground_budget_exceeded");
    let run_id = result["run_id"].as_str().unwrap().to_owned();
    assert!(supervisor.cancel(&run_id).is_ok());
}

#[tokio::test]
async fn identical_command_is_not_deduplicated() {
    let root = tempdir().unwrap();
    let mut dedupe_policy = policy();
    dedupe_policy.bash.foreground_budget_ms = 200;
    dedupe_policy.bash.default_timeout_ms = 10_000;
    let supervisor = BashSupervisor::new(dedupe_policy).unwrap();
    let request = || StartRequest {
        command: "sleep 30".to_owned(),
        cwd: None,
        background: None,
        timeout_ms: None,
    };
    let first = supervisor.start(root.path(), request()).await.unwrap();
    assert_eq!(first["status"], "running");
    let run_id = first["run_id"].as_str().unwrap().to_owned();

    let retry = supervisor.start(root.path(), request()).await.unwrap_err();
    assert_eq!(retry.0.code, "RUN_BUSY");
    assert_eq!(
        retry.0.details.as_ref().unwrap()["active_run"]["run_id"],
        run_id
    );

    // A genuinely different command still gets a busy error carrying the
    // active run so the model can poll or cancel.
    let busy = supervisor
        .start(
            root.path(),
            StartRequest {
                command: "echo other".to_owned(),
                cwd: None,
                background: None,
                timeout_ms: None,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(busy.0.code, "RUN_BUSY");
    let details = busy.0.details.unwrap();
    assert_eq!(details["active_run"]["run_id"], run_id);
    assert!(supervisor.cancel(&run_id).is_ok());
}

#[tokio::test]
async fn timeout_retains_partial_output() {
    let root = tempdir().unwrap();
    let mut timeout_policy = policy();
    timeout_policy.bash.default_timeout_ms = 100;
    let supervisor = BashSupervisor::new(timeout_policy).unwrap();
    let result = supervisor
        .start(
            root.path(),
            StartRequest {
                command: "printf partial; sleep 30".to_owned(),
                cwd: None,
                background: None,
                timeout_ms: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(result["status"], "timed_out");
    assert!(result["output"].as_str().unwrap().contains("partial"));
}

// Scripted-process tests: deterministic cancel, timeout and abandonment
// behaviour without a real shell.

fn fake_supervisor(launcher: Arc<FakeLauncher>, policy: PolicyConfig) -> BashSupervisor {
    let readiness = BashReadiness {
        configured: true,
        configured_executable: "bash".to_owned(),
        resolved_executable: Some("bash".to_owned()),
        shell_type: "bash".to_owned(),
        readiness: "ready".to_owned(),
        failure_reason: None,
    };
    BashSupervisor::with_parts(policy, readiness, Some(launcher))
}

fn request(background: Option<bool>, timeout_ms: Option<u64>) -> StartRequest {
    StartRequest {
        command: "scripted".to_owned(),
        cwd: None,
        background,
        timeout_ms,
    }
}

async fn wait_until(mut ready: impl FnMut() -> bool) {
    for _ in 0..250 {
        if ready() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("condition not reached within five seconds");
}

#[tokio::test]
async fn cancel_stops_the_process_group_and_keeps_partial_output() {
    let root = tempdir().unwrap();
    let launcher = Arc::new(FakeLauncher::new("partial", FakeReaction::StopsOnTerminate));
    let supervisor = fake_supervisor(launcher.clone(), policy());
    let started = supervisor
        .start(root.path(), request(Some(true), None))
        .await
        .unwrap();
    let run_id = started["run_id"].as_str().unwrap();
    wait_until(|| supervisor.status(run_id).unwrap()["status"] == "running").await;
    assert_eq!(supervisor.status(run_id).unwrap()["pid"], 4242);

    assert_eq!(
        supervisor.cancel(run_id).unwrap(),
        json!({"run_id": run_id, "status": "cancelling"})
    );
    wait_until(|| launcher.events().contains(&"dropped")).await;

    let status = supervisor.status(run_id).unwrap();
    assert_eq!(status["status"], "cancelled");
    assert_eq!(
        status["output"],
        "partial\n\nBash run was cancelled; partial output was retained."
    );
    assert_eq!(status["pid"], Value::Null);
    assert_eq!(launcher.events(), ["launch", "terminate", "dropped"]);
}

#[tokio::test]
async fn timeout_stops_the_process_and_reports_timed_out() {
    let root = tempdir().unwrap();
    let launcher = Arc::new(FakeLauncher::new("partial", FakeReaction::StopsOnTerminate));
    let supervisor = fake_supervisor(launcher.clone(), policy());

    let result = supervisor
        .start(root.path(), request(None, Some(50)))
        .await
        .unwrap();

    assert_eq!(result["status"], "timed_out");
    assert_eq!(result["exit_code"], Value::Null);
    assert_eq!(
        result["output"],
        "partial\n\nBash run exceeded timeout of 50 ms; partial output was retained."
    );
    assert_eq!(launcher.events(), ["launch", "terminate", "dropped"]);
}

#[tokio::test]
async fn a_process_that_ignores_terminate_is_killed_after_the_grace_period() {
    let root = tempdir().unwrap();
    let launcher = Arc::new(FakeLauncher::new("", FakeReaction::IgnoresTerminate));
    let supervisor = fake_supervisor(launcher.clone(), policy());

    let result = supervisor
        .start(root.path(), request(None, Some(50)))
        .await
        .unwrap();

    assert_eq!(result["status"], "timed_out");
    assert_eq!(
        launcher.events(),
        ["launch", "terminate", "kill", "dropped"]
    );
}

#[test]
fn an_abandoned_run_is_cancelled_and_its_process_group_killed() {
    let root = tempdir().unwrap();
    let launcher = Arc::new(FakeLauncher::new("", FakeReaction::IgnoresTerminate));
    let supervisor = fake_supervisor(launcher.clone(), policy());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let run_id = runtime.block_on(async {
        let started = supervisor
            .start(root.path(), request(Some(true), None))
            .await
            .unwrap();
        let run_id = started["run_id"].as_str().unwrap().to_owned();
        wait_until(|| supervisor.status(&run_id).unwrap()["status"] == "running").await;
        run_id
    });

    // Shutting the runtime down drops the execution task mid-run.
    drop(runtime);

    let status = supervisor.status(&run_id).unwrap();
    assert_eq!(status["status"], "cancelled");
    assert_eq!(
        status["output"],
        "Bash run was abandoned; the process tree was terminated."
    );
    assert_eq!(launcher.events(), ["launch", "dropped"]);
    assert_eq!(supervisor.running_count(), 0);
}

#[tokio::test]
async fn a_queued_run_cancelled_before_it_starts_is_never_launched() {
    let root = tempdir().unwrap();
    let launcher = Arc::new(FakeLauncher::new("", FakeReaction::StopsOnTerminate));
    let supervisor = fake_supervisor(launcher.clone(), policy());
    let active = supervisor
        .start(root.path(), request(Some(true), None))
        .await
        .unwrap();
    let active_id = active["run_id"].as_str().unwrap();
    let queued = supervisor
        .queue(root.path(), request(None, None))
        .await
        .unwrap();
    let queued_id = queued["run_id"].as_str().unwrap();
    assert_eq!(queued["status"], "queued");

    assert_eq!(
        supervisor.cancel(queued_id).unwrap()["status"],
        "cancelling"
    );
    supervisor.cancel(active_id).unwrap();
    wait_until(|| supervisor.status(queued_id).unwrap()["ended_at"].is_string()).await;

    let status = supervisor.status(queued_id).unwrap();
    assert_eq!(status["status"], "cancelled");
    assert_eq!(
        status["output"],
        "Bash run was cancelled before the process started."
    );
    let launches = launcher
        .events()
        .into_iter()
        .filter(|event| *event == "launch")
        .count();
    assert_eq!(launches, 1);
}

#[tokio::test]
async fn cancelling_a_finished_run_leaves_it_unchanged() {
    let root = tempdir().unwrap();
    let launcher = Arc::new(FakeLauncher::new("out", FakeReaction::ExitsWith(3)));
    let supervisor = fake_supervisor(launcher, policy());
    let finished = supervisor
        .start(root.path(), request(None, None))
        .await
        .unwrap();
    assert_eq!(finished["status"], "failed");

    let cancelled = supervisor
        .cancel(finished["run_id"].as_str().unwrap())
        .unwrap();

    assert_eq!(cancelled["status"], "failed");
    assert_eq!(cancelled["exit_code"], 3);
    assert_eq!(cancelled["output"], "out");
}

#[tokio::test]
async fn only_the_newest_completed_runs_are_retained() {
    let root = tempdir().unwrap();
    let launcher = Arc::new(FakeLauncher::new("", FakeReaction::ExitsWith(0)));
    let supervisor = fake_supervisor(launcher, policy());
    let mut run_ids = Vec::new();
    for _ in 0..=MAX_RETAINED_RUNS {
        let finished = supervisor
            .start(root.path(), request(None, None))
            .await
            .unwrap();
        run_ids.push(finished["run_id"].as_str().unwrap().to_owned());
    }

    let evicted = supervisor.status(&run_ids[0]).unwrap_err();
    assert_eq!(evicted.0.code, "RUN_NOT_FOUND");
    assert_eq!(
        supervisor.status(&run_ids[1]).unwrap()["status"],
        "succeeded"
    );
}

#[tokio::test]
async fn output_streams_are_kept_apart_and_report_dropped_prefixes() {
    let root = tempdir().unwrap();
    let mut launcher = FakeLauncher::new("stdout-only", FakeReaction::ExitsWith(0));
    launcher.stderr = "stderr-only";
    let mut small = policy();
    small.bash.max_output_chars = 8;
    let supervisor = fake_supervisor(Arc::new(launcher), small);
    let finished = supervisor
        .start(root.path(), request(None, None))
        .await
        .unwrap();
    let run_id = finished["run_id"].as_str().unwrap();

    let stdout = supervisor
        .output_stream(run_id, None, Some("stdout"))
        .unwrap();
    assert_eq!(stdout["output"], "out-only");
    assert_eq!(stdout["total_chars"], 11);
    assert_eq!(stdout["dropped_prefix_chars"], 3);
    assert_eq!(stdout["retained_start_offset"], 3);
    assert_eq!(stdout["output_truncated"], true);
    assert_eq!(stdout["continuation"], Value::Null);
    let stderr = supervisor
        .output_stream(run_id, None, Some("stderr"))
        .unwrap();
    assert_eq!(stderr["output"], "err-only");
    let invalid = supervisor
        .output_stream(run_id, None, Some("both"))
        .unwrap_err();
    assert_eq!(invalid.0.code, "INVALID_ARGUMENT");
}
