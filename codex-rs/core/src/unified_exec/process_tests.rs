use super::process::UnifiedExecProcess;
use crate::config::ToolOutputSpillConfig;
use crate::unified_exec::NoopSpawnLifecycle;
use crate::unified_exec::OutputArtifactStore;
use crate::unified_exec::UNIFIED_EXEC_OUTPUT_MAX_BYTES;
use crate::unified_exec::UnifiedExecError;
use codex_exec_server::ExecProcess;
use codex_exec_server::ExecProcessEventReceiver;
use codex_exec_server::ExecProcessFuture;
use codex_exec_server::ExecServerError;
use codex_exec_server::ProcessId;
use codex_exec_server::ProcessSignal;
use codex_exec_server::ReadResponse;
use codex_exec_server::StartedExecProcess;
use codex_exec_server::WriteResponse;
use codex_exec_server::WriteStatus;
use pretty_assertions::assert_eq;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::watch;

#[cfg(unix)]
#[tokio::test]
async fn local_pipe_artifact_captures_bytes_before_head_tail_omission() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let artifact_root =
        codex_utils_absolute_path::AbsolutePathBuf::try_from(temp_dir.path().join("artifacts"))
            .expect("absolute artifact root");
    let store = OutputArtifactStore::new(
        ToolOutputSpillConfig {
            enabled: true,
            token_threshold: 1,
            preview_token_limit: 100,
            max_artifact_bytes: 2 * 1024 * 1024,
            max_store_bytes: 4 * 1024 * 1024,
            retention_days: 7,
            output_dir: artifact_root,
        },
        "thread-e2e",
    )
    .expect("enabled artifact store");
    let byte_count = UNIFIED_EXEC_OUTPUT_MAX_BYTES + 128 * 1024;
    let args = vec!["-c".to_string(), format!("head -c {byte_count} /dev/zero")];
    let spawned = codex_utils_pty::pipe::spawn_process_no_stdin(
        "/bin/sh",
        &args,
        temp_dir.path(),
        &HashMap::new(),
        &None,
    )
    .await
    .expect("spawn local pipe process");
    let process = UnifiedExecProcess::from_spawned(
        spawned,
        codex_sandboxing::SandboxType::None,
        Box::new(NoopSpawnLifecycle),
        Some(store.spool()),
    )
    .await
    .expect("start unified exec process");
    let handles = process.output_handles();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !handles
            .output_closed
            .load(std::sync::atomic::Ordering::Acquire)
        {
            handles.output_closed_notify.notified().await;
        }
    })
    .await
    .expect("output should drain");

    let descriptor = process
        .output_artifact_descriptor()
        .await
        .expect("artifact descriptor");
    assert_eq!(descriptor.observed_bytes, byte_count as u64);
    assert_eq!(descriptor.stored_bytes, byte_count as u64);
    assert!(descriptor.complete);
    assert_eq!(
        std::fs::read(&descriptor.path).expect("read artifact"),
        vec![0; byte_count]
    );

    let buffer = handles.output_buffer.lock().await;
    assert_eq!(buffer.total_bytes(), byte_count);
    assert_eq!(
        buffer.omitted_bytes(),
        byte_count - UNIFIED_EXEC_OUTPUT_MAX_BYTES
    );
}

#[cfg(unix)]
#[tokio::test]
async fn local_early_sandbox_denial_carries_completed_artifact() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let artifact_root = codex_utils_absolute_path::AbsolutePathBuf::try_from(
        temp_dir.path().join("sandbox-artifacts"),
    )
    .expect("absolute artifact root");
    let store = OutputArtifactStore::new(
        ToolOutputSpillConfig {
            enabled: true,
            token_threshold: 1,
            preview_token_limit: 100,
            max_artifact_bytes: 1024,
            max_store_bytes: 4096,
            retention_days: 7,
            output_dir: artifact_root,
        },
        "thread-denial",
    )
    .expect("enabled artifact store");
    let args = vec![
        "-c".to_string(),
        "printf 'Operation not permitted'; exit 1".to_string(),
    ];
    let spawned = codex_utils_pty::pipe::spawn_process_no_stdin(
        "/bin/sh",
        &args,
        temp_dir.path(),
        &HashMap::new(),
        &None,
    )
    .await
    .expect("spawn local pipe process");

    let err = UnifiedExecProcess::from_spawned(
        spawned,
        codex_sandboxing::SandboxType::LinuxSeccomp,
        Box::new(NoopSpawnLifecycle),
        Some(store.spool()),
    )
    .await
    .expect_err("sandbox denial should be detected");
    let UnifiedExecError::SandboxDenied {
        output_artifact: Some(descriptor),
        ..
    } = err
    else {
        panic!("expected sandbox denial with artifact");
    };
    assert!(descriptor.complete);
    assert_eq!(
        std::fs::read_to_string(descriptor.path).expect("read denial artifact"),
        "Operation not permitted"
    );
}

