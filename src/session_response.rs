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
                let mut text: Option<String> = None;
                for block in blocks {
                    if matches!(block.kind.as_str(), "text" | "output_text") {
                        match text.as_mut() {
                            Some(text) if !text.is_empty() => text.push_str(&block.text),
                            _ => text = Some(block.text),
                        }
                    }
                }
                text
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
            .or_else(|| self.unmarked.map(last_paragraph))
            .filter(|text| !text.trim().is_empty())
    }
}

fn last_paragraph(mut text: String) -> String {
    let trimmed = text.trim_end();
    let end = trimmed.len();
    let mut start = 0;
    let mut position = 0;
    for line in trimmed.split_inclusive('\n') {
        position += line.len();
        if line.trim().is_empty() {
            start = position;
        }
    }
    // Keep the selected paragraph in the original allocation, including when
    // the entire unmarked answer is one very large paragraph.
    text.truncate(end);
    drop(text.drain(..start));
    text
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
    fn unmarked_answer_keeps_the_exact_entire_last_paragraph() {
        for (input, expected) in [
            ("", None),
            (" \t\r\n\u{2003}", None),
            ("one paragraph", Some("one paragraph")),
            (
                "  indented\nsecond line \t\n",
                Some("  indented\nsecond line"),
            ),
            ("first\n\nlast\nline\n", Some("last\nline")),
            (
                "first\r\n \r\n末段😀\r\n下一行\t ",
                Some("末段😀\r\n下一行"),
            ),
            ("first\n\u{2003}\n\n  final\n\n \t\n", Some("  final")),
            ("first\n \t\nsecond\n\nlast", Some("last")),
        ] {
            assert_eq!(
                Answer {
                    unmarked: Some(input.into()),
                    ..Answer::default()
                }
                .finish()
                .as_deref(),
                expected,
                "input: {input:?}"
            );
        }
    }

    #[test]
    fn selected_text_blocks_and_last_paragraph_transfer_the_allocation() {
        let paragraph = format!("  {}😀\nlast line", "x".repeat(1024 * 1024));
        for prefix in ["", "earlier paragraph\r\n \r\n"] {
            let source = format!("{prefix}{paragraph}\r\n\t ");
            let pointer = source.as_ptr();
            let capacity = source.capacity();
            let text = Content::Blocks(vec![
                TextBlock {
                    kind: "tool_use".into(),
                    text: "ignored".into(),
                },
                TextBlock {
                    kind: "text".into(),
                    ..TextBlock::default()
                },
                TextBlock {
                    kind: "output_text".into(),
                    text: source,
                },
            ])
            .text()
            .unwrap();
            assert_eq!(
                text.as_ptr(),
                pointer,
                "reuse the first nonempty text block"
            );
            let selected = Answer {
                unmarked: Some(text),
                ..Answer::default()
            }
            .finish()
            .unwrap();
            assert_eq!(selected, paragraph);
            assert_eq!(selected.as_ptr(), pointer, "compact the paragraph in place");
            assert_eq!(selected.capacity(), capacity);
        }
        assert_eq!(
            Content::Blocks(vec![
                TextBlock {
                    kind: "text".into(),
                    text: "first\n".into()
                },
                TextBlock {
                    kind: "tool_use".into(),
                    text: "ignored".into()
                },
                TextBlock {
                    kind: "output_text".into(),
                    text: "second".into()
                },
            ])
            .text()
            .as_deref(),
            Some("first\nsecond")
        );
        assert_eq!(Content::Blocks(vec![]).text(), None);
    }

    #[test]
    fn explicit_answer_keeps_all_paragraphs_and_whitespace_without_fallback() {
        let explicit = "  first\n\nlast😀\n\t ".to_owned();
        let pointer = explicit.as_ptr();
        let selected = Answer {
            explicit: Some(explicit),
            unmarked: Some("fallback".into()),
        }
        .finish()
        .unwrap();
        assert_eq!(selected, "  first\n\nlast😀\n\t ");
        assert_eq!(selected.as_ptr(), pointer);
        assert_eq!(
            Answer {
                explicit: Some(" \n\t".into()),
                unmarked: Some("fallback".into()),
            }
            .finish(),
            None
        );
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
