use super::*;
use crate::session::InputQueueActivity;
use crate::tools::handlers::multi_agents_spec::WaitAgentTimeoutOptions;
use crate::tools::handlers::multi_agents_spec::create_wait_agent_tool_v2;
use codex_protocol::ThreadId;
use codex_tools::ToolSpec;
use futures::FutureExt;
use futures::future::BoxFuture;
use futures::future::select_all;
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::Instant;

#[derive(Default)]
pub(crate) struct Handler {
    options: WaitAgentTimeoutOptions,
}

impl Handler {
    pub(crate) fn new(options: WaitAgentTimeoutOptions) -> Self {
        Self { options }
    }
}

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("wait_agent")
    }
    fn spec(&self) -> ToolSpec {
        create_wait_agent_tool_v2(self.options)
    }
    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(self.handle_call(invocation))
    }
}

pub(crate) struct Target {
    requested: String,
    id: ThreadId,
    status_rx: Option<tokio::sync::watch::Receiver<AgentStatus>>,
}

pub(crate) struct BarrierResult {
    pub(crate) outcome: WaitOutcome,
    pub(crate) status: HashMap<String, AgentStatus>,
}

pub(crate) struct PreparedTargetBarrier {
    targets: Vec<Target>,
    activity_rx: tokio::sync::watch::Receiver<InputQueueActivity>,
    pending: Option<InputQueueActivity>,
}

impl PreparedTargetBarrier {
    pub(crate) fn target_ids(&self) -> Vec<ThreadId> {
        self.targets.iter().map(|target| target.id).collect()
    }
}

pub(crate) async fn prepare_target_barrier(
    session: &std::sync::Arc<crate::session::session::Session>,
    turn: &std::sync::Arc<crate::session::turn_context::TurnContext>,
    requested_targets: &[String],
) -> Result<PreparedTargetBarrier, FunctionCallError> {
    if requested_targets.is_empty() {
        return Err(FunctionCallError::RespondToModel(
            "targets must be non-empty".to_string(),
        ));
    }
    let mut targets = Vec::new();
    for requested in requested_targets {
        let id = resolve_agent_target(session, turn, requested).await?;
        if id == session.thread_id {
            return Err(FunctionCallError::RespondToModel(
                "wait_agent cannot target the current agent".to_string(),
            ));
        }
        session
            .services
            .agent_control
            .ensure_agent_known(id)
            .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;
        if targets.iter().any(|target: &Target| target.id == id) {
            continue;
        }
        let status_rx = session
            .services
            .agent_control
            .subscribe_status(id)
            .await
            .ok();
        targets.push(Target {
            requested: requested.clone(),
            id,
            status_rx,
        });
    }
    let turn_state = session
        .input_queue
        .turn_state_for_sub_id(&session.active_turn, &turn.sub_id)
        .await;
    let (activity_rx, pending) = session
        .input_queue
        .subscribe_activity(turn_state.as_deref())
        .await;
    Ok(PreparedTargetBarrier {
        targets,
        activity_rx,
        pending,
    })
}

pub(crate) async fn wait_for_target_barrier(
    session: &std::sync::Arc<crate::session::session::Session>,
    mut barrier: PreparedTargetBarrier,
    deadline: Instant,
) -> BarrierResult {
    let outcome = wait_for_targets(
        session,
        &mut barrier.targets,
        &mut barrier.activity_rx,
        barrier.pending,
        deadline,
    )
    .await;
    BarrierResult {
        outcome,
        status: snapshot(session, &barrier.targets).await,
    }
}

impl Handler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            payload,
            call_id,
            ..
        } = invocation;
        let args: WaitArgs = parse_arguments(&function_arguments(payload)?)?;
        if args.targets.is_empty() {
            return Err(FunctionCallError::RespondToModel(
                "targets must be non-empty".to_string(),
            ));
        }
        let config = &turn.config.multi_agent_v2;
        let timeout_ms = args.timeout_ms.unwrap_or(config.default_wait_timeout_ms);
        if timeout_ms < config.min_wait_timeout_ms {
            return Err(FunctionCallError::RespondToModel(format!(
                "timeout_ms must be at least {}",
                config.min_wait_timeout_ms
            )));
        }
        if timeout_ms > config.max_wait_timeout_ms {
            return Err(FunctionCallError::RespondToModel(format!(
                "timeout_ms must be at most {}",
                config.max_wait_timeout_ms
            )));
        }

        let requested_targets = args.targets;
        let prepared = prepare_target_barrier(&session, &turn, &requested_targets).await?;
        let target_ids = prepared.target_ids();
        let target_pairs = prepared
            .targets
            .iter()
            .map(|target| (target.requested.clone(), target.id))
            .collect::<Vec<_>>();

        session
            .emit_turn_item_started(
                &turn,
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
                    agents_states: Default::default(),
                }),
            )
            .await;

        let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
        let barrier = wait_for_target_barrier(&session, prepared, deadline).await;
        let outcome = barrier.outcome;
        let status = barrier.status;
        let result = WaitAgentResult::new(outcome, status.clone());
        let tool_status = if matches!(outcome, WaitOutcome::Failed | WaitOutcome::TimedOut) {
            CollabAgentToolCallStatus::Failed
        } else {
            CollabAgentToolCallStatus::Completed
        };
        session
            .emit_turn_item_completed(
                &turn,
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
                    agents_states: target_pairs
                        .iter()
                        .filter_map(|(requested, target_id)| {
                            status
                                .get(requested)
                                .cloned()
                                .map(|status| (*target_id, status))
                        })
                        .collect(),
                }),
            )
            .await;
        Ok(boxed_tool_output(result))
    }
}

