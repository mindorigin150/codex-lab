use super::*;
use crate::agent::control::SpawnAgentForkMode;
use crate::agent::control::SpawnAgentOptions;
use crate::agent::exceeds_thread_spawn_depth_limit;
use crate::agent::next_thread_spawn_depth;
use crate::agent::role::EXPLORER_ROLE_NAME;
use crate::agent::role::REVIEWER_ROLE_NAME;
use crate::agent_communication::AgentCommunicationContext;
use crate::agent_communication::AgentCommunicationKind;
use crate::tools::handlers::multi_agents_spec::SpawnAgentToolOptions;
use crate::tools::handlers::multi_agents_spec::create_spawn_agent_tool_v2;
use crate::tools::handlers::multi_agents_spec::create_spawn_agents_tool_v2;
use crate::tools::handlers::multi_agents_v2::message_tool::message_content;
use codex_protocol::AgentPath;
use codex_tools::ToolSpec;

#[derive(Default)]
pub(crate) struct Handler {
    options: SpawnAgentToolOptions,
}

impl Handler {
    pub(crate) fn new(options: SpawnAgentToolOptions) -> Self {
        Self { options }
    }
}

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("spawn_agent")
    }

    fn spec(&self) -> ToolSpec {
        create_spawn_agent_tool_v2(self.options.clone())
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move { handle_spawn_agent(invocation).await.map(boxed_tool_output) })
    }
}

#[derive(Default)]
pub(crate) struct BatchHandler {
    options: SpawnAgentToolOptions,
}

impl BatchHandler {
    pub(crate) fn new(options: SpawnAgentToolOptions) -> Self {
        Self { options }
    }
}

impl ToolExecutor<ToolInvocation> for BatchHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("spawn_agents")
    }

    fn spec(&self) -> ToolSpec {
        create_spawn_agents_tool_v2(self.options.clone())
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move { handle_spawn_agents(invocation).await.map(boxed_tool_output) })
    }
}

async fn handle_spawn_agent(
    invocation: ToolInvocation,
) -> Result<SpawnAgentResult, FunctionCallError> {
    let arguments = function_arguments(invocation.payload.clone())?;
    let args: SpawnAgentArgs = parse_arguments(&arguments)?;
    let prepared = prepare_spawn_agent(&invocation, args).await?;
    commit_spawn_agent(
        invocation, prepared, /*emit_activity_now*/ true,
        /*reserved_v2_residency_slot*/ None,
    )
    .await
    .map(|spawned| spawned.result)
}

struct PreparedSpawnAgent {
    config: crate::config::Config,
    communication: codex_protocol::protocol::InterAgentCommunication,
    fork_mode: Option<SpawnAgentForkMode>,
    spawn_source: codex_protocol::protocol::SessionSource,
    new_agent_path: AgentPath,
    role_name: String,
}

