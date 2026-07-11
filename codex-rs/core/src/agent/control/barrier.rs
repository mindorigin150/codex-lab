use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Mutex;

use codex_protocol::ThreadId;

use crate::agent::AgentStatus;
use crate::agent::control::AgentControl;

#[derive(Default)]
pub(super) struct CompletionReceipts(Mutex<HashMap<ThreadId, CompletionProgress>>);

#[derive(Default)]
struct CompletionProgress {
    next_generation: u64,
    current_generation: u64,
    pending_generations: VecDeque<u64>,
    delivered: Option<(u64, AgentStatus)>,
}

#[derive(Default)]
pub(super) struct BlockingBarriers(Mutex<HashMap<ThreadId, BlockingBarrierState>>);

#[derive(Default)]
struct BlockingBarrierState {
    targets: HashSet<ThreadId>,
    failure_pending: bool,
    retry_used: bool,
}

impl AgentControl {
    pub(super) fn reserve_agent_generation(&self, agent_id: ThreadId) -> u64 {
        let mut receipts = self
            .completion_receipts
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let progress = receipts.entry(agent_id).or_default();
        progress.next_generation = progress.next_generation.saturating_add(1);
        let generation = progress.next_generation;
        progress.current_generation = generation;
        progress.pending_generations.push_back(generation);
        generation
    }

    pub(super) fn cancel_agent_generation(&self, agent_id: ThreadId, generation: u64) {
        let mut receipts = self
            .completion_receipts
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(progress) = receipts.get_mut(&agent_id) else {
            return;
        };
        progress
            .pending_generations
            .retain(|pending| *pending != generation);
        if progress.current_generation == generation {
            progress.current_generation = progress
                .pending_generations
                .back()
                .copied()
                .or_else(|| progress.delivered.as_ref().map(|(delivered, _)| *delivered))
                .unwrap_or_default();
        }
    }

    pub(crate) fn record_completion_delivery(&self, agent_id: ThreadId, status: AgentStatus) {
        let mut receipts = self
            .completion_receipts
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let progress = receipts.entry(agent_id).or_default();
        let Some(generation) = progress.pending_generations.pop_front() else {
            tracing::warn!(%agent_id, "completion delivered without a pending agent generation");
            return;
        };
        progress.delivered = Some((generation, status));
    }

    pub(crate) fn current_completion_receipt(&self, agent_id: ThreadId) -> Option<AgentStatus> {
        let receipts = self
            .completion_receipts
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let progress = receipts.get(&agent_id)?;
        progress
            .delivered
            .as_ref()
            .filter(|(delivered, _)| *delivered == progress.current_generation)
            .map(|(_, status)| status.clone())
    }

    pub(crate) fn register_blocking_agent(&self, parent_thread_id: ThreadId, agent_id: ThreadId) {
        let mut barriers = self
            .blocking_barriers
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let barrier = barriers.entry(parent_thread_id).or_default();
        barrier.targets.insert(agent_id);
    }

    pub(crate) fn begin_blocking_agent_start(
        &self,
        parent_thread_id: ThreadId,
    ) -> Result<bool, String> {
        let mut barriers = self
            .blocking_barriers
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(barrier) = barriers.get_mut(&parent_thread_id) else {
            return Ok(false);
        };
        if !barrier.failure_pending {
            return Ok(false);
        }
        if barrier.retry_used {
            return Err(
                "Blocking agent retry already failed; ask the user how to proceed before retrying again."
                    .to_string(),
            );
        }
        barrier.failure_pending = false;
        barrier.retry_used = true;
        Ok(true)
    }

    pub(crate) fn cancel_blocking_agent_start(&self, parent_thread_id: ThreadId, was_retry: bool) {
        if !was_retry {
            return;
        }
        let mut barriers = self
            .blocking_barriers
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let barrier = barriers.entry(parent_thread_id).or_default();
        barrier.failure_pending = true;
        barrier.retry_used = false;
    }

    pub(crate) fn blocking_agent_targets(
        &self,
        parent_thread_id: ThreadId,
    ) -> Vec<(ThreadId, String)> {
        let barriers = self
            .blocking_barriers
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(barrier) = barriers.get(&parent_thread_id) else {
            return Vec::new();
        };
        let mut targets = barrier
            .targets
            .iter()
            .map(|thread_id| {
                let target = self
                    .state
                    .agent_metadata_for_thread(*thread_id)
                    .and_then(|metadata| metadata.agent_path)
                    .map(String::from)
                    .unwrap_or_else(|| thread_id.to_string());
                (*thread_id, target)
            })
            .collect::<Vec<_>>();
        targets.sort_by(|left, right| left.1.cmp(&right.1));
        targets
    }

    pub(crate) fn settle_blocking_agents(
        &self,
        parent_thread_id: ThreadId,
        agent_ids: &[ThreadId],
        failed: bool,
    ) {
        let mut barriers = self
            .blocking_barriers
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(barrier) = barriers.get_mut(&parent_thread_id) else {
            return;
        };
        for agent_id in agent_ids {
            barrier.targets.remove(agent_id);
        }
        barrier.failure_pending |= failed;
        if barrier.targets.is_empty() && !barrier.failure_pending {
            barriers.remove(&parent_thread_id);
        }
    }

    pub(crate) fn barrier_failure_pending(&self, parent_thread_id: ThreadId) -> bool {
        self.blocking_barriers
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&parent_thread_id)
            .is_some_and(|barrier| barrier.failure_pending)
    }

    pub(crate) fn acknowledge_barrier_failure(&self, parent_thread_id: ThreadId) {
        let mut barriers = self
            .blocking_barriers
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(barrier) = barriers.get_mut(&parent_thread_id) else {
            return;
        };
        barrier.failure_pending = false;
        barrier.retry_used = false;
        if barrier.targets.is_empty() {
            barriers.remove(&parent_thread_id);
        }
    }
}
