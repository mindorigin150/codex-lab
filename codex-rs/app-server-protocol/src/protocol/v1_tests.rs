use super::*;
use codex_protocol::protocol::AgentRoleProvenance;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn conversation_session_source_omits_role_provenance() {
    let parent_thread_id = ThreadId::new();
    let source = CoreSessionSource::SubAgent(CoreSubAgentSource::ThreadSpawn {
        parent_thread_id,
        depth: 1,
        agent_path: None,
        agent_nickname: None,
        agent_role: Some("explorer".to_string()),
        agent_role_provenance: Some(AgentRoleProvenance::BuiltIn),
    });

    assert_eq!(
        serde_json::to_value(SessionSource::from(source)).expect("serialize v1 source"),
        json!({
            "subagent": {
                "thread_spawn": {
                    "parent_thread_id": parent_thread_id,
                    "depth": 1,
                    "agent_path": null,
                    "agent_nickname": null,
                    "agent_role": "explorer"
                }
            }
        })
    );
}