struct MockExecProcess {
    process_id: ProcessId,
    write_response: WriteResponse,
    read_responses: Mutex<VecDeque<ReadResponse>>,
    terminate_error: Option<String>,
    wake_tx: watch::Sender<u64>,
}

impl MockExecProcess {
    async fn read(&self) -> Result<ReadResponse, ExecServerError> {
        Ok(self
            .read_responses
            .lock()
            .await
            .pop_front()
            .unwrap_or(ReadResponse {
                chunks: Vec::new(),
                next_seq: 1,
                exited: false,
                exit_code: None,
                closed: false,
                failure: None,
                sandbox_denied: false,
            }))
    }

    async fn terminate(&self) -> Result<(), ExecServerError> {
        if let Some(message) = &self.terminate_error {
            return Err(ExecServerError::Protocol(message.clone()));
        }
        Ok(())
    }
}

impl ExecProcess for MockExecProcess {
    fn process_id(&self) -> &ProcessId {
        &self.process_id
    }

    fn subscribe_wake(&self) -> watch::Receiver<u64> {
        self.wake_tx.subscribe()
    }

    fn subscribe_events(&self) -> ExecProcessEventReceiver {
        ExecProcessEventReceiver::empty()
    }

    fn read(
        &self,
        _after_seq: Option<u64>,
        _max_bytes: Option<usize>,
        _wait_ms: Option<u64>,
    ) -> ExecProcessFuture<'_, ReadResponse> {
        Box::pin(MockExecProcess::read(self))
    }

    fn write(&self, _chunk: Vec<u8>) -> ExecProcessFuture<'_, WriteResponse> {
        Box::pin(async { Ok(self.write_response.clone()) })
    }

    fn signal(&self, _signal: ProcessSignal) -> ExecProcessFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }

    fn terminate(&self) -> ExecProcessFuture<'_, ()> {
        Box::pin(MockExecProcess::terminate(self))
    }
}

async fn remote_process(
    write_status: WriteStatus,
    terminate_error: Option<String>,
) -> UnifiedExecProcess {
    let (wake_tx, _wake_rx) = watch::channel(0);
    let started = StartedExecProcess {
        process: Arc::new(MockExecProcess {
            process_id: "test-process".to_string().into(),
            write_response: WriteResponse {
                status: write_status,
            },
            read_responses: Mutex::new(VecDeque::new()),
            terminate_error,
            wake_tx,
        }),
    };

    UnifiedExecProcess::from_exec_server_started(started)
        .await
        .expect("remote process should start")
}

#[tokio::test]
async fn remote_write_unknown_process_marks_process_exited() {
    let process = remote_process(WriteStatus::UnknownProcess, /*terminate_error*/ None).await;

    let err = process
        .write(b"hello")
        .await
        .expect_err("expected write failure");

    assert!(matches!(err, UnifiedExecError::WriteToStdin));
    assert!(process.has_exited());
}

#[tokio::test]
async fn remote_write_closed_stdin_marks_process_exited() {
    let process = remote_process(WriteStatus::StdinClosed, /*terminate_error*/ None).await;

    let err = process
        .write(b"hello")
        .await
        .expect_err("expected write failure");

    assert!(matches!(err, UnifiedExecError::WriteToStdin));
    assert!(process.has_exited());
}

#[tokio::test]
async fn fail_and_terminate_preserves_failure_message() {
    let process = remote_process(WriteStatus::Accepted, /*terminate_error*/ None).await;

    process.fail_and_terminate("network denied".to_string());
    process.fail_and_terminate("second failure".to_string());

    assert!(process.has_exited());
    assert_eq!(
        process.failure_message(),
        Some("network denied".to_string())
    );
}

#[tokio::test]
async fn remote_terminate_confirmed_updates_state_on_success_only() {
    let process = remote_process(
        WriteStatus::Accepted,
        Some("terminate unavailable".to_string()),
    )
    .await;

    let err = process
        .terminate_confirmed()
        .await
        .expect_err("expected terminate failure");

    assert!(matches!(err, UnifiedExecError::ProcessFailed { .. }));
    assert!(!process.has_exited());

    let process = remote_process(WriteStatus::Accepted, /*terminate_error*/ None).await;

    process
        .terminate_confirmed()
        .await
        .expect("terminate should succeed");

    assert!(process.has_exited());
}
