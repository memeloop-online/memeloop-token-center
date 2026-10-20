//! Final-answer extraction for background archive projection. Deserialize only
//! textual protocol fields: tool arguments, results and media are ignored.
use serde::Deserialize;
use std::io::{BufRead, Read};

const MAX_EVENT_BYTES: usize = 4 * 1024 * 1024;

#[derive(Default, Deserialize)]
#[serde(default)]
struct Message {
    #[serde(rename = "type")]
    kind: String,
    role: String,
    phase: Option<String>,
    channel: Option<String>,
    content: Content,
}

#[derive(Default, Deserialize)]
#[serde(untagged)]
enum Content {
    Text(String),
    Blocks(Vec<TextBlock>),
    #[default]
    Empty,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct TextBlock {
    #[serde(rename = "type")]
    kind: String,
    text: String,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Choice {
    index: usize,
    message: Message,
    delta: ChatDelta,
    finish_reason: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct ChatDelta {
    content: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Response {
    status: Option<String>,
    output: Vec<Message>,
    choices: Vec<Choice>,
    content: Content,
    stop_reason: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Event {
    #[serde(rename = "type")]
    kind: String,
    response: Response,
    item: Message,
    delta: Delta,
    choices: Vec<Choice>,
    content_block: TextBlock,
}

#[derive(Default, Deserialize)]
#[serde(untagged)]
enum Delta {
    Text(String),
    Block {
        text: Option<String>,
        stop_reason: Option<String>,
    },
    #[default]
    Empty,
}

#[derive(Default)]
struct Answer {
    explicit: Option<String>,
    unmarked: Option<String>,
}

impl Content {
    fn text(self) -> Option<String> {
        match self {
            Self::Text(text) => Some(text),
            Self::Blocks(blocks) => {
                let mut text = String::new();
                let mut found = false;
                for block in blocks {
                    if matches!(block.kind.as_str(), "text" | "output_text") {
                        text.push_str(&block.text);
                        found = true;
                    }
                }
                found.then_some(text)
            }
            Self::Empty => None,
        }
    }
}

impl Answer {
    fn message(&mut self, message: Message) {
        if !matches!(message.role.as_str(), "" | "assistant")
            || !matches!(message.kind.as_str(), "" | "message" | "agent_message")
        {
            return;
        }
        let Some(text) = message.content.text() else {
            return;
        };
        match message.phase.or(message.channel).as_deref() {
            Some("final" | "final_answer") => self.explicit = Some(text),
            None => self.unmarked = Some(text),
            _ => {}
        }
    }

    fn response(&mut self, response: Response) -> Result<(), &'static str> {
        if response
            .status
            .as_deref()
            .is_some_and(|status| status != "completed")
        {
            return Err("response_incomplete");
        }
        for message in response.output {
            self.message(message);
        }
        for choice in response.choices {
            if choice.index == 0 {
                if choice.finish_reason.as_deref().is_some_and(|reason| {
                    !matches!(reason, "stop" | "tool_calls" | "function_call")
                }) {
                    return Err("response_incomplete");
                }
                self.message(choice.message);
            }
        }
        if let Some(text) = response.content.text() {
            if response
                .stop_reason
                .as_deref()
                .is_some_and(|reason| !matches!(reason, "end_turn" | "tool_use" | "stop_sequence"))
            {
                return Err("response_incomplete");
            }
            self.unmarked = Some(text);
        }
        Ok(())
    }

    fn finish(self) -> Option<String> {
        self.explicit
            .or_else(|| self.unmarked.map(|text| last_paragraph(&text).to_owned()))
            .filter(|text| !text.trim().is_empty())
    }
}

fn last_paragraph(text: &str) -> &str {
    let text = text.trim_end();
    let mut start = 0;
    let mut position = 0;
    for line in text.split_inclusive('\n') {
        position += line.len();
        if line.trim().is_empty() {
            start = position;
        }
    }
    &text[start..]
}

/// Caller limits total archive bytes and uses a blocking worker, never the
/// HTTP/inference critical path. A missing terminal marker is not success.
pub(crate) fn extract(mut reader: impl BufRead) -> Result<Option<String>, &'static str> {
    loop {
        let bytes = reader.fill_buf().map_err(|_| "archive_read_failed")?;
        if bytes.is_empty() {
            return Err("archive_empty");
        }
        let leading = bytes
            .iter()
            .take_while(|byte| byte.is_ascii_whitespace())
            .count();
        if leading == 0 {
            break;
        }
        reader.consume(leading);
    }
    if reader
        .fill_buf()
        .map_err(|_| "archive_read_failed")?
        .first()
        == Some(&b'{')
    {
        let response: Response =
            serde_json::from_reader(reader).map_err(|_| "archive_invalid_json")?;
        let mut answer = Answer::default();
        answer.response(response)?;
        return Ok(answer.finish());
    }
    let mut answer = Answer::default();
    let mut delta_text = String::new();
    let mut terminal = false;
    let mut event = Vec::new();
    loop {
        let mut line = Vec::new();
        let read = reader
            .by_ref()
            .take((MAX_EVENT_BYTES + 1) as u64)
            .read_until(b'\n', &mut line)
            .map_err(|_| "archive_read_failed")?;
        if read == 0 {
            break;
        }
        if line.len() > MAX_EVENT_BYTES {
            return Err("response_event_too_large");
        }
        let line = line.strip_suffix(b"\n").unwrap_or(&line);
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            if event.is_empty() {
                continue;
            }
            let data = std::mem::take(&mut event);
            if data == b"[DONE]" {
                terminal = true;
                continue;
            }
            let value: Event =
                serde_json::from_slice(&data).map_err(|_| "archive_invalid_event")?;
            match value.kind.as_str() {
                "response.completed" => {
                    answer.response(value.response)?;
                    terminal = true;
                }
                "response.failed" | "response.incomplete" | "error" => {
                    return Err("response_incomplete");
                }
                "response.output_item.done" => answer.message(value.item),
                // Responses output_item.done/completed carry phase markers.
                // Deltas alone cannot distinguish commentary from final text.
                "response.output_text.delta" => {
                    if let Delta::Text(text) = value.delta {
                        drop(text);
                    }
                }
                "content_block_start" if value.content_block.kind == "text" => {
                    delta_text.push_str(&value.content_block.text)
                }
                "content_block_delta" => {
                    if let Delta::Block {
                        text: Some(text), ..
                    } = value.delta
                    {
                        delta_text.push_str(&text);
                    }
                }
                "message_delta" => {
                    if let Delta::Block {
                        stop_reason: Some(reason),
                        ..
                    } = value.delta
                        && !matches!(reason.as_str(), "end_turn" | "tool_use" | "stop_sequence")
                    {
                        return Err("response_incomplete");
                    }
                }
                "message_stop" => terminal = true,
                "" => {
                    for choice in value.choices {
                        if choice.index == 0 {
                            if let Some(text) = choice.delta.content {
                                delta_text.push_str(&text);
                            }
                            if let Some(reason) = choice.finish_reason
                                && !matches!(
                                    reason.as_str(),
                                    "stop" | "tool_calls" | "function_call"
                                )
                            {
                                return Err("response_incomplete");
                            }
                        }
                    }
                }
                _ => {}
            }
        } else if let Some(data) = line.strip_prefix(b"data:") {
            let data = data.strip_prefix(b" ").unwrap_or(data);
            if event.len() + data.len() + 1 > MAX_EVENT_BYTES {
                return Err("response_event_too_large");
            }
            if !event.is_empty() {
                event.push(b'\n');
            }
            event.extend_from_slice(data);
        }
    }
    if !terminal || !event.is_empty() {
        return Err("response_incomplete");
    }
    if !delta_text.is_empty() {
        answer.unmarked = Some(delta_text);
    }
    Ok(answer.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Cursor;

    fn parse(value: serde_json::Value) -> Result<Option<String>, &'static str> {
        extract(Cursor::new(serde_json::to_vec(&value).unwrap()))
    }

    #[test]
    fn explicit_final_wins_and_tools_are_never_promoted() {
        assert_eq!(parse(json!({"status":"completed","output":[
            {"type":"message","phase":"commentary","role":"assistant","content":[{"type":"output_text","text":"Inspecting"}]},
            {"type":"function_call","arguments":"tool-only","name":"exec"},
            {"type":"message","phase":"final_answer","role":"assistant","content":[{"type":"output_text","text":"完整第一段\n\n完整结论"}]}
        ]})).unwrap().as_deref(), Some("完整第一段\n\n完整结论"));
        assert_eq!(
            parse(json!({"output":[{"type":"message","phase":"commentary","content":"Checking"}]}))
                .unwrap(),
            None
        );
        assert_eq!(parse(json!({"choices":[{"message":{"role":"assistant","content":"Explanation\n\nConclusion"},"finish_reason":"stop"}]})).unwrap().as_deref(), Some("Conclusion"));
        assert_eq!(parse(json!({"content":[{"type":"text","text":"说明\n\n结论"},{"type":"tool_use","input":{"secret":"tool"}}],"stop_reason":"end_turn"})).unwrap().as_deref(), Some("结论"));
    }

    #[test]
    fn streaming_formats_require_terminal_and_preserve_final_markers() {
        let responses = "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"phase\":\"final\",\"content\":\"First\\n\\nLast\"}}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n";
        assert_eq!(
            extract(Cursor::new(responses)).unwrap().as_deref(),
            Some("First\n\nLast")
        );
        let chat = "data: {\"choices\":[{\"delta\":{\"content\":\"Start\\n\\nFinal😀\"}}]}\n\ndata: [DONE]\n\n";
        assert_eq!(
            extract(Cursor::new(chat)).unwrap().as_deref(),
            Some("Final😀")
        );
        let anthropic = "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Answer\"}}\n\ndata: {\"type\":\"message_stop\"}\n\n";
        assert_eq!(
            extract(Cursor::new(anthropic)).unwrap().as_deref(),
            Some("Answer")
        );
        assert!(extract(Cursor::new(chat.replace("data: [DONE]\n\n", ""))).is_err());
        assert!(parse(json!({"status":"incomplete","output":[]})).is_err());
        assert!(
            parse(json!({"choices":[{"message":{"content":"partial"},"finish_reason":"length"}]}))
                .is_err()
        );
    }
}
