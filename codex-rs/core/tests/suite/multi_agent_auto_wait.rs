use anyhow::Result;
use codex_features::Feature;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_response_once_match;
use core_test_support::responses::mount_sse_once_match;
use core_test_support::responses::sse;
use core_test_support::responses::sse_response;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

const PARENT_PROMPT: &str = "delegate this investigation to an explorer";
const CHILD_ANSWER: &str = "the explorer found the answer";
const SPAWN_CALL_ID: &str = "spawn-explorer";
const MULTI_AGENT_V2_NAMESPACE: &str = "agents";

fn body_contains(req: &wiremock::Request, text: &str) -> bool {
    decoded_body(req)
        .unwrap_or_default()
        .as_slice()
        .windows(text.len())
        .any(|window| window == text.as_bytes())
}

fn request_has_input_type(req: &wiremock::Request, ty: &str) -> bool {
    decoded_body(req)
        .and_then(|body| serde_json::from_slice::<Value>(&body).ok())
        .and_then(|body| body.get("input").and_then(Value::as_array).cloned())
        .is_some_and(|items| {
            items
                .iter()
                .any(|item| item.get("type").and_then(Value::as_str) == Some(ty))
        })
}

fn is_subagent_request(req: &wiremock::Request) -> bool {
    req.headers
        .get("x-openai-subagent")
        .and_then(|value| value.to_str().ok())
        == Some("collab_spawn")
}

