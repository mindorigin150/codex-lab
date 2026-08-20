use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use codex_protocol::items::CollabAgentTool;
use codex_protocol::items::CollabAgentToolCallItem;
use codex_protocol::items::CollabAgentToolCallStatus;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::AgentStatus;
use tokio::time::Instant;

use crate::session::InputQueueActivity;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BarrierOutcome {
    Completed,
    Failed,
    Steered,
    TimedOut,
}

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
        let target_ids = targets.iter().map(|(id, _)| *id).collect::<Vec<_>>();
        let turn_state = session
            .input_queue
            .turn_state_for_sub_id(&session.active_turn, &turn_context.sub_id)
            .await;
        let (mut activity_rx, pending_activity) = session
            .input_queue
            .subscribe_activity(turn_state.as_deref())
            .await;
        let call_id = format!("auto-barrier-{}", codex_protocol::ThreadId::new());

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
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let outcome = wait_for_targets(
            session,
            &target_ids,
            &mut activity_rx,
            pending_activity,
            deadline,
        )
        .await;
        let statuses = snapshot_statuses(session, &target_ids).await;
        let status = match outcome {
            BarrierOutcome::Failed | BarrierOutcome::TimedOut => CollabAgentToolCallStatus::Failed,
            BarrierOutcome::Completed | BarrierOutcome::Steered => {
                CollabAgentToolCallStatus::Completed
            }
        };
        session
            .emit_turn_item_completed(
                turn_context,
                TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                    id: call_id,
                    tool: CollabAgentTool::Wait,
                    status,
                    sender_thread_id: session.thread_id,
                    receiver_thread_ids: target_ids.clone(),
                    receiver_agents: Vec::new(),
                    prompt: None,
                    model: None,
                    reasoning_effort: None,
                    agents_states: statuses,
                }),
            )
            .await;

        match outcome {
            BarrierOutcome::Completed => {
                session
                    .input_queue
                    .accept_mailbox_delivery_for_current_turn(
                        &session.active_turn,
                        &turn_context.sub_id,
                    )
                    .await;
                session.services.agent_control.settle_blocking_agents(
                    session.thread_id,
                    &target_ids,
                    false,
                );
                return false;
            }
            BarrierOutcome::Failed => {
                session
                    .input_queue
                    .accept_mailbox_delivery_for_current_turn(
                        &session.active_turn,
                        &turn_context.sub_id,
                    )
                    .await;
                session.services.agent_control.settle_blocking_agents(
                    session.thread_id,
                    &target_ids,
                    true,
                );
                return false;
            }
            BarrierOutcome::Steered => {
                session
                    .input_queue
                    .accept_mailbox_delivery_for_current_turn(
                        &session.active_turn,
                        &turn_context.sub_id,
                    )
                    .await;
                return true;
            }
            BarrierOutcome::TimedOut => {}
        }
    }
}

async fn snapshot_statuses(
    session: &Session,
    target_ids: &[codex_protocol::ThreadId],
) -> HashMap<codex_protocol::ThreadId, AgentStatus> {
    let mut statuses = HashMap::new();
    for target_id in target_ids {
        let status = session.services.agent_control.get_status(*target_id).await;
        statuses.insert(*target_id, status);
    }
    statuses
}

async fn wait_for_targets(
    session: &Session,
    target_ids: &[codex_protocol::ThreadId],
    activity_rx: &mut tokio::sync::watch::Receiver<InputQueueActivity>,
    pending_activity: Option<InputQueueActivity>,
    deadline: Instant,
) -> BarrierOutcome {
    if pending_activity == Some(InputQueueActivity::Steer) {
        return BarrierOutcome::Steered;
    }
    loop {
        if let Some(failed) = targets_ready(session, target_ids).await {
            return if failed {
                BarrierOutcome::Failed
            } else {
                BarrierOutcome::Completed
            };
        }
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return BarrierOutcome::TimedOut,
            changed = activity_rx.changed() => {
                if changed.is_err() {
                    return BarrierOutcome::TimedOut;
                }
                if *activity_rx.borrow_and_update() == InputQueueActivity::Steer {
                    return BarrierOutcome::Steered;
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(25)) => {}
        }
    }
}

async fn targets_ready(session: &Session, target_ids: &[codex_protocol::ThreadId]) -> Option<bool> {
    let mut failed = false;
    for target_id in target_ids {
        let status = session.services.agent_control.get_status(*target_id).await;
        let terminal = matches!(
            status,
            AgentStatus::Completed(_)
                | AgentStatus::Errored(_)
                | AgentStatus::Shutdown
                | AgentStatus::NotFound
        );
        if !terminal {
            return None;
        }
        if matches!(
            status,
            AgentStatus::Completed(_) | AgentStatus::Errored(_) | AgentStatus::Shutdown
        ) && session
            .services
            .agent_control
            .current_completion_receipt(*target_id)
            .is_none()
        {
            return None;
        }
        failed |= !matches!(status, AgentStatus::Completed(_));
    }
    Some(failed)
}
