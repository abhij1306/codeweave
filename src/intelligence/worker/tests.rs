//! Worker lifecycle: synchronization, diagnostics transport, restart policy,
//! bounded queueing, and status reporting.
use super::super::test_support::{FakeServer, Fixture, Reply};
use super::*;
use std::sync::atomic::AtomicUsize;
use std::sync::mpsc;

fn definition() -> WorkerOperation {
    WorkerOperation::Definition {
        position: Position { line: 1, column: 3 },
    }
}

#[test]
fn a_changed_document_is_resent_in_full_with_the_next_version() {
    let fixture = Fixture::new(&[("main.rs", "fn café() {}\n")]);
    let server = FakeServer::new(|_, _| Reply::Result(json!([])));
    let worker = server.worker(
        fixture.root.path(),
        LspPreset::Rust,
        Duration::from_millis(200),
    );

    worker
        .execute(definition(), fixture.document("main.rs", "rust"))
        .unwrap();
    fixture.write("main.rs", "fn café() { println!(\"new\"); }\n");
    let second = worker
        .execute(definition(), fixture.document("main.rs", "rust"))
        .unwrap();

    assert_eq!(second.synchronized.version, 2);
    let changes = server.messages("textDocument/didChange");
    assert_eq!(changes.len(), 1);
    assert_eq!(
        changes[0]["params"]["contentChanges"],
        json!([{"text": "fn café() { println!(\"new\"); }\n"}])
    );
    let status = worker.status();
    assert_eq!(
        [
            &status["readiness"],
            &status["synchronized_document_count"],
            &status["latency_ms"]["request_count"],
            &status["capabilities"]["server_name"],
        ],
        [&json!("ready"), &json!(1), &json!(2), &json!("fixture")]
    );
}

#[test]
fn pull_diagnostics_are_requested_once_then_served_from_cache() {
    let fixture = Fixture::new(&[("sample.py", "value: int = 'wrong'\n")]);
    let server = FakeServer::new(|_, _| {
        Reply::Result(json!({"kind": "full", "items": [{"message": "wrong type"}]}))
    });
    let worker = server.worker(
        fixture.root.path(),
        LspPreset::Python,
        Duration::from_millis(200),
    );
    let document = fixture.document("sample.py", "python");

    let cold = worker
        .execute(WorkerOperation::Diagnostics, document.clone())
        .unwrap();
    let warm = worker
        .execute(WorkerOperation::Diagnostics, document)
        .unwrap();

    assert_eq!(cold.result, json!([{"message": "wrong type"}]));
    assert_eq!(warm.result, cold.result);
    assert_eq!(server.messages("textDocument/diagnostic").len(), 1);
}

#[test]
fn published_diagnostics_match_equivalently_encoded_uris() {
    let fixture = Fixture::new(&[("café.py", "value = 1\n")]);
    let server = FakeServer::new(|_, _| Reply::Silence)
        .with_capability("diagnosticProvider", Value::Null)
        .publishing(|uri| {
            json!({
                "uri": uri.replace("%C3%A9", "%c3%a9"),
                "diagnostics": [{"message": "published"}]
            })
        });
    let worker = server.worker(
        fixture.root.path(),
        LspPreset::Python,
        Duration::from_millis(200),
    );
    let document = fixture.document("café.py", "python");

    let cold = worker
        .execute(WorkerOperation::Diagnostics, document.clone())
        .unwrap();
    let warm = worker
        .execute(WorkerOperation::Diagnostics, document)
        .unwrap();

    assert_eq!(cold.result, json!([{"message": "published"}]));
    assert_eq!(warm.result, cold.result);
    assert_eq!(worker.status()["restart_count"], 0);
    assert!(server.messages("textDocument/diagnostic").is_empty());
}

#[test]
fn a_timed_out_request_restarts_the_server_and_reopens_the_document() {
    let fixture = Fixture::new(&[("main.rs", "fn target() {}\n")]);
    let first = AtomicBool::new(true);
    let server = FakeServer::new(move |_, _| {
        if first.swap(false, Ordering::SeqCst) {
            Reply::Silence
        } else {
            Reply::Result(json!([]))
        }
    });
    let worker = server.worker(
        fixture.root.path(),
        LspPreset::Rust,
        Duration::from_millis(200),
    );

    let response = worker.execute(definition(), fixture.document("main.rs", "rust"));

    assert_eq!(response.unwrap().result, json!([]));
    assert_eq!(server.spawn_count(), 2);
    assert_eq!(server.messages("textDocument/didOpen").len(), 2);
    assert_eq!(worker.status()["restart_count"], 1);
}