async fn prepare_spawn_agent(
    invocation: &ToolInvocation,
    args: SpawnAgentArgs,
) -> Result<PreparedSpawnAgent, FunctionCallError> {
    let session = &invocation.session;
    let turn = &invocation.turn;
    let role_name = args.agent_type.trim();
    if role_name.is_empty() {
        return Err(FunctionCallError::RespondToModel(
            "agent_type must not be empty".to_string(),
        ));
    }
    let fork_mode = args.fork_mode(Some(role_name))?;

    let message = message_content(args.message)?;
    let session_source = turn.session_source.clone();
    let child_depth = next_thread_spawn_depth(&session_source);
    if exceeds_thread_spawn_depth_limit(child_depth, turn.config.agent_max_depth) {
        return Err(FunctionCallError::RespondToModel(
            "Agent depth limit reached. Solve the task yourself.".to_string(),
        ));
    }
    let mut config =
        build_agent_spawn_config(&session.get_base_instructions().await, turn.as_ref())?;
    let validate_effective_model_overrides = args.model.is_some()
        || args.reasoning_effort.is_some()
        || turn.config.agent_default_subagent_model.is_some()
        || turn
            .config
            .agent_default_subagent_reasoning_effort
            .is_some();
    if let Some(service_tier) = args.service_tier.as_ref() {
        config.service_tier = Some(service_tier.clone());
    }
    apply_requested_spawn_agent_model_overrides(
        session,
        turn.as_ref(),
        &mut config,
        args.model.as_deref(),
        args.reasoning_effort.clone(),
    )
    .await?;
    let model_before_role = config.model.clone();
    apply_spawn_agent_role(session, &mut config, Some(role_name)).await?;
    if validate_effective_model_overrides || config.model != model_before_role {
        validate_effective_spawn_agent_model_overrides(session, turn.as_ref(), &config).await?;
    }
    apply_spawn_agent_service_tier(
        session,
        &mut config,
        turn.config.service_tier.as_deref(),
        args.service_tier.as_deref(),
    )
    .await?;
    apply_spawn_agent_runtime_overrides(&mut config, turn.as_ref())?;
    super::super::multi_agents_common::preflight_spawn_agent_sandbox(&config).await?;

    let spawn_source = thread_spawn_source(
        session.thread_id,
        &turn.session_source,
        child_depth,
        Some(role_name),
        Some(args.task_name.clone()),
    )?;
    let new_agent_path = spawn_source.get_agent_path().ok_or_else(|| {
        FunctionCallError::RespondToModel(
            "spawned agent is missing a canonical task name".to_string(),
        )
    })?;
    if session
        .services
        .agent_control
        .has_live_agent_path(&new_agent_path)
    {
        return Err(FunctionCallError::RespondToModel(format!(
            "agent path `{}` is already active; reuse that agent or choose another task_name",
            new_agent_path.as_str()
        )));
    }
    let author = turn
        .session_source
        .get_agent_path()
        .unwrap_or_else(AgentPath::root);
    let communication = communication_from_tool_message(author, new_agent_path.clone(), message);
    Ok(PreparedSpawnAgent {
        config,
        communication,
        fork_mode,
        spawn_source,
        new_agent_path,
        role_name: role_name.to_string(),
    })
}

async fn commit_spawn_agent(
    invocation: ToolInvocation,
    prepared: PreparedSpawnAgent,
    emit_activity_now: bool,
    reserved_v2_residency_slot: Option<crate::agent::control::V2ResidencySlot>,
) -> Result<SpawnedAgentResult, FunctionCallError> {
    let ToolInvocation {
        session,
        turn,
        call_id,
        ..
    } = invocation;
    let PreparedSpawnAgent {
        config,
        communication,
        fork_mode,
        spawn_source,
        new_agent_path,
        role_name,
    } = prepared;
    let context = AgentCommunicationContext::new(AgentCommunicationKind::Spawn, session.thread_id);
    let blocking_role = matches!(role_name.as_str(), EXPLORER_ROLE_NAME | REVIEWER_ROLE_NAME);
    let blocking_retry = if blocking_role {
        session
            .services
            .agent_control
            .begin_blocking_agent_start(session.thread_id)
            .map_err(FunctionCallError::RespondToModel)?
    } else {
        false
    };
    let options = SpawnAgentOptions {
        fork_parent_spawn_call_id: fork_mode.as_ref().map(|_| call_id.clone()),
        fork_mode,
        parent_thread_id: Some(session.thread_id),
        environments: Some(turn.environments.to_selections()),
    };
    let spawn_result = if let Some(residency_slot) = reserved_v2_residency_slot {
        Box::pin(
            session
                .services
                .agent_control
                .spawn_agent_with_reserved_v2_residency(
                    config,
                    communication,
                    context,
                    Some(spawn_source),
                    options,
                    residency_slot,
                ),
        )
        .await
    } else {
        Box::pin(
            session
                .services
                .agent_control
                .spawn_agent_with_communication(
                    config,
                    communication,
                    context,
                    Some(spawn_source),
                    options,
                ),
        )
        .await
    };
    let spawned_agent = match spawn_result {
        Ok(spawned_agent) => spawned_agent,
        Err(err) => {
            session
                .services
                .agent_control
                .cancel_blocking_agent_start(session.thread_id, blocking_retry);
            return Err(collab_spawn_error(err));
        }
    };
    let new_thread_id = spawned_agent.thread_id;
    if blocking_role {
        session
            .services
            .agent_control
            .register_blocking_agent(session.thread_id, new_thread_id);
    }
    let agent_snapshot = session
        .services
        .agent_control
        .get_agent_config_snapshot(new_thread_id)
        .await;
    let nickname = agent_snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.session_source.get_nickname())
        .or(spawned_agent.metadata.agent_nickname);
    let activity = SubAgentActivityItem {
        id: call_id,
        agent_thread_id: new_thread_id,
        agent_path: new_agent_path.clone(),
        kind: SubAgentActivityKind::Started,
        operation: Some(SubAgentActivityOperation::Spawn),
        generation: session
            .services
            .agent_control
            .current_agent_generation(new_thread_id),
    };
    if emit_activity_now {
        emit_sub_agent_activity(&session, &turn, activity.clone()).await;
        turn.session_telemetry.counter(
            "codex.multi_agent.spawn",
            /*inc*/ 1,
            &[("role", role_name.as_str()), ("version", "v2")],
        );
    }
    let task_name = String::from(new_agent_path);

    let hide_agent_metadata = turn.config.multi_agent_v2.hide_spawn_agent_metadata;
    let result = if hide_agent_metadata {
        SpawnAgentResult::HiddenMetadata { task_name }
    } else {
        SpawnAgentResult::WithNickname {
            task_name,
            nickname,
        }
    };
    Ok(SpawnedAgentResult {
        thread_id: new_thread_id,
        result,
        activity,
        role_name,
    })
}

