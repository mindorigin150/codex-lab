use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::agent::role::EXPLORER_ROLE_NAME;
use crate::agent::role::PLAN_EVIDENCE_ROLE_NAME;
use crate::agent::role::PLAN_REVIEW_ROLE_NAME;
use crate::agent::role::RESULT_REVIEW_ROLE_NAME;
use crate::agent::role::TASK_CONTRACT_ROLE_NAME;
use crate::agent::role::WORKER_PLAN_ROLE_NAME;
use crate::agent::role::WORKER_ROLE_NAME;
use crate::client::ModelClientSession;
use crate::context::ContextualUserFragment;
use crate::context::OrchestratedExecutionFacts;
use crate::responses_metadata::CodexResponsesRequestKind;
use crate::tools::context::SharedTurnDiffTracker;
use codex_config::Constrained;
use codex_protocol::config_types::ModeKind;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::ContentItem;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::permissions::FileSystemAccessMode;
use codex_protocol::permissions::FileSystemSandboxKind;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::OrchestratedRoleUpdatedEvent;
use codex_utils_output_truncation::approx_bytes_for_tokens;
use tokio_util::sync::CancellationToken;
use tracing::info;

use super::TurnInput;
use super::session::Session;
use super::turn::run_sampling_request;
use super::turn_context::TurnContext;

const MAX_PLAN_EVIDENCE_PACKET_TOKENS: usize = 1_000;
// Prompts target 2 KiB packets. Keep a larger hard ceiling so small model-side
// budget misses do not throw away useful evidence or exhaust review retries.
const MAX_PACKET_BYTES: usize = 8_192;
const TRUNCATED_PACKET_SUFFIX: &str =
    "\n[packet truncated: phase output exceeded the 8192-byte hard limit]";
const TRUNCATED_PLAN_EVIDENCE_PACKET_SUFFIX: &str =
    "\n[packet truncated: plan evidence exceeded the 1000-token hard limit]";

#[derive(Clone, Copy, Eq, PartialEq)]
enum Phase {
    TaskContract,
    Explorer,
    WorkerPlan,
    PlanReview,
    PlanEvidence,
    WorkerExec,
    ResultReview,
}

pub(super) enum Outcome {
    Skipped,
    Completed,
    Stopped,
}

struct PhasePacket {
    text: String,
    truncated: bool,
    execution_facts: OrchestratedExecutionFacts,
}

struct TurnBudget {
    requests: usize,
    start_tokens: i64,
    max_requests: usize,
    max_tokens: u64,
    max_phase_steps: usize,
}

impl TurnBudget {
    fn claim_request(&mut self) -> CodexResult<()> {
        if self.requests >= self.max_requests {
            return Err(CodexErr::SessionBudgetExceeded);
        }
        self.requests += 1;
        Ok(())
    }

