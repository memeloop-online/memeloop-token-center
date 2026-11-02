use super::*;
use serde_json::json;

fn assert_stream_matches_buffered(request: &Value, stream: &str, buffered: &Value) -> SessionBody {
    let streamed_document = raw_return_document(stream.as_bytes()).expect("SSE document");
    let buffered_bytes = serde_json::to_vec(buffered).unwrap();
    let buffered_document = raw_return_document(&buffered_bytes).expect("buffered document");
    let streamed_body = session_body_from_documents(request, &streamed_document);
    let buffered_body = session_body_from_documents(request, &buffered_document);
    assert_eq!(streamed_body, buffered_body);
    assert_eq!(streamed_body.into_prefix(), buffered_body.into_prefix());
    streamed_body
}

#[test]
fn chat_sse_tool_round_matches_buffered_and_echoed_history() {
    let request = json!({"messages": [{"role": "user", "content": "查天气"}]});
    let message = json!({
        "role": "assistant", "content": "正在查询",
        "tool_calls": [
            {"id": "call_weather", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"北京\"}"}},
            {"id": "call_clock", "type": "function", "function": {"name": "clock", "arguments": "{}"}}
        ]
    });
    let stream = r#"
data: {"id":"chatcmpl-1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}

data: {"choices":[{"index":0,"delta":{"content":"正在"}},{"index":1,"delta":{"content":"other candidate"}}]}

data: {"choices":[{"index":0,"delta":{"content":"查询","tool_calls":[{"index":0,"id":"call_","type":"function","function":{"name":"get_","arguments":""}},{"index":1,"id":"call_clock","type":"function","function":{"name":"clock","arguments":"{"}}]}}]}

data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"function":{"arguments":"}"}},{"index":0,"id":"weather","function":{"name":"weather","arguments":"{\"city\":"}}]}}]}

data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"北京\"}"}}]}}]}

data: {"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}

data: {"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":12,"total_tokens":22}}

data: [DONE]

"#;
    let first = assert_stream_matches_buffered(
        &request,
        stream,
        &json!({"choices": [{"message": message}]}),
    );
    assert_eq!(first.turns.len(), 2);
    let parts = &first.turns[1].parts;
    assert_eq!(parts.len(), 3);
    assert_eq!(parts[0].text.as_deref(), Some("正在查询"));
    assert_eq!(parts[1].call_id.as_deref(), Some("call_weather"));
    assert_eq!(
        parts[1].text.as_deref(),
        Some("get_weather\n{\"city\":\"北京\"}")
    );
    assert_eq!(parts[2].call_id.as_deref(), Some("call_clock"));
    assert_eq!(parts[2].text.as_deref(), Some("clock\n{}"));
    let followup = json!({"messages": [
        request["messages"][0], message,
        {"role": "tool", "tool_call_id": "call_weather", "content": "晴"},
        {"role": "tool", "tool_call_id": "call_clock", "content": "12:00"}
    ]});
    let next = assert_stream_matches_buffered(
        &followup,
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"北京晴，12:00\"}}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
        &json!({"choices": [{"message": {"role": "assistant", "content": "北京晴，12:00"}}]}),
    );
    assert_eq!(
        next.turns
            .iter()
            .map(|turn| turn.role.as_str())
            .collect::<Vec<_>>(),
        ["user", "assistant", "tool", "tool", "assistant"]
    );
    assert_eq!(
        next.turns[2].parts[0].tool_call_id.as_deref(),
        Some("call_weather")
    );
    assert_eq!(
        next.turns[3].parts[0].tool_call_id.as_deref(),
        Some("call_clock")
    );
    assert_eq!(first.into_prefix().1, next.into_prefix().1[..2]);
}

#[test]
fn chat_full_message_survives_empty_deltas_and_usage() {
    let buffered = json!({"choices": [{"message": {
        "role": "assistant", "content": null,
        "tool_calls": [{"id": "call-1", "type": "function", "function": {"name": "lookup", "arguments": "{}"}}]
    }}]});
    let stream = r#"
data: {"choices":[{"index":0,"delta":{"role":"assistant","content":"draft"}}]}

data: {"choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call-1","type":"function","function":{"name":"lookup","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}

data: {"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}

data: {"choices":[],"usage":{"completion_tokens":1}}

data: [DONE]

"#;
    let body = assert_stream_matches_buffered(&json!({}), stream, &buffered);
    assert_eq!(body.turns.len(), 1);
    assert_eq!(body.turns[0].parts.len(), 1);
    assert_eq!(body.turns[0].parts[0].call_id.as_deref(), Some("call-1"));
}

#[test]
fn chat_tool_only_deltas_survive_finish_and_crlf_usage_trailer() {
    let stream = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":null}}]}\r\n\r\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-1\",\"type\":\"function\",\"function\":{\"name\":\"lookup\",\"arguments\":\"{\"}}]}}]}\r\n\r\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"}\"}}]}}]}\r\n\r\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\r\n\r\n",
        "data: {\"choices\":[],\"usage\":{\"completion_tokens\":1}}\r\n\r\n",
        "data: [DONE]\r\n\r\n",
    );
    let body = assert_stream_matches_buffered(
        &json!({}),
        stream,
        &json!({"choices": [{"message": {"role": "assistant", "content": null, "tool_calls": [
            {"id": "call-1", "type": "function", "function": {"name": "lookup", "arguments": "{}"}}
        ]}}]}),
    );
    assert_eq!(body.turns.len(), 1);
    assert_eq!(body.turns[0].parts.len(), 1);
    assert_eq!(body.turns[0].parts[0].kind, "tool_call");
    assert_eq!(body.turns[0].parts[0].text.as_deref(), Some("lookup\n{}"));
    assert_eq!(body.turns[0].parts[0].call_id.as_deref(), Some("call-1"));
}

#[test]
fn anthropic_sse_blocks_and_tool_results_match_buffered_round() {
    let request = json!({"messages": [{"role": "user", "content": "查天气"}]});
    let message = json!({"id": "msg-1", "type": "message", "role": "assistant", "content": [
        {"type": "text", "text": "先查天气"},
        {"type": "tool_use", "id": "toolu_weather", "name": "weather", "input": {"city": "北京"}},
        {"type": "text", "text": "再查时间"},
        {"type": "tool_use", "id": "toolu_clock", "name": "clock", "input": {}}
    ]});
    let stream = r#"
event: message_start
data: {"type":"message_start","message":{"id":"msg-1","type":"message","role":"assistant","content":[],"stop_reason":null,"usage":{"input_tokens":10,"output_tokens":0}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"先查"}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"天气"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: ping
data: {"type":"ping"}

event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_weather","name":"weather","input":{}}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"city\":"}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"北京\"}"}}