async fn handle_spawn_agents(
    invocation: ToolInvocation,
) -> Result<SpawnAgentsResult, FunctionCallError> {
    let arguments = function_arguments(invocation.payload.clone())?;
    let args: SpawnAgentsArgs = parse_arguments(&arguments)?;
    if args.tasks.len() < 2 {
        return Err(FunctionCallError::RespondToModel(
            "tasks must contain at least two independent tasks; use spawn_agent for one task"
                .to_string(),
        ));
    }

    let mut names = std::collections::HashSet::new();
    for task in &args.tasks {
        let role_name = task.agent_type.trim();
        if role_name.is_empty() {
            return Err(FunctionCallError::RespondToModel(
                "agent_type must not be empty".to_string(),
            ));
        }
        if task.task_name.trim().is_empty() {
            return Err(FunctionCallError::RespondToModel(
                "task_name must not be empty".to_string(),
            ));
        }
        task.fork_mode(Some(role_name))?;
        message_content(task.message.clone())?;
        if !names.insert(task.task_name.trim().to_string()) {
            return Err(FunctionCallError::RespondToModel(format!(
                "duplicate task_name in batch: {}",
                task.task_name
            )));
        }
    }

    let calls = args.tasks.into_iter().enumerate().map(|(index, task)| {
        let mut child_invocation = invocation.clone();
        child_invocation.call_id = format!("{}:{index}", invocation.call_id);
        child_invocation.tool_name = ToolName::plain("spawn_agent");
        child_invocation.payload = ToolPayload::Function {
            arguments: serde_json::to_string(&task)
                .expect("serializing validated spawn task should succeed"),
        };
        (child_invocation, task)
    });
    // Complete every deterministic and sandbox preflight before starting the first child. In
    // particular, a bad role/config/model/path/message in a later task must not leave an earlier
    // task running merely because creation is intentionally serialized below.
    let mut prepared_calls = Vec::new();
    for (child_invocation, task) in calls {
        let prepared = prepare_spawn_agent(&child_invocation, task).await?;
        prepared_calls.push((child_invocation, prepared));
    }
    // Reserve the entire batch before the first child starts. This prevents a capacity failure
    // on a later task from producing a partially running batch.
    let configs = prepared_calls
        .iter()
        .map(|(_, prepared)| prepared.config.clone())
        .collect::<Vec<_>>();
    let mut residency_slots = invocation
        .session
        .services
        .agent_control
        .reserve_v2_batch_residency_slots(&configs)
        .await
        .map_err(collab_spawn_error)?
        .into_iter();

    // Creation is serialized inside this one tool call so registry, path, and blocking-barrier
    // mutations cannot race. Each child starts working immediately, so their actual
    // investigations overlap before this call returns.
    let mut spawned = Vec::new();
    for (child_invocation, prepared) in prepared_calls {
        let residency_slot = residency_slots
            .next()
            .expect("batch residency reservation must match prepared task count");
        match commit_spawn_agent(
            child_invocation,
            prepared,
            /*emit_activity_now*/ false,
            Some(residency_slot),
        )
        .await
        {
            Ok(result) => spawned.push(result),
            Err(err) => {
                for result in spawned {
                    let generation = invocation
                        .session
                        .services
                        .agent_control
                        .current_agent_generation(result.thread_id);
                    invocation
                        .session
                        .services
                        .agent_control
                        .cleanup_rolled_back_agent(
                            invocation.session.thread_id,
                            result.thread_id,
                            generation,
                        );
                    let _ = invocation
                        .session
                        .services
                        .agent_control
                        .close_agent(result.thread_id)
                        .await;
                }
                return Err(err);
            }
        }
    }

    // Publish only after every child started successfully. A compensated failure cannot leave
    // a misleading Started item in the parent rollout.
    for result in &spawned {
        emit_sub_agent_activity(
            &invocation.session,
            &invocation.turn,
            result.activity.clone(),
        )
        .await;
        invocation.turn.session_telemetry.counter(
            "codex.multi_agent.spawn",
            /*inc*/ 1,
            &[("role", result.role_name.as_str()), ("version", "v2")],
        );
    }

    Ok(SpawnAgentsResult {
        agents: spawned.into_iter().map(|result| result.result).collect(),
    })
}