fn decoded_body(req: &wiremock::Request) -> Option<Vec<u8>> {
    let is_zstd = req
        .headers
        .get("content-encoding")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.split(',').any(|entry| entry.trim() == "zstd"));
    if is_zstd {
        zstd::stream::decode_all(std::io::Cursor::new(&req.body)).ok()
    } else {
        Some(req.body.clone())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parent_automatically_waits_for_explorer_final_answer_before_sampling() -> Result<()> {
    let server = start_mock_server().await;
    let spawn_args = serde_json::to_string(&json!({
        "message": "investigate independently and report back",
        "task_name": "explorer_task",
        "agent_type": "explorer",
    }))?;

    mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, PARENT_PROMPT),
        sse(vec![
            ev_response_created("resp-parent-spawn"),
            ev_function_call_with_namespace(
                SPAWN_CALL_ID,
                MULTI_AGENT_V2_NAMESPACE,
                "spawn_agent",
                &spawn_args,
            ),
            ev_completed("resp-parent-spawn"),
        ]),
    )
    .await;

    // Keep the child running across several barrier deadlines. A timeout must
    // refresh the wait instead of authorizing an eager parent follow-up.
    let child = mount_response_once_match(
        &server,
        |req: &wiremock::Request| {
            is_subagent_request(req) && request_has_input_type(req, "agent_message")
        },
        sse_response(sse(vec![
            ev_response_created("resp-child"),
            ev_assistant_message("msg-child", CHILD_ANSWER),
            ev_completed("resp-child"),
        ]))
        .set_delay(Duration::from_millis(250)),
    )
    .await;

    let parent_after_child = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| {
            body_contains(req, SPAWN_CALL_ID)
                && body_contains(req, "Message Type: FINAL_ANSWER")
                && body_contains(req, CHILD_ANSWER)
        },
        sse(vec![
            ev_response_created("resp-parent-final"),
            ev_assistant_message("msg-parent-final", "parent incorporated explorer result"),
            ev_completed("resp-parent-final"),
        ]),
    )
    .await;

    let test = test_codex()
        .with_model("koffing")
        .with_config(|config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("test config should allow feature update");
            config
                .features
                .enable(Feature::MultiAgentV2)
                .expect("test config should allow feature update");
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
            config.model_provider.supports_websockets = false;
            config.multi_agent_v2.min_wait_timeout_ms = 10;
            config.multi_agent_v2.default_wait_timeout_ms = 50;
            config.multi_agent_v2.max_wait_timeout_ms = 1_000;
        })
        .build(&server)
        .await?;

    match tokio::time::timeout(Duration::from_secs(10), test.submit_turn(PARENT_PROMPT)).await {
        Ok(result) => result?,
        Err(_) => anyhow::bail!(
            "turn timed out; child requests={}, parent-after-child requests={}",
            child.requests().len(),
            parent_after_child.requests().len(),
        ),
    }

    assert_eq!(child.requests().len(), 1, "child should sample once");
    assert!(
        !parent_after_child.requests().is_empty(),
        "parent should sample after receiving the child's mailbox delivery"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_input_temporarily_releases_the_explorer_barrier_without_cancelling_child()
-> Result<()> {
    const INITIAL_PROMPT: &str = "delegate this investigation and wait";
    const STEER_PROMPT: &str = "answer this new instruction now";
    const CALL_ID: &str = "spawn-steered-explorer";

    let server = start_mock_server().await;
    let spawn_args = serde_json::to_string(&json!({
        "message": "keep investigating until the delayed response arrives",
        "task_name": "steered_explorer",
        "agent_type": "explorer",
    }))?;
    mount_sse_once_match(
        &server,
        |req: &wiremock::Request| {
            body_contains(req, INITIAL_PROMPT) && !body_contains(req, CALL_ID)
        },
        sse(vec![
            ev_response_created("resp-steer-spawn"),
            ev_function_call_with_namespace(
                CALL_ID,
                MULTI_AGENT_V2_NAMESPACE,
                "spawn_agent",
                &spawn_args,
            ),
            ev_completed("resp-steer-spawn"),
        ]),
    )
    .await;
    let child = mount_response_once_match(
        &server,
        |req: &wiremock::Request| {
            is_subagent_request(req) && request_has_input_type(req, "agent_message")
        },
        sse_response(sse(vec![
            ev_response_created("resp-steered-child"),
            ev_assistant_message("msg-steered-child", "delayed child answer"),
            ev_completed("resp-steered-child"),
        ]))
        .set_delay(Duration::from_secs(10)),
    )
    .await;
    let parent_after_steer = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, STEER_PROMPT),
        sse(vec![
            ev_response_created("resp-parent-after-steer"),
            ev_assistant_message("msg-parent-after-steer", "handled steered input"),
            ev_completed("resp-parent-after-steer"),
        ]),
    )
    .await;

    let test = Arc::new(
        test_codex()
            .with_model("koffing")
            .with_config(|config| {
                config
                    .features
                    .enable(Feature::Collab)
                    .expect("test config should allow feature update");
                config
                    .features
                    .enable(Feature::MultiAgentV2)
                    .expect("test config should allow feature update");
                config.model_provider.request_max_retries = Some(0);
                config.model_provider.stream_max_retries = Some(0);
                config.model_provider.supports_websockets = false;
            })
            .build(&server)
            .await?,
    );
    let turn = tokio::spawn({
        let test = Arc::clone(&test);
        async move { test.submit_turn(INITIAL_PROMPT).await }
    });

    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::CollabWaitingBegin(_))
    })
    .await;
    test.codex
        .steer_input(
            vec![UserInput::Text {
                text: STEER_PROMPT.to_string(),
                text_elements: Vec::new(),
            }],
            Default::default(),
            /*expected_turn_id*/ None,
            /*client_user_message_id*/ None,
            /*responsesapi_client_metadata*/ None,
        )
        .await
        .map_err(|err| anyhow::anyhow!("failed to steer parent turn: {err:?}"))?;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if !parent_after_steer.requests().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("parent did not sample after steered input"))?;

    assert!(!child.requests().is_empty(), "child should have started");
    assert_eq!(
        parent_after_steer.requests().len(),
        1,
        "parent should handle the steered input before child completion"
    );
    test.codex.submit(Op::Interrupt).await?;
    let _ = turn.await;
    Ok(())
}
