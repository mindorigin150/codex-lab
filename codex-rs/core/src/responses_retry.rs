//! Shared retry and transport fallback decisions for Responses requests.

use std::time::Duration;

use crate::client::ModelClientSession;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::util::backoff;
use codex_client::RetryOperation;
use codex_features::Feature;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::WarningEvent;
use tokio_util::sync::CancellationToken;
use tracing::warn;

const INITIAL_CONNECTION_RETRY_DELAY: Duration = Duration::from_secs(5);
const MAX_CONNECTION_RETRY_DELAY: Duration = Duration::from_secs(60);
const SERVER_OVERLOADED_MAX_RETRY_DELAY: Duration = Duration::from_secs(10);
const SERVER_OVERLOADED_BACKOFF_CAP_ATTEMPT: u64 = 7;

#[derive(Debug, Clone, Copy)]
pub(crate) enum ResponsesStreamRequest {
    Sampling,
    RemoteCompactionV2,
}

pub(crate) struct ResponsesStreamRetryState {
    retries: u64,
    overload_retries: u64,
    connection_retries: u64,
    connection_retry_delay: Duration,
}

impl Default for ResponsesStreamRetryState {
    fn default() -> Self {
        Self {
            retries: 0,
            overload_retries: 0,
            connection_retries: 0,
            connection_retry_delay: INITIAL_CONNECTION_RETRY_DELAY,
        }
    }
}

pub(crate) fn server_overloaded_retry_delay(retry_count: u64) -> Duration {
    backoff(retry_count.min(SERVER_OVERLOADED_BACKOFF_CAP_ATTEMPT))
        .min(SERVER_OVERLOADED_MAX_RETRY_DELAY)
}

/// Handles a retryable stream error and returns `Ok(())` when the caller should
/// retry the request loop.
pub(crate) async fn handle_retryable_response_stream_error(
    retry_state: &mut ResponsesStreamRetryState,
    max_retries: u64,
    err: CodexErr,
    client_session: &mut ModelClientSession,
    sess: &Session,
    turn_context: &TurnContext,
    request: ResponsesStreamRequest,
    cancellation_token: Option<&CancellationToken>,
) -> Result<(), CodexErr> {
    if cancellation_token.is_some_and(CancellationToken::is_cancelled) {
        return Err(CodexErr::TurnAborted);
    }

    let operation = match request {
        ResponsesStreamRequest::Sampling => RetryOperation::Sampling,
        ResponsesStreamRequest::RemoteCompactionV2 => RetryOperation::RemoteCompactionV2,
    };

    if turn_context
        .config
        .features
        .enabled(Feature::UnboundedConnectionRetries)
        && matches!(request, ResponsesStreamRequest::Sampling)
        && matches!(err.details(), CodexErrorDetails::ConnectionFailed(_))
        && !turn_context.session_source.is_internal()
        && !turn_context.provider.info().is_amazon_bedrock()
    {
        let retry_delay = retry_state.connection_retry_delay;
        warn!(
            turn_id = %turn_context.sub_id,
            error = %err,
            ?retry_delay,
            "stream connection failed; waiting to retry"
        );
        notify_retry(
            sess,
            turn_context,
            "Reconnecting... waiting for network".to_string(),
            err,
            cancellation_token,
        )
        .await?;
        retry_state.connection_retries = retry_state.connection_retries.saturating_add(1);
        codex_client::record_retry!(retry_state.connection_retries, retry_delay, operation);
        wait_for_retry_delay(retry_delay, cancellation_token).await?;
        retry_state.connection_retry_delay = retry_delay
            .saturating_mul(2)
            .min(MAX_CONNECTION_RETRY_DELAY);
        return Ok(());
    }

    if matches!(err.details(), CodexErrorDetails::ServerOverloaded) {
        retry_state.overload_retries += 1;
        let retry_count = retry_state.overload_retries;
        let delay = server_overloaded_retry_delay(retry_count);
        log_retry(request, turn_context, &err, retry_count, max_retries, delay);
        notify_retry(
            sess,
            turn_context,
            format!("Reconnecting... overload attempt {retry_count}"),
            err,
            cancellation_token,
        )
        .await?;
        codex_client::record_retry!(retry_count, delay, operation);
        wait_for_retry_delay(delay, cancellation_token).await?;
        return Ok(());
    }

    if retry_state.retries >= max_retries
        && client_session.try_switch_fallback_transport(
            &turn_context.session_telemetry,
            &turn_context.model_info,
        )
    {
        let send_warning = sess.send_event(
            turn_context,
            EventMsg::Warning(WarningEvent {
                message: format!("Falling back from WebSockets to HTTPS transport. {err:#}"),
            }),
        );
        await_with_cancellation(send_warning, cancellation_token).await?;
        retry_state.retries = 0;
        return Ok(());
    }

    if retry_state.retries < max_retries {
        retry_state.retries += 1;
        let retry_count = retry_state.retries;
        let delay = err.retry_delay().unwrap_or_else(|| backoff(retry_count));
        log_retry(request, turn_context, &err, retry_count, max_retries, delay);

        // In release builds, hide the first websocket retry notification to reduce noisy
        // transient reconnect messages. In debug builds, keep full visibility for diagnosis.
        let report_error = retry_count > 1
            || cfg!(debug_assertions)
            || !sess.services.model_client.responses_websocket_enabled();
        if report_error {
            // Surface retry information to any UI/front-end so the user understands what is
            // happening instead of staring at a seemingly frozen screen.
            notify_retry(
                sess,
                turn_context,
                format!("Reconnecting... {retry_count}/{max_retries}"),
                err,
                cancellation_token,
            )
            .await?;
        }
        codex_client::record_retry!(retry_count, delay, operation);
        wait_for_retry_delay(delay, cancellation_token).await?;
        return Ok(());
    }

    Err(err)
}