#[test]
fn a_server_error_reply_is_reported_without_restarting() {
    let fixture = Fixture::new(&[("main.rs", "fn target() {}\n")]);
    let server =
        FakeServer::new(|_, _| Reply::Error(json!({"code": -32603, "message": "internal"})));
    let worker = server.worker(
        fixture.root.path(),
        LspPreset::Rust,
        Duration::from_millis(200),
    );

    let error = worker
        .execute(definition(), fixture.document("main.rs", "rust"))
        .unwrap_err();

    let error = AppError::from(error);
    assert_eq!(error.0.code, "LSP_ERROR");
    assert_eq!(error.0.details.unwrap()["message"], "internal");
    assert_eq!(server.spawn_count(), 1);
    assert_eq!(worker.status()["readiness"], "ready");
}

#[test]
fn an_unsupported_operation_is_refused_before_any_request() {
    let fixture = Fixture::new(&[("main.rs", "fn target() {}\n")]);
    let server = FakeServer::new(|_, _| Reply::Result(json!([])))
        .with_capability("definitionProvider", json!(false));
    let worker = server.worker(
        fixture.root.path(),
        LspPreset::Rust,
        Duration::from_millis(200),
    );

    let error = worker
        .execute(definition(), fixture.document("main.rs", "rust"))
        .unwrap_err();

    assert_eq!(AppError::from(error).0.code, "LSP_UNSUPPORTED");
    assert!(server.messages("textDocument/definition").is_empty());
}

#[test]
fn requests_beyond_the_queue_capacity_are_refused_as_busy() {
    let fixture = Fixture::new(&[("main.rs", "fn target() {}\n")]);
    let (entered_tx, entered) = mpsc::channel();
    let (release, release_rx) = mpsc::channel::<()>();
    let entered_tx = parking_lot::Mutex::new(Some(entered_tx));
    let release_rx = parking_lot::Mutex::new(release_rx);
    let server = FakeServer::new(move |_, _| {
        if let Some(entered) = entered_tx.lock().take() {
            entered.send(()).unwrap();
            let _ = release_rx.lock().recv();
        }
        Reply::Result(json!([]))
    });
    let worker =
        Arc::new(server.worker(fixture.root.path(), LspPreset::Rust, Duration::from_secs(5)));
    let document = fixture.document("main.rs", "rust");
    let first = {
        let (worker, document) = (Arc::clone(&worker), document.clone());
        thread::spawn(move || worker.execute(definition(), document))
    };
    entered.recv().unwrap();

    let busy = Arc::new(AtomicUsize::new(0));
    let callers = (0..QUEUE_CAPACITY + 4)
        .map(|_| {
            let (worker, document, busy) =
                (Arc::clone(&worker), document.clone(), Arc::clone(&busy));
            thread::spawn(move || {
                let result = worker.execute(definition(), document);
                if matches!(result, Err(WorkerError::Busy { .. })) {
                    busy.fetch_add(1, Ordering::SeqCst);
                }
                result.is_ok()
            })
        })
        .collect::<Vec<_>>();
    let waited = Instant::now();
    while busy.load(Ordering::SeqCst) < 4 && waited.elapsed() < Duration::from_secs(10) {
        thread::sleep(Duration::from_millis(10));
    }
    release.send(()).unwrap();
    let succeeded = callers
        .into_iter()
        .map(|caller| caller.join().unwrap())
        .filter(|ok| *ok)
        .count();

    assert!(first.join().unwrap().is_ok());
    assert_eq!(
        [busy.load(Ordering::SeqCst), succeeded],
        [4, QUEUE_CAPACITY]
    );
}

#[test]
fn a_disabled_worker_reports_disabled_and_refuses_requests() {
    let fixture = Fixture::new(&[("main.rs", "fn target() {}\n")]);
    let worker = LspWorker::spawn(
        fixture.root.path(),
        LspPreset::Rust,
        &LanguageServerSettings::default(),
    )
    .unwrap();

    let error = worker
        .execute(definition(), fixture.document("main.rs", "rust"))
        .unwrap_err();

    assert_eq!(AppError::from(error).0.code, "SEMANTIC_BACKEND_UNAVAILABLE");
    assert_eq!(worker.status()["readiness"], "disabled");
    assert_eq!(worker.status()["command"], "rust-analyzer");
}
