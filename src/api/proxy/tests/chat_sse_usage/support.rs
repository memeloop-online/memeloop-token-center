use super::*;

pub(super) fn chat_chunk(id: &str, choices: Value, usage: Option<Value>) -> String {
    chat_chunk_with_metadata(id, choices, usage, None)
}

pub(super) fn chat_chunk_with_metadata(
    id: &str,
    choices: Value,
    usage: Option<Value>,
    service_tier: Option<&str>,
) -> String {
    let mut value = json!({
        "id": id,
        "object": "chat.completion.chunk",
        "model": "compatible-chat-model",
        "choices": choices,
    });
    if let Some(usage) = usage {
        value["usage"] = usage;
    }
    if let Some(service_tier) = service_tier {
        value["service_tier"] = json!(service_tier);
    }
    format!("data: {value}\n\n")
}

pub(super) fn chat_content(id: &str) -> String {
    chat_chunk(
        id,
        json!([{
            "index": 0,
            "delta": {"content": "ok"},
            "finish_reason": null,
        }]),
        None,
    )
}

pub(super) fn chat_finish(id: &str) -> String {
    chat_chunk(
        id,
        json!([{
            "index": 0,
            "delta": {},
            "finish_reason": "stop",
        }]),
        None,
    )
}

pub(super) fn chat_usage_only(id: &str, usage: Value) -> String {
    chat_chunk(id, json!([]), Some(usage))
}

pub(super) fn chat_usage_only_with_service_tier(
    id: &str,
    usage: Value,
    service_tier: &str,
) -> String {
    format!(
        "data: {}\n\n",
        json!({
            "id": id,
            "object": "chat.completion.chunk",
            "model": "compatible-chat-model",
            "choices": [],
            "usage": usage,
            "service_tier": service_tier,
            "obfuscation": null,
            "moderation": null,
        })
    )
}

pub(super) fn chat_role_only(id: &str) -> String {
    chat_chunk(
        id,
        json!([{
            "index": 0,
            "delta": {"role": "assistant", "content": null},
            "finish_reason": null,
        }]),
        None,
    )
}

pub(super) fn chat_terminal_noop(id: &str) -> String {
    chat_chunk(
        id,
        json!([{
            "index": 0,
            "delta": {"role": "assistant", "content": null},
            "finish_reason": "stop",
        }]),
        None,
    )
}

pub(super) fn chat_moderation_only(id: &str) -> String {
    format!(
        "data: {}\n\n",
        json!({
            "id": id,
            "object": "chat.completion.chunk",
            "model": "compatible-chat-model",
            "choices": [],
            "usage": null,
            "moderation": {"flagged": false},
        })
    )
}

pub(super) fn done() -> &'static str {
    "data: [DONE]\n\n"
}

pub(super) fn usage(prompt_tokens: i64, completion_tokens: i64, total_tokens: i64) -> Value {
    json!({
        "prompt_tokens": prompt_tokens,
        "completion_tokens": completion_tokens,
        "total_tokens": total_tokens,
        "prompt_tokens_details": {"cached_tokens": 11},
    })
}

pub(super) fn chat_request(model: &str) -> Value {
    json!({
        "model": model,
        "messages": [{"role": "user", "content": "confirm"}],
        "stream": true,
        "stream_options": {"include_usage": true},
        "max_tokens": 16,
    })
}

pub(super) async fn fragmented_sse_upstream(
    chunks: Vec<Vec<u8>>,
) -> (String, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uri = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0_u8; 4096];
        assert!(stream.read(&mut request).await.unwrap() > 0);
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        for chunk in chunks {
            stream
                .write_all(format!("{:X}\r\n", chunk.len()).as_bytes())
                .await
                .unwrap();
            stream.write_all(&chunk).await.unwrap();
            stream.write_all(b"\r\n").await.unwrap();
            stream.flush().await.unwrap();
            tokio::task::yield_now().await;
        }
        stream.write_all(b"0\r\n\r\n").await.unwrap();
    });
    (uri, server)
}
