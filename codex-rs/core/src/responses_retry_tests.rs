use super::ResponsesStreamRequest;
use super::log_retry;
use super::server_overloaded_retry_delay;
use super::wait_for_retry_delay;
use crate::session::tests::make_session_and_context;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing_test::internal::MockWriter;

#[test]
fn server_overloaded_retry_delay_is_capped() {
    assert!(server_overloaded_retry_delay(1) < Duration::from_secs(1));
    assert_eq!(
        server_overloaded_retry_delay(u64::MAX),
        Duration::from_secs(10)
    );
}

#[tokio::test]
async fn retry_delay_returns_turn_aborted_when_cancelled() {
    let cancellation_token = CancellationToken::new();
    let cancel = cancellation_token.clone();
    tokio::spawn(async move {
        tokio::task::yield_now().await;
        cancel.cancel();
    });

    let result = tokio::time::timeout(
        Duration::from_secs(1),
        wait_for_retry_delay(Duration::from_secs(60), Some(&cancellation_token)),
    )
    .await
    .expect("retry delay should observe cancellation");

    assert!(matches!(
        result,
        Err(err) if matches!(err.details(), CodexErrorDetails::TurnAborted)
    ));
}

#[tokio::test]
async fn sampling_retry_logs_stream_error_context() {
    let (_session, turn_context) = make_session_and_context().await;
    let buffer: &'static std::sync::Mutex<Vec<u8>> =
        Box::leak(Box::new(std::sync::Mutex::new(Vec::new())));
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .with_writer(MockWriter::new(buffer))
        .finish();
    let _subscriber_guard = tracing::subscriber::set_default(subscriber);

    log_retry(
        ResponsesStreamRequest::Sampling,
        &turn_context,
        &CodexErr::Stream("websocket closed by server before response.completed".to_string()),
        /*retries*/ 2,
        /*max_retries*/ 5,
        Duration::from_secs(1),
    );

    let logs = String::from_utf8(
        buffer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone(),
    )
    .expect("retry log should be valid utf-8");
    assert!(logs.contains("stream disconnected - retrying sampling request"));
    assert!(logs.contains(&format!("turn_id={}", turn_context.sub_id)));
    assert!(logs.contains("retries=2"));
    assert!(logs.contains("max_retries=5"));
    assert!(logs.contains(
        "sampling_error=stream disconnected before completion: websocket closed by server before response.completed"
    ));
}
