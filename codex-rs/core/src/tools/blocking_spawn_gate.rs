use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use tokio::sync::Notify;

use crate::tools::context::ToolPayload;
use crate::tools::router::ToolCall;

#[derive(Default)]
pub(super) struct BlockingSpawnGate {
    seen: AtomicBool,
    pending: AtomicUsize,
    succeeded: AtomicBool,
    settled: Notify,
}

impl BlockingSpawnGate {
    pub(super) fn register(self: &Arc<Self>) -> BlockingSpawnRegistration {
        self.seen.store(true, Ordering::Release);
        self.pending.fetch_add(1, Ordering::AcqRel);
        BlockingSpawnRegistration {
            gate: Arc::clone(self),
            finished: false,
        }
    }

    pub(super) async fn successful_spawn_settled(&self) -> bool {
        if !self.seen.load(Ordering::Acquire) {
            return false;
        }
        loop {
            let notified = self.settled.notified();
            if self.pending.load(Ordering::Acquire) == 0 {
                return self.succeeded.load(Ordering::Acquire);
            }
            notified.await;
        }
    }
}

pub(super) struct BlockingSpawnRegistration {
    gate: Arc<BlockingSpawnGate>,
    finished: bool,
}

impl BlockingSpawnRegistration {
    pub(super) fn finish(&mut self, succeeded: bool) {
        if succeeded {
            self.gate.succeeded.store(true, Ordering::Release);
        }
        self.finished = true;
        self.gate.pending.fetch_sub(1, Ordering::AcqRel);
        self.gate.settled.notify_waiters();
    }
}

impl Drop for BlockingSpawnRegistration {
    fn drop(&mut self) {
        if !self.finished {
            self.gate.pending.fetch_sub(1, Ordering::AcqRel);
            self.gate.settled.notify_waiters();
        }
    }
}

pub(super) fn is_collaboration_call(call: &ToolCall, namespace: Option<&str>) -> bool {
    call.tool_name.namespace.as_deref() == namespace
        && matches!(
            call.tool_name.name.as_str(),
            "spawn_agent"
                | "spawn_agents"
                | "send_message"
                | "followup_task"
                | "wait_agent"
                | "interrupt_agent"
                | "list_agents"
        )
}

pub(super) fn is_blocking_spawn(call: &ToolCall, namespace: Option<&str>) -> bool {
    if !is_collaboration_call(call, namespace)
        || !matches!(call.tool_name.name.as_str(), "spawn_agent" | "spawn_agents")
    {
        return false;
    }
    let ToolPayload::Function { arguments } = &call.payload else {
        return false;
    };
    let Ok(arguments) = serde_json::from_str::<serde_json::Value>(arguments) else {
        return false;
    };
    if call.tool_name.name == "spawn_agent" {
        return arguments
            .get("agent_type")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|role| matches!(role.trim(), "explorer" | "reviewer"));
    }
    arguments
        .get("tasks")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|tasks| {
            tasks.iter().any(|task| {
                task.get("agent_type")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|role| matches!(role.trim(), "explorer" | "reviewer"))
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn successful_spawn_settled_waits_for_all_registered_calls() {
        use std::time::Duration;

        let gate = Arc::new(BlockingSpawnGate::default());
        let mut first = gate.register();
        let mut second = gate.register();
        first.finish(true);
        assert!(
            tokio::time::timeout(Duration::from_millis(10), gate.successful_spawn_settled())
                .await
                .is_err()
        );
        second.finish(false);
        assert!(gate.successful_spawn_settled().await);
    }
}