    async fn check_tokens(&self, sess: &Session) -> CodexResult<()> {
        let current_tokens = sess
            .total_token_usage()
            .await
            .map_or(self.start_tokens, |usage| usage.total_tokens);
        let used_tokens = current_tokens.saturating_sub(self.start_tokens) as u64;
        if used_tokens > self.max_tokens {
            Err(CodexErr::SessionBudgetExceeded)
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum WorkerStatus {
    Complete,
    Incomplete,
    Invalid,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum PhaseStatus {
    Direct,
    Approved,
    EvidenceNeeded,
    WorkerComplete,
    WorkerIncomplete,
}

impl Phase {
    fn name(self) -> &'static str {
        match self {
            Self::TaskContract => TASK_CONTRACT_ROLE_NAME,
            Self::Explorer => EXPLORER_ROLE_NAME,
            Self::WorkerPlan => WORKER_PLAN_ROLE_NAME,
            Self::PlanReview => PLAN_REVIEW_ROLE_NAME,
            Self::PlanEvidence => PLAN_EVIDENCE_ROLE_NAME,
            Self::WorkerExec => WORKER_ROLE_NAME,
            Self::ResultReview => RESULT_REVIEW_ROLE_NAME,
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        match name {
            TASK_CONTRACT_ROLE_NAME => Some(Self::TaskContract),
            EXPLORER_ROLE_NAME => Some(Self::Explorer),
            WORKER_PLAN_ROLE_NAME => Some(Self::WorkerPlan),
            PLAN_REVIEW_ROLE_NAME => Some(Self::PlanReview),
            PLAN_EVIDENCE_ROLE_NAME => Some(Self::PlanEvidence),
            WORKER_ROLE_NAME => Some(Self::WorkerExec),
            RESULT_REVIEW_ROLE_NAME => Some(Self::ResultReview),
            _ => None,
        }
    }

    fn model_override(self, turn_context: &TurnContext) -> Option<&str> {
        match self {
            Self::TaskContract | Self::PlanReview | Self::ResultReview => None,
            Self::Explorer | Self::PlanEvidence => turn_context
                .config
                .orchestrated_mode
                .explorer_model
                .as_deref(),
            Self::WorkerPlan | Self::WorkerExec => turn_context
                .config
                .orchestrated_mode
                .worker_model
                .as_deref(),
        }
    }

    fn reasoning_effort_override(self, turn_context: &TurnContext) -> Option<ReasoningEffort> {
        match self {
            Self::TaskContract | Self::PlanReview | Self::ResultReview => None,
            Self::Explorer | Self::PlanEvidence => turn_context
                .config
                .orchestrated_mode
                .explorer_reasoning_effort
                .clone(),
            Self::WorkerPlan | Self::WorkerExec => turn_context
                .config
                .orchestrated_mode
                .worker_reasoning_effort
                .clone(),
        }
    }

    fn prompt(self) -> &'static str {
        match self {
            Self::TaskContract => codex_prompts::ORCHESTRATED_TASK_CONTRACT,
            Self::Explorer => codex_prompts::ORCHESTRATED_EXPLORER,
            Self::WorkerPlan => codex_prompts::ORCHESTRATED_WORKER_PLAN,
            Self::PlanReview => codex_prompts::ORCHESTRATED_PLAN_REVIEW,
            Self::PlanEvidence => codex_prompts::ORCHESTRATED_PLAN_EVIDENCE,
            Self::WorkerExec => codex_prompts::ORCHESTRATED_WORKER,
            Self::ResultReview => codex_prompts::ORCHESTRATED_RESULT_REVIEW,
        }
    }
}

pub(super) async fn run_for_input(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    turn_extension_data: Arc<codex_extension_api::ExtensionData>,
    turn_diff_tracker: SharedTurnDiffTracker,
    input: &[TurnInput],
    client_session: &mut ModelClientSession,
    cancellation_token: CancellationToken,
) -> CodexResult<Outcome> {
    let starts_orchestrated_flow = input.iter().any(|input_item| match input_item {
        TurnInput::UserInput { content, .. } => !content.is_empty(),
        TurnInput::InterAgentCommunication(communication) => communication.trigger_turn,
        TurnInput::ResponseItem(_) => false,
    });
    if turn_context.collaboration_mode.mode != ModeKind::Orchestrated
        || !turn_context.config.orchestrated_mode.enabled
        || !starts_orchestrated_flow
    {
        return Ok(Outcome::Skipped);
    }
    if input
        .iter()
        .any(|input_item| matches!(input_item, TurnInput::UserInput { .. }))
    {
        turn_context
            .orchestrated_execution_ledger
            .lock()
            .await
            .invalidate();
    }
    turn_context
        .orchestrated_execution_approved
        .store(false, Ordering::Relaxed);

    let max_turn_seconds = turn_context.config.orchestrated_mode.max_turn_seconds;
    let phase_cancellation = cancellation_token.child_token();
    let phase_result = {
        let phase_future = run_phases(
            Arc::clone(&sess),
            Arc::clone(&turn_context),
            turn_extension_data,
            turn_diff_tracker,
            client_session,
            phase_cancellation.clone(),
        );
        tokio::pin!(phase_future);
        tokio::select! {
            result = &mut phase_future => Some(result),
            _ = tokio::time::sleep(Duration::from_secs(max_turn_seconds)) => {
                phase_cancellation.cancel();
                // Wait for run_phase to compact or conservatively retain its in-flight history.
                let _ = phase_future.await;
                None
            }
        }
    };
    match phase_result {
        Some(Ok(true)) => Ok(Outcome::Completed),
        Some(Ok(false)) => {
            block_orchestration(&sess, &turn_context, "review gate was not approved").await;
            Ok(Outcome::Completed)
        }
        None => {
            emit_role_update(&sess, &turn_context, None).await;
            block_orchestration(&sess, &turn_context, "internal phase time budget exhausted").await;
            Ok(Outcome::Completed)
        }
        Some(Err(CodexErr::SessionBudgetExceeded)) => {
            emit_role_update(&sess, &turn_context, None).await;
            block_orchestration(
                &sess,
                &turn_context,
                "internal request or token budget exhausted",
            )
            .await;
            Ok(Outcome::Completed)
        }
        Some(Err(err @ CodexErr::TurnAborted)) => {
            emit_role_update(&sess, &turn_context, None).await;
            Err(err)
        }
        Some(Err(err)) => {
            turn_context
                .orchestrated_execution_approved
                .store(false, Ordering::Relaxed);
            info!("Orchestrated internal phase error: {err:#}");
            emit_role_update(&sess, &turn_context, None).await;
            let error = err.to_codex_protocol_error();
            sess.emit_turn_error_lifecycle(turn_context.as_ref(), error.clone())
                .await;
            sess.track_turn_codex_error(turn_context.as_ref(), &err);
            let event = EventMsg::Error(err.to_error_event(/*message_prefix*/ None));
            sess.send_event(&turn_context, event).await;
            Ok(Outcome::Stopped)
        }
    }
}

async fn block_orchestration(sess: &Session, turn_context: &TurnContext, reason: &str) {
    turn_context
        .orchestrated_execution_approved
        .store(false, Ordering::Relaxed);
    let item = ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: format!(
                "orchestrator-gate: blocked\nreason: {reason}\nDo not claim completion. Report the blocker and verified partial progress."
            ),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    sess.record_conversation_items(turn_context, &[item]).await;
}

async fn run_phases(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    turn_extension_data: Arc<codex_extension_api::ExtensionData>,
    turn_diff_tracker: SharedTurnDiffTracker,
    client_session: &mut ModelClientSession,
    cancellation_token: CancellationToken,
) -> CodexResult<bool> {
    let config = &turn_context.config.orchestrated_mode;
    let start_tokens = sess
        .total_token_usage()
        .await
        .map_or(0, |usage| usage.total_tokens);
    let mut budget = TurnBudget {
        requests: 0,
        start_tokens,
        max_requests: config.max_turn_model_requests,
        max_tokens: config.max_turn_tokens,
        max_phase_steps: config.max_phase_steps,
    };
    let task_contract = run_phase(
        Arc::clone(&sess),
        Arc::clone(&turn_context),
        Arc::clone(&turn_extension_data),
        Arc::clone(&turn_diff_tracker),
        Phase::TaskContract,
        client_session,
        &mut budget,
        cancellation_token.child_token(),
    )
    .await?;
    if config.direct_enabled
        && !task_contract.truncated
        && phase_status(&task_contract.text, Phase::TaskContract) == Some(PhaseStatus::Direct)
    {
        turn_context
            .orchestrated_execution_approved
            .store(true, Ordering::Relaxed);
        let mut previous_retry_signature = None;
        let mut result_approved = false;
        for _ in 0..=config.max_work_revisions {
            let worker_packet = run_phase(
                Arc::clone(&sess),
                Arc::clone(&turn_context),
                Arc::clone(&turn_extension_data),
                Arc::clone(&turn_diff_tracker),
                Phase::WorkerExec,
                client_session,
                &mut budget,
                cancellation_token.child_token(),
            )
            .await?;
            let worker_status = worker_status(&worker_packet.text);
            if !worker_packet.truncated && worker_status == WorkerStatus::Complete {
                let review_packet = run_phase(
                    Arc::clone(&sess),
                    Arc::clone(&turn_context),
                    Arc::clone(&turn_extension_data),
                    Arc::clone(&turn_diff_tracker),
                    Phase::ResultReview,
                    client_session,
                    &mut budget,
                    cancellation_token.child_token(),
                )
                .await?;
                if !review_packet.truncated
                    && review_approved(&review_packet.text, RESULT_REVIEW_ROLE_NAME)
                {
                    result_approved = true;
                    break;
                }
            }
            if !worker_packet.truncated
                && worker_status == WorkerStatus::Incomplete
                && worker_packet
                    .text
                    .lines()
                    .any(|line| line.trim_start().starts_with("evidence-needed:"))
            {
                let evidence_packet = run_phase(
                    Arc::clone(&sess),
                    Arc::clone(&turn_context),
                    Arc::clone(&turn_extension_data),
                    Arc::clone(&turn_diff_tracker),
                    Phase::Explorer,
                    client_session,
                    &mut budget,
                    cancellation_token.child_token(),
                )
                .await?;
                if evidence_packet.truncated {
                    break;
                }
                previous_retry_signature = None;
                continue;
            }
            let retry_signature = worker_retry_signature(&worker_packet);
            if previous_retry_signature.as_ref() == Some(&retry_signature) {
                break;
            }
            previous_retry_signature = Some(retry_signature);
        }
        emit_role_update(&sess, &turn_context, None).await;
        return Ok(result_approved);
    }

    let _ = run_phase(
        Arc::clone(&sess),
        Arc::clone(&turn_context),
        Arc::clone(&turn_extension_data),
        Arc::clone(&turn_diff_tracker),
        Phase::Explorer,
        client_session,
        &mut budget,
        cancellation_token.child_token(),
    )
    .await?;

    let mut result_approved = false;
    for _ in 0..=config.max_plan_revisions {
        let worker_plan = run_phase(
            Arc::clone(&sess),
            Arc::clone(&turn_context),
            Arc::clone(&turn_extension_data),
            Arc::clone(&turn_diff_tracker),
            Phase::WorkerPlan,
            client_session,
            &mut budget,
            cancellation_token.child_token(),
        )
        .await?;
        let mut plan_evidence_truncated = false;
        let mut plan_evidence_gathered = false;
        let review_packet = loop {
            let review_packet = run_phase(
                Arc::clone(&sess),
                Arc::clone(&turn_context),
                Arc::clone(&turn_extension_data),
                Arc::clone(&turn_diff_tracker),
                Phase::PlanReview,
                client_session,
                &mut budget,
                cancellation_token.child_token(),
            )
            .await?;
            if review_packet.truncated
                || !review_requests_evidence(&review_packet.text)
                || plan_evidence_gathered
            {
                break review_packet;
            }
            let evidence_packet = run_phase(
                Arc::clone(&sess),
                Arc::clone(&turn_context),
                Arc::clone(&turn_extension_data),
                Arc::clone(&turn_diff_tracker),
                Phase::PlanEvidence,
                client_session,
                &mut budget,
                cancellation_token.child_token(),
            )
            .await?;
            plan_evidence_truncated |= evidence_packet.truncated;
            plan_evidence_gathered = true;
        };
        if !worker_plan.truncated
            && packet_has_valid_role_prefix(&worker_plan.text, Phase::WorkerPlan)
            && !plan_evidence_truncated
            && !review_packet.truncated
            && review_approved(&review_packet.text, PLAN_REVIEW_ROLE_NAME)
        {
            turn_context
                .orchestrated_execution_approved
                .store(true, Ordering::Relaxed);
            let mut previous_retry_signature = None;
            for _ in 0..=config.max_work_revisions {
                let worker_packet = run_phase(
                    Arc::clone(&sess),
                    Arc::clone(&turn_context),
                    Arc::clone(&turn_extension_data),
                    Arc::clone(&turn_diff_tracker),
                    Phase::WorkerExec,
                    client_session,
                    &mut budget,
                    cancellation_token.child_token(),
                )
                .await?;
                let worker_status = worker_status(&worker_packet.text);
                if worker_packet.truncated || worker_status == WorkerStatus::Invalid {
                    continue;
                }
                let review_packet = run_phase(
                    Arc::clone(&sess),
                    Arc::clone(&turn_context),
                    Arc::clone(&turn_extension_data),
                    Arc::clone(&turn_diff_tracker),
                    Phase::ResultReview,
                    client_session,
                    &mut budget,
                    cancellation_token.child_token(),
                )
                .await?;
                if worker_status == WorkerStatus::Complete
                    && !review_packet.truncated
                    && review_approved(&review_packet.text, RESULT_REVIEW_ROLE_NAME)
                {
                    result_approved = true;
                    break;
                }
                match correction_owner(&review_packet.text) {
                    Some(CorrectionOwner::Worker) => {
                        let retry_signature = retry_signature(&worker_packet, &review_packet);
                        if previous_retry_signature.as_ref() == Some(&retry_signature) {
                            break;
                        }
                        previous_retry_signature = Some(retry_signature);
                    }
                    Some(CorrectionOwner::Explorer) => {
                        let evidence_packet = run_phase(
                            Arc::clone(&sess),
                            Arc::clone(&turn_context),
                            Arc::clone(&turn_extension_data),
                            Arc::clone(&turn_diff_tracker),
                            Phase::Explorer,
                            client_session,
                            &mut budget,
                            cancellation_token.child_token(),
                        )
                        .await?;
                        if evidence_packet.truncated {
                            break;
                        }
                        previous_retry_signature = None;
                    }
                    Some(CorrectionOwner::Root | CorrectionOwner::User) | None => break,
                }
            }
            break;
        }
    }
    emit_role_update(&sess, &turn_context, None).await;

    Ok(result_approved)
}

fn retry_signature(worker_packet: &PhasePacket, review_packet: &PhasePacket) -> String {
    let worker_signature = worker_retry_signature(worker_packet);
    format!("{}\n{}", worker_signature, review_packet.text.trim(),)
}

fn worker_retry_signature(worker_packet: &PhasePacket) -> String {
    let facts = worker_packet.execution_facts.progress_signature();
    format!(
        "{}\n{}\n{}",
        worker_status(&worker_packet.text) as u8,
        worker_packet.text.trim(),
        facts
    )
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum CorrectionOwner {
    Worker,
    Explorer,
    Root,
    User,
}

fn correction_owner(packet: &str) -> Option<CorrectionOwner> {
    let mut lines = packet.lines();
    let status_line = lines.next()?;
    if parse_status_token(status_line, Phase::ResultReview) != Some("revise") {
        return None;
    }
    match lines.next()?.trim() {
        "owner: worker" => Some(CorrectionOwner::Worker),
        "owner: explorer" => Some(CorrectionOwner::Explorer),
        "owner: root" => Some(CorrectionOwner::Root),
        "owner: user" => Some(CorrectionOwner::User),
        _ => None,
    }
}

fn review_approved(packet: &str, role: &str) -> bool {
    let phase = match role {
        PLAN_REVIEW_ROLE_NAME => Phase::PlanReview,
        RESULT_REVIEW_ROLE_NAME => Phase::ResultReview,
        _ => return false,
    };
    phase_status(packet, phase) == Some(PhaseStatus::Approved)
}

fn review_requests_evidence(packet: &str) -> bool {
    phase_status(packet, Phase::PlanReview) == Some(PhaseStatus::EvidenceNeeded)
}

fn worker_status(packet: &str) -> WorkerStatus {
    let packet = packet
        .strip_prefix("orc:")
        .map(str::trim_start)
        .unwrap_or(packet);
    match phase_status(packet, Phase::WorkerExec) {
        Some(PhaseStatus::WorkerComplete) => WorkerStatus::Complete,
        Some(PhaseStatus::WorkerIncomplete) => WorkerStatus::Incomplete,
        _ => WorkerStatus::Invalid,
    }
}

fn phase_status(packet: &str, phase: Phase) -> Option<PhaseStatus> {
    match (phase, parse_status_token(packet.lines().next()?, phase)?) {
        (Phase::TaskContract, "direct") => Some(PhaseStatus::Direct),
        (Phase::PlanReview | Phase::ResultReview, "approved") => Some(PhaseStatus::Approved),
        (Phase::PlanReview, "evidence-needed") => Some(PhaseStatus::EvidenceNeeded),
        (Phase::WorkerExec, "complete") => Some(PhaseStatus::WorkerComplete),
        (Phase::WorkerExec, "incomplete") => Some(PhaseStatus::WorkerIncomplete),
        _ => None,
    }
}

fn packet_has_valid_role_prefix(packet: &str, phase: Phase) -> bool {
    packet
        .lines()
        .next()
        .and_then(|line| line.strip_prefix(phase.name()))
        .and_then(|line| line.strip_prefix(':'))
        .is_some_and(|remainder| !remainder.trim().is_empty())
}

fn parse_status_token(line: &str, phase: Phase) -> Option<&str> {
    let line = if phase == Phase::WorkerExec {
        line.strip_prefix("orc:")
            .map(str::trim_start)
            .unwrap_or(line)
    } else {
        line
    };
    let remainder = line
        .strip_prefix(phase.name())?
        .strip_prefix(':')?
        .trim_start();
    let end = remainder
        .find(|character: char| character.is_whitespace() || matches!(character, ';' | ':'))
        .unwrap_or(remainder.len());
    let token = &remainder[..end];
    (!token.is_empty()).then_some(token)
}

pub(super) fn add_sampling_instruction(turn_context: &TurnContext, input: &mut Vec<ResponseItem>) {
    if let Some(phase) = turn_context.orchestrated_role.and_then(Phase::from_name) {
        input.push(developer_instruction_item(phase.prompt()));
        if phase == Phase::TaskContract && !turn_context.config.orchestrated_mode.direct_enabled {
            input.push(developer_instruction_item(
                "Direct routing is disabled. Emit a normal `task-contract:` packet and never emit `task-contract: direct`.",
            ));
        }
        return;
    }
    if turn_context.collaboration_mode.mode == ModeKind::Orchestrated {
        input.push(developer_instruction_item(
            codex_prompts::ORCHESTRATED_ORCHESTRATOR,
        ));
    }
}

fn developer_instruction_item(text: &str) -> ResponseItem {
    match crate::context_manager::updates::build_developer_update_item(vec![text.to_string()]) {
        Some(item) => item,
        None => ResponseItem::Message {
            id: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText {
                text: text.to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_phase(
    sess: Arc<Session>,
    root_turn_context: Arc<TurnContext>,
    turn_extension_data: Arc<codex_extension_api::ExtensionData>,
    turn_diff_tracker: SharedTurnDiffTracker,
    phase: Phase,
    client_session: &mut ModelClientSession,
    budget: &mut TurnBudget,
    cancellation_token: CancellationToken,
) -> CodexResult<PhasePacket> {
    let history_baseline = sess.clone_history().await.into_raw_items();
    emit_role_update(&sess, &root_turn_context, Some(phase.name())).await;
    let mut role_turn_context = root_turn_context
        .with_model(
            phase
                .model_override(&root_turn_context)
                .unwrap_or(root_turn_context.model_info.slug.as_str())
                .to_string(),
            &sess.services.models_manager,
        )
        .await;
    role_turn_context.orchestrated_role = Some(phase.name());
    role_turn_context.final_output_json_schema = None;
    if matches!(phase, Phase::Explorer | Phase::PlanEvidence) {
        role_turn_context.approval_policy = Constrained::allow_only(AskForApproval::Never);
        role_turn_context.permission_profile =
            explorer_permission_profile(&root_turn_context.permission_profile);
    }
    if let Some(reasoning_effort) = phase.reasoning_effort_override(&root_turn_context) {
        role_turn_context.reasoning_effort = Some(reasoning_effort);
        role_turn_context.collaboration_mode = role_turn_context.collaboration_mode.with_updates(
            /*model*/ None,
            Some(role_turn_context.reasoning_effort.clone()),
            /*developer_instructions*/ None,
        );
    }
    let role_turn_context = Arc::new(role_turn_context);
    let mut phase_result = Ok(());
    let mut phase_complete = false;
    for _ in 0..budget.max_phase_steps {
        if let Err(err) = budget.claim_request() {
            phase_result = Err(err);
            break;
        }
        let step_context = sess
            .capture_step_context(Arc::clone(&role_turn_context))
            .await;
        let prompt_input = sess
            .clone_history()
            .await
            .for_prompt(&role_turn_context.model_info.input_modalities);
        let window_id = sess.current_window_id().await;
        let responses_metadata = role_turn_context.turn_metadata_state.to_responses_metadata(
            sess.installation_id.clone(),
            window_id,
            CodexResponsesRequestKind::Turn,
        );
        let sampling_result = run_sampling_request(
            Arc::clone(&sess),
            step_context,
            Arc::clone(&turn_extension_data),
            Arc::clone(&turn_diff_tracker),
            client_session,
            &responses_metadata,
            prompt_input,
            cancellation_token.child_token(),
        )
        .await;
        if let Err(err) = budget.check_tokens(sess.as_ref()).await {
            phase_result = Err(err);
            break;
        }
        match sampling_result {
            Ok((sampling_result, _)) if sampling_result.needs_follow_up => {}
            Ok(_) => {
                phase_complete = true;
                break;
            }
            Err(err) => {
                phase_result = Err(err);
                break;
            }
        }
    }
    if !phase_complete && phase_result.is_ok() {
        phase_result = Err(CodexErr::SessionBudgetExceeded);
    }

    let packet = compact_phase_history(
        sess.as_ref(),
        root_turn_context.as_ref(),
        history_baseline,
        phase,
    )
    .await;
    phase_result?;
    Ok(packet)
}

fn explorer_permission_profile(parent: &PermissionProfile) -> PermissionProfile {
    let mut file_system = parent.file_system_sandbox_policy();
    match file_system.kind {
        FileSystemSandboxKind::Restricted => {
            for entry in &mut file_system.entries {
                if entry.access == FileSystemAccessMode::Write {
                    entry.access = FileSystemAccessMode::Read;
                }
            }
            PermissionProfile::from_runtime_permissions(
                &file_system,
                NetworkSandboxPolicy::Restricted,
            )
        }
        FileSystemSandboxKind::Unrestricted => PermissionProfile::read_only(),
        FileSystemSandboxKind::ExternalSandbox => PermissionProfile::External {
            network: NetworkSandboxPolicy::Restricted,
        },
    }
}

async fn emit_role_update(sess: &Session, turn_context: &TurnContext, role: Option<&str>) {
    sess.send_event(
        turn_context,
        EventMsg::OrchestratedRoleUpdated(OrchestratedRoleUpdatedEvent {
            turn_id: turn_context.sub_id.clone(),
            role: role.map(str::to_string),
        }),
    )
    .await;
}

async fn compact_phase_history(
    sess: &Session,
    turn_context: &TurnContext,
    baseline: Vec<ResponseItem>,
    phase: Phase,
) -> PhasePacket {
    let after_items = sess.clone_history().await.into_raw_items();
    let phase_items = after_items.get(baseline.len()..).unwrap_or_default();
    let full_packet = phase_packet(phase, phase_items);
    let packet = truncate_packet(&full_packet, phase);
    let packet_item = ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: packet.text.clone(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let mut execution_facts_update = None;
    let execution_facts = if phase == Phase::WorkerExec {
        let mut ledger = turn_context.orchestrated_execution_ledger.lock().await;
        let facts = ledger.facts();
        execution_facts_update = ledger.take_update();
        facts
    } else {
        OrchestratedExecutionFacts::default()
    };
    let mut retained_items = vec![packet_item];
    if let Some(execution_facts_update) = execution_facts_update {
        retained_items.push(ContextualUserFragment::into(execution_facts_update));
    }
    sess.replace_orchestrated_phase_history(turn_context, baseline, after_items, retained_items)
        .await;
    PhasePacket {
        execution_facts,
        ..packet
    }
}

fn phase_packet(phase: Phase, phase_items: &[ResponseItem]) -> String {
    let phase_prefix = format!("{}:", phase.name());
    let mut latest_assistant_message = None;
    let packet = phase_items
        .iter()
        .rev()
        .filter_map(assistant_message_text)
        .find_map(|text| {
            if latest_assistant_message.is_none() {
                latest_assistant_message = Some(text.clone());
            }
            text.trim_start().starts_with(&phase_prefix).then_some(text)
        })
        .or(latest_assistant_message)
        .unwrap_or_else(|| format!("{}: no final packet produced", phase.name()));
    packet.trim().to_string()
}

fn assistant_message_text(item: &ResponseItem) -> Option<String> {
    let ResponseItem::Message { role, content, .. } = item else {
        return None;
    };
    if role != "assistant" {
        return None;
    }
    let text = content
        .iter()
        .filter_map(|content| match content {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                Some(text.as_str())
            }
            _ => None,
        })
        .collect::<String>();
    (!text.is_empty()).then_some(text)
}

fn truncate_packet(text: &str, phase: Phase) -> PhasePacket {
    let (max_bytes, suffix) = match phase {
        Phase::PlanEvidence => (
            approx_bytes_for_tokens(MAX_PLAN_EVIDENCE_PACKET_TOKENS),
            TRUNCATED_PLAN_EVIDENCE_PACKET_SUFFIX,
        ),
        _ => (MAX_PACKET_BYTES, TRUNCATED_PACKET_SUFFIX),
    };
    if text.len() <= max_bytes {
        return PhasePacket {
            text: text.to_string(),
            truncated: false,
            execution_facts: OrchestratedExecutionFacts::default(),
        };
    }
    let mut end = max_bytes.saturating_sub(suffix.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    PhasePacket {
        text: format!("{}{}", &text[..end], suffix),
        truncated: true,
        execution_facts: OrchestratedExecutionFacts::default(),
    }
}

#[cfg(test)]
#[path = "orchestrated_tests.rs"]
mod tests;