async fn notify_retry(
    sess: &Session,
    turn_context: &TurnContext,
    message: String,
    err: CodexErr,
    cancellation_token: Option<&CancellationToken>,
) -> Result<(), CodexErr> {
    await_with_cancellation(
        sess.notify_stream_error(turn_context, message, err),
        cancellation_token,
    )
    .await
}

async fn await_with_cancellation<F>(
    future: F,
    cancellation_token: Option<&CancellationToken>,
) -> Result<(), CodexErr>
where
    F: std::future::Future<Output = ()>,
{
    if let Some(cancellation_token) = cancellation_token {
        tokio::select! {
            biased;
            _ = cancellation_token.cancelled() => Err(CodexErr::TurnAborted),
            _ = future => Ok(()),
        }
    } else {
        future.await;
        Ok(())
    }
}

async fn wait_for_retry_delay(
    delay: Duration,
    cancellation_token: Option<&CancellationToken>,
) -> Result<(), CodexErr> {
    if let Some(cancellation_token) = cancellation_token {
        tokio::select! {
            biased;
            _ = cancellation_token.cancelled() => Err(CodexErr::TurnAborted),
            _ = tokio::time::sleep(delay) => Ok(()),
        }
    } else {
        tokio::time::sleep(delay).await;
        Ok(())
    }
}

fn log_retry(
    request: ResponsesStreamRequest,
    turn_context: &TurnContext,
    err: &CodexErr,
    retries: u64,
    max_retries: u64,
    delay: Duration,
) {
    match request {
        ResponsesStreamRequest::Sampling => {
            if matches!(err.details(), CodexErrorDetails::ServerOverloaded) {
                warn!(
                    turn_id = %turn_context.sub_id,
                    retries,
                    delay = ?delay,
                    sampling_error = %err,
                    "model overloaded; retrying sampling request indefinitely (attempt {retries})...",
                );
            } else {
                warn!(
                    turn_id = %turn_context.sub_id,
                    retries,
                    max_retries,
                    sampling_error = %err,
                    "stream disconnected - retrying sampling request ({retries}/{max_retries} in {delay:?})...",
                );
            }
        }
        ResponsesStreamRequest::RemoteCompactionV2 => {
            if matches!(err.details(), CodexErrorDetails::ServerOverloaded) {
                warn!(
                    turn_id = %turn_context.sub_id,
                    retries,
                    delay = ?delay,
                    compact_error = %err,
                    "model overloaded; retrying remote compaction v2 indefinitely (attempt {retries})...",
                );
            } else {
                warn!(
                    turn_id = %turn_context.sub_id,
                    retries,
                    max_retries,
                    compact_error = %err,
                    "remote compaction v2 stream failed; retrying request after delay"
                );
            }
        }
    }
}

#[cfg(test)]
#[path = "responses_retry_tests.rs"]
mod tests;
