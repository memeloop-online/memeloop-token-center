use serde_json::Value;

/// Monotonic gateway observations, sampled before downstream delivery or
/// archive settlement. These are not supplier-internal decoding timestamps.
#[derive(Default)]
pub(super) struct OutputTiming {
    first: Option<i64>,
    terminal: Option<i64>,
}

impl OutputTiming {
    pub(super) fn observe(&mut self, frame: &[u8], terminal: bool, elapsed_ms: i64) {
        if self.terminal.is_some() {
            return;
        }
        if terminal {
            self.terminal = Some(elapsed_ms);
        } else if self.first.is_none() && has_output_delta(frame) {
            self.first = Some(elapsed_ms);
        }
    }

    pub(super) fn finish(&self, succeeded: bool) -> (Option<i64>, Option<i64>) {
        let duration = if succeeded {
            self.first
                .zip(self.terminal)
                .and_then(|(first, last)| (last > first).then_some(last - first))
        } else {
            None
        };
        (self.first, duration)
    }
}

fn nonempty(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty())
}

fn has_output_delta(frame: &[u8]) -> bool {
    let Ok(frame) = std::str::from_utf8(frame) else {
        return false;
    };
    // The existing SSE capture has already assembled complete events. JSON
    // data lines may be folded; comments and transport chunks are not tokens.
    let data = frame
        .lines()
        .filter_map(|line| {
            line.strip_prefix("data:")
                .map(|line| line.strip_prefix(' ').unwrap_or(line))
        })
        .collect::<Vec<_>>()
        .join("\n");
    let Ok(value) = serde_json::from_str::<Value>(&data) else {
        return false;
    };
    match value.get("type").and_then(Value::as_str) {
        Some(
            "response.output_text.delta"
            | "response.reasoning_text.delta"
            | "response.reasoning_summary_text.delta"
            | "response.function_call_arguments.delta"
            | "response.refusal.delta",
        ) => nonempty(value.get("delta")),
        Some("content_block_delta") => {
            nonempty(value.pointer("/delta/text"))
                || nonempty(value.pointer("/delta/partial_json"))
                || nonempty(value.pointer("/delta/thinking"))
        }
        _ => value
            .get("choices")
            .and_then(Value::as_array)
            .is_some_and(|choices| {
                choices.iter().any(|choice| {
                    nonempty(choice.pointer("/delta/content"))
                        || nonempty(choice.pointer("/delta/reasoning_content"))
                        || choice
                            .pointer("/delta/tool_calls")
                            .and_then(Value::as_array)
                            .is_some_and(|calls| {
                                calls
                                    .iter()
                                    .any(|call| nonempty(call.pointer("/function/arguments")))
                            })
                })
            }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_output_starts_clock_and_terminal_freezes_it() {
        let mut timing = OutputTiming::default();
        timing.observe(b"data: {\"type\":\"response.created\"}\n\n", false, 100);
        timing.observe(
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"\"}\n\n",
            false,
            200,
        );
        timing.observe(
            b"data: {\"type\":\"response.function_call_arguments.delta\",\"delta\":\"{\"}\n\n",
            false,
            300,
        );
        timing.observe(b"data: {}\n\n", true, 500);
        timing.observe(b"data: {}\n\n", true, 900);
        assert_eq!(timing.finish(true), (Some(300), Some(200)));
        assert_eq!(timing.finish(false), (Some(300), None));
    }

    #[test]
    fn terminal_only_and_same_batch_do_not_invent_speed() {
        let mut timing = OutputTiming::default();
        timing.observe(b"data: {}\n\n", true, 10);
        assert_eq!(timing.finish(true), (None, None));
        let mut timing = OutputTiming::default();
        timing.observe(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n",
            false,
            10,
        );
        timing.observe(b"data: [DONE]\n\n", true, 10);
        assert_eq!(timing.finish(true), (Some(10), None));
    }
}