impl CoreToolRuntime for Handler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

impl CoreToolRuntime for BatchHandler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SpawnAgentArgs {
    message: String,
    task_name: String,
    agent_type: String,
    model: Option<String>,
    reasoning_effort: Option<ReasoningEffort>,
    service_tier: Option<String>,
    fork_turns: Option<String>,
    fork_context: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnAgentsArgs {
    tasks: Vec<SpawnAgentArgs>,
}

struct SpawnedAgentResult {
    thread_id: codex_protocol::ThreadId,
    result: SpawnAgentResult,
    activity: SubAgentActivityItem,
    role_name: String,
}

impl SpawnAgentArgs {
    fn fork_mode(
        &self,
        role_name: Option<&str>,
    ) -> Result<Option<SpawnAgentForkMode>, FunctionCallError> {
        if self.fork_context.is_some() {
            return Err(FunctionCallError::RespondToModel(
                "fork_context is not supported in MultiAgentV2; use fork_turns instead".to_string(),
            ));
        }

        let fork_turns = self
            .fork_turns
            .as_deref()
            .map(str::trim)
            .filter(|fork_turns| !fork_turns.is_empty());

        if role_name == Some(crate::agent::role::EXPLORER_ROLE_NAME) {
            if fork_turns.is_none_or(|fork_turns| fork_turns.eq_ignore_ascii_case("none")) {
                return Ok(None);
            }
            return Err(FunctionCallError::RespondToModel(
                "explorer agents require fresh context; omit fork_turns or set it to `none`"
                    .to_string(),
            ));
        }

        let fork_turns = fork_turns.unwrap_or("all");

        if fork_turns.eq_ignore_ascii_case("none") {
            return Ok(None);
        }
        if fork_turns.eq_ignore_ascii_case("all") {
            return Ok(Some(SpawnAgentForkMode::FullHistory));
        }

        let last_n_turns = fork_turns.parse::<usize>().map_err(|_| {
            FunctionCallError::RespondToModel(
                "fork_turns must be `none`, `all`, or a positive integer string".to_string(),
            )
        })?;
        if last_n_turns == 0 {
            return Err(FunctionCallError::RespondToModel(
                "fork_turns must be `none`, `all`, or a positive integer string".to_string(),
            ));
        }

        Ok(Some(SpawnAgentForkMode::LastNTurns(last_n_turns)))
    }
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum SpawnAgentResult {
    WithNickname {
        task_name: String,
        nickname: Option<String>,
    },
    HiddenMetadata {
        task_name: String,
    },
}

#[derive(Debug, Serialize)]
pub(crate) struct SpawnAgentsResult {
    agents: Vec<SpawnAgentResult>,
}

impl ToolOutput for SpawnAgentsResult {
    fn log_preview(&self) -> String {
        tool_output_json_text(self, "spawn_agents")
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(call_id, payload, self, Some(true), "spawn_agents")
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        tool_output_code_mode_result(self, "spawn_agents")
    }
}

impl ToolOutput for SpawnAgentResult {
    fn log_preview(&self) -> String {
        tool_output_json_text(self, "spawn_agent")
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(call_id, payload, self, Some(true), "spawn_agent")
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        tool_output_code_mode_result(self, "spawn_agent")
    }
}
