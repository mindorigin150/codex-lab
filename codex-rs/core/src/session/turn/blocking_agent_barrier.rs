use std::collections::HashMap;
use std::sync::Arc;

use codex_protocol::items::CollabAgentTool;
use codex_protocol::items::CollabAgentToolCallItem;
use codex_protocol::items::CollabAgentToolCallStatus;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::WarningEvent;

use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::tools::handlers::multi_agents_v2::wait::WaitOutcome;
use crate::tools::handlers::multi_agents_v2::wait::prepare_target_barrier;
use crate::tools::handlers::multi_agents_v2::wait::wait_for_target_barrier;

pub(super) async fn enforce_blocking_agent_barrier(
    session: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
) -> bool {
    loop {
        let targets = session
            .services
            .agent_control
            .blocking_agent_targets(session.thread_id);
        if targets.is_empty() {
            return false;
        }
        let target_ids = targets
            .iter()
            .map(|(thread_id, _)| *thread_id)
            .collect::<Vec<_>>();
        let target_names = targets
            .iter()
            .map(|(_, target)| target.clone())
            .collect::<Vec<_>>();
        let call_id = format!("auto-barrier-{}", codex_protocol::ThreadId::new());
        let prepared = match prepare_target_barrier(session, turn_context, &target_names).await {
            Ok(prepared) => prepared,
            Err(err) => {
                session.services.agent_control.settle_blocking_agents(
                    session.thread_id,
                    &target_ids,
                    /*failed*/ true,
                );
                session
                    .send_event(
                        turn_context,
                        EventMsg::Warning(WarningEvent {
                            message: format!("Blocking-agent barrier failed: {err}"),
                        }),
                    )
                    .await;
                return false;
            }
        };
        session
            .emit_turn_item_started(
                turn_context,
                &TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                    id: call_id.clone(),
                    tool: CollabAgentTool::Wait,
                    status: CollabAgentToolCallStatus::InProgress,
                    sender_thread_id: session.thread_id,
                    receiver_thread_ids: target_ids.clone(),
                    receiver_agents: Vec::new(),
                    prompt: None,
                    model: None,
                    reasoning_effort: None,
                    agents_states: HashMap::new(),
                }),
            )
            .await;

        let timeout_ms = turn_context.config.multi_agent_v2.default_wait_timeout_ms as u64;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        let barrier = wait_for_target_barrier(session, prepared, deadline).await;
        let agents_states = targets
            .iter()
            .filter_map(|(thread_id, target)| {
                barrier
                    .status
                    .get(target)
                    .cloned()
                    .map(|status| (*thread_id, status))
            })
            .collect();
        let tool_status = match barrier.outcome {
            WaitOutcome::Completed | WaitOutcome::Steered => CollabAgentToolCallStatus::Completed,
            WaitOutcome::Failed | WaitOutcome::TimedOut => CollabAgentToolCallStatus::Failed,
        };
        session
            .emit_turn_item_completed(
                turn_context,
                TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                    id: call_id,
                    tool: CollabAgentTool::Wait,
                    status: tool_status,
                    sender_thread_id: session.thread_id,
                    receiver_thread_ids: target_ids.clone(),
                    receiver_agents: Vec::new(),
                    prompt: None,
                    model: None,
                    reasoning_effort: None,
                    agents_states,
                }),
            )
            .await;

        match barrier.outcome {
            WaitOutcome::Completed => {
                session.services.agent_control.settle_blocking_agents(
                    session.thread_id,
                    &target_ids,
                    /*failed*/ false,
                );
                return false;
            }
            WaitOutcome::Failed => {
                session.services.agent_control.settle_blocking_agents(
                    session.thread_id,
                    &target_ids,
                    /*failed*/ true,
                );
                return false;
            }
            WaitOutcome::Steered => return true,
            WaitOutcome::TimedOut => {}
        }
    }
}
