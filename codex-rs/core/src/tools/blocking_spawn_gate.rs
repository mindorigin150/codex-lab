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
                | "send_message"
                | "followup_task"
                | "wait_agent"
                | "interrupt_agent"
                | "list_agents"
        )
}

pub(super) fn is_blocking_spawn(call: &ToolCall, namespace: Option<&str>) -> bool {
    if !is_collaboration_call(call, namespace) || call.tool_name.name != "spawn_agent" {
        return false;
    }
    let ToolPayload::Function { arguments } = &call.payload else {
        return false;
    };
    serde_json::from_str::<serde_json::Value>(arguments)
        .ok()
        .and_then(|arguments| arguments.get("agent_type")?.as_str().map(str::to_owned))
        .is_some_and(|role| matches!(role.trim(), "explorer" | "reviewer"))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn blocking_spawn_detection_is_role_specific() {
        let spawn = |agent_type: &str| ToolCall {
            tool_name: codex_tools::ToolName::plain("spawn_agent"),
            call_id: "call-spawn".to_string(),
            payload: ToolPayload::Function {
                arguments: serde_json::json!({ "agent_type": agent_type }).to_string(),
            },
        };

        assert!(is_blocking_spawn(&spawn("explorer"), None));
        assert!(is_blocking_spawn(&spawn("reviewer"), None));
        assert!(!is_blocking_spawn(&spawn("worker"), None));
        assert!(is_collaboration_call(&spawn("explorer"), None));
        assert!(!is_collaboration_call(&spawn("explorer"), Some("agents")));
        assert!(!is_collaboration_call(
            &ToolCall {
                tool_name: codex_tools::ToolName::plain("exec_command"),
                call_id: "call-exec".to_string(),
                payload: ToolPayload::Function {
                    arguments: "{}".to_string(),
                },
            },
            None
        ));
    }

    #[tokio::test]
    async fn blocks_only_after_a_successful_spawn() {
        let failed_gate = Arc::new(BlockingSpawnGate::default());
        let mut failed = failed_gate.register();
        failed.finish(false);
        assert!(!failed_gate.successful_spawn_settled().await);

        let successful_gate = Arc::new(BlockingSpawnGate::default());
        let mut successful = successful_gate.register();
        successful.finish(true);
        assert!(successful_gate.successful_spawn_settled().await);
    }

    #[tokio::test]
    async fn waits_for_every_registered_spawn() {
        let gate = Arc::new(BlockingSpawnGate::default());
        let mut first = gate.register();
        let mut second = gate.register();
        first.finish(true);

        let wait = gate.successful_spawn_settled();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), wait)
                .await
                .is_err()
        );

        second.finish(false);
        assert!(gate.successful_spawn_settled().await);
    }
}