impl CoreToolRuntime for Handler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitArgs {
    targets: Vec<String>,
    timeout_ms: Option<i64>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WaitOutcome {
    Completed,
    Failed,
    Steered,
    TimedOut,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct WaitAgentResult {
    pub(crate) message: String,
    pub(crate) timed_out: bool,
    pub(crate) outcome: WaitOutcome,
    pub(crate) statuses: HashMap<String, AgentStatus>,
}

impl WaitAgentResult {
    fn new(outcome: WaitOutcome, status: HashMap<String, AgentStatus>) -> Self {
        let message = match outcome {
            WaitOutcome::Completed => "All target agents completed.",
            WaitOutcome::Failed => "All target agents settled; at least one failed.",
            WaitOutcome::Steered => "Wait interrupted by new input.",
            WaitOutcome::TimedOut => "Wait timed out.",
        };
        Self {
            message: message.to_string(),
            timed_out: outcome == WaitOutcome::TimedOut,
            outcome,
            statuses: status,
        }
    }
}

impl ToolOutput for WaitAgentResult {
    fn log_preview(&self) -> String {
        tool_output_json_text(self, "wait_agent")
    }
    fn success_for_logging(&self) -> bool {
        true
    }
    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(call_id, payload, self, None, "wait_agent")
    }
    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        tool_output_code_mode_result(self, "wait_agent")
    }
}

fn barrier_terminal(status: &AgentStatus) -> bool {
    matches!(
        status,
        AgentStatus::Completed(_)
            | AgentStatus::Errored(_)
            | AgentStatus::Interrupted
            | AgentStatus::Shutdown
            | AgentStatus::NotFound
    )
}

async fn snapshot(
    session: &crate::session::session::Session,
    targets: &[Target],
) -> HashMap<String, AgentStatus> {
    let mut result = HashMap::new();
    for target in targets {
        result.insert(
            target.requested.clone(),
            session.services.agent_control.get_status(target.id).await,
        );
    }
    result
}

async fn targets_ready(
    session: &crate::session::session::Session,
    targets: &[Target],
) -> Option<bool> {
    let mut failed = false;
    for target in targets {
        let status = session.services.agent_control.get_status(target.id).await;
        if !barrier_terminal(&status) {
            return None;
        }
        if matches!(
            status,
            AgentStatus::Completed(_) | AgentStatus::Errored(_) | AgentStatus::Shutdown
        ) && session
            .services
            .agent_control
            .current_completion_receipt(target.id)
            .is_none()
        {
            return None;
        }
        failed |= !matches!(status, AgentStatus::Completed(_));
    }
    Some(failed)
}

async fn wait_for_targets(
    session: &crate::session::session::Session,
    targets: &mut [Target],
    activity_rx: &mut tokio::sync::watch::Receiver<InputQueueActivity>,
    pending: Option<InputQueueActivity>,
    deadline: Instant,
) -> WaitOutcome {
    if pending == Some(InputQueueActivity::Steer) {
        return WaitOutcome::Steered;
    }
    loop {
        if let Some(failed) = targets_ready(session, targets).await {
            return if failed {
                WaitOutcome::Failed
            } else {
                WaitOutcome::Completed
            };
        }
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return WaitOutcome::TimedOut,
            changed = activity_rx.changed() => {
                if changed.is_err() { return WaitOutcome::TimedOut; }
                if *activity_rx.borrow_and_update() == InputQueueActivity::Steer { return WaitOutcome::Steered; }
            }
            changed = next_status_change(targets) => {
                if let Some((index, Err(_))) = changed {
                    targets[index].status_rx = None;
                }
            }
        }
    }
}

async fn next_status_change(
    targets: &mut [Target],
) -> Option<(usize, Result<(), tokio::sync::watch::error::RecvError>)> {
    let changes = targets
        .iter_mut()
        .enumerate()
        .filter_map(|(index, target)| {
            target.status_rx.as_mut().map(|status_rx| {
                async move { (index, status_rx.changed().await) }.boxed() as BoxFuture<'_, _>
            })
        })
        .collect::<Vec<_>>();
    if changes.is_empty() {
        return futures::future::pending().await;
    }
    let (changed, _, _) = select_all(changes).await;
    Some(changed)
}