event: content_block_stop
data: {"type":"content_block_stop","index":1}

event: content_block_start
data: {"type":"content_block_start","index":2,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"再查时间"}}

event: content_block_stop
data: {"type":"content_block_stop","index":2}

event: content_block_start
data: {"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"toolu_clock","name":"clock","input":{}}}

event: content_block_stop
data: {"type":"content_block_stop","index":3}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":12}}

event: message_stop
data: {"type":"message_stop"}

"#;
    let first = assert_stream_matches_buffered(&request, stream, &message);
    assert_eq!(first.turns.len(), 2);
    let parts = &first.turns[1].parts;
    assert_eq!(
        parts
            .iter()
            .map(|part| part.kind.as_str())
            .collect::<Vec<_>>(),
        ["text", "tool_call", "text", "tool_call"]
    );
    assert_eq!(
        parts.iter().map(|part| part.part_index).collect::<Vec<_>>(),
        [0, 1, 2, 3]
    );
    assert_eq!(parts[1].call_id.as_deref(), Some("toolu_weather"));
    assert_eq!(
        parts[1].text.as_deref(),
        Some("weather\n{\"city\":\"北京\"}")
    );
    assert_eq!(parts[3].call_id.as_deref(), Some("toolu_clock"));
    let followup = json!({"messages": [
        request["messages"][0], message,
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_weather", "content": "晴"},
            {"type": "tool_result", "tool_use_id": "toolu_clock", "content": [{"type": "text", "text": "12:00"}]},
            {"type": "text", "text": "请总结"}
        ]}
    ]});
    let next = assert_stream_matches_buffered(
        &followup,
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"role\":\"assistant\",\"content\":[]}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"北京晴\"}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        &json!({"role": "assistant", "content": [{"type": "text", "text": "北京晴"}]}),
    );
    assert_eq!(
        next.turns
            .iter()
            .map(|turn| turn.role.as_str())
            .collect::<Vec<_>>(),
        ["user", "assistant", "user", "assistant"]
    );
    assert_eq!(next.turns[2].parts[0].kind, "tool_result");
    assert_eq!(
        next.turns[2].parts[0].tool_call_id.as_deref(),
        Some("toolu_weather")
    );
    assert_eq!(next.turns[2].parts[0].text.as_deref(), Some("晴"));
    assert_eq!(
        next.turns[2].parts[1].tool_call_id.as_deref(),
        Some("toolu_clock")
    );
    assert_eq!(next.turns[2].parts[2].text.as_deref(), Some("请总结"));
    assert_eq!(first.into_prefix().1, next.into_prefix().1[..2]);
}

#[test]
fn completed_responses_output_keeps_message_and_tool_order() {
    let buffered = json!({"output": [
        {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "checking"}]},
        {"type": "function_call", "call_id": "call-1", "name": "lookup", "arguments": "{}"},
        {"type": "function_call_output", "call_id": "call-1", "output": "ok"},
        {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "done"}]}
    ]});
    let stream = format!(
        "event: response.output_text.delta\ndata: {{\"type\":\"response.output_text.delta\",\"delta\":\"draft\"}}\n\nevent: response.completed\ndata: {}\n\n",
        json!({"type": "response.completed", "response": buffered}),
    );
    let body = assert_stream_matches_buffered(&json!({"input": "question"}), &stream, &buffered);
    assert_eq!(
        body.turns
            .iter()
            .map(|turn| turn.role.as_str())
            .collect::<Vec<_>>(),
        ["user", "assistant", "assistant", "tool", "assistant"]
    );
    assert_eq!(body.turns[2].parts[0].call_id.as_deref(), Some("call-1"));
    assert_eq!(body.turns[3].parts[0].call_id.as_deref(), Some("call-1"));
}
