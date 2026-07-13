use super::WorkspaceActor;
use crate::bash::StartRequest;
use serde_json::{json, Value};
use std::time::Instant;

pub(super) struct ValidationOutcome {
    pub(super) validation: Vec<Value>,
    pub(super) failed: bool,
    pub(super) pending_run_id: Option<String>,
    pub(super) deferred_run_id: Option<String>,
}

impl WorkspaceActor {
    pub(super) async fn run_edit_validation(
        &self,
        session_id: &str,
        commands: &[String],
    ) -> ValidationOutcome {
        let budget_ms = self.policy.bash.foreground_budget_ms;
        let validation_started = Instant::now();
        let mut validation = Vec::new();
        let mut failed = false;
        let mut pending_run_id = None;
        let mut deferred_run_id = None;
        let mut remaining = commands.iter().peekable();

        while let Some(command) = remaining.next() {
            let over_budget =
                budget_ms != 0 && validation_started.elapsed().as_millis() as u64 >= budget_ms;
            if over_budget {
                let mut deferred = vec![command.clone()];
                deferred.extend(remaining.map(String::clone));
                match self
                    .spawn_pending_validation(session_id, &deferred, &mut validation)
                    .await
                {
                    Some(run_id) => pending_run_id = Some(run_id),
                    None => failed = true,
                }
                break;
            }

            match self
                .bash
                .start_for_session(
                    &self.root,
                    session_id,
                    StartRequest {
                        command: command.clone(),
                        cwd: None,
                        background: Some(false),
                        timeout_ms: None,
                    },
                )
                .await
            {
                Ok(result) => match result.get("status").and_then(Value::as_str) {
                    Some("succeeded") => {
                        validation.push(json!({"command": command, "result": result}));
                    }
                    Some("running") => {
                        let run_id = result
                            .get("run_id")
                            .and_then(Value::as_str)
                            .map(str::to_owned);
                        validation.push(json!({"command": command, "result": result}));
                        if let Some(run_id) = run_id {
                            pending_run_id = Some(run_id);
                            let deferred = remaining.map(String::clone).collect::<Vec<_>>();
                            if !deferred.is_empty() {
                                match self
                                    .spawn_pending_validation(
                                        session_id,
                                        &deferred,
                                        &mut validation,
                                    )
                                    .await
                                {
                                    Some(run_id) => deferred_run_id = Some(run_id),
                                    None => failed = true,
                                }
                            }
                        } else {
                            failed = true;
                        }
                        break;
                    }
                    _ => {
                        failed = true;
                        validation.push(json!({"command": command, "result": result}));
                    }
                },
                Err(error) => {
                    failed = true;
                    validation.push(json!({
                        "command": command,
                        "error": error.0,
                    }));
                }
            }
            if failed {
                break;
            }
        }

        ValidationOutcome {
            validation,
            failed,
            pending_run_id,
            deferred_run_id,
        }
    }

    /// Queue the remaining validation commands as one sequential Bash run.
    /// The queued run is immediately pollable and starts after any validator
    /// that already consumed the single execution slot.
    async fn spawn_pending_validation(
        &self,
        session_id: &str,
        commands: &[String],
        validation: &mut Vec<Value>,
    ) -> Option<String> {
        let joined = commands
            .iter()
            .map(|command| format!("({command})"))
            .collect::<Vec<_>>()
            .join(" && ");
        match self
            .bash
            .queue_for_session(
                &self.root,
                session_id,
                StartRequest {
                    command: joined.clone(),
                    cwd: None,
                    background: Some(true),
                    timeout_ms: None,
                },
            )
            .await
        {
            Ok(result) => {
                let run_id = result
                    .get("run_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                if let Some(run_id) = run_id.as_deref() {
                    for command in commands {
                        validation.push(json!({
                            "command": command,
                            "deferred": true,
                            "result": {
                                "status": "pending",
                                "reason": "blocked_by_pending_validation",
                                "run_id": run_id
                            }
                        }));
                    }
                } else {
                    validation.push(json!({
                        "command": joined,
                        "deferred": true,
                        "result": result
                    }));
                }
                run_id
            }
            Err(error) => {
                validation.push(json!({"command": joined, "error": error.0}));
                None
            }
        }
    }
}
