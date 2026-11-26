use serde_json::Value;

/// Monotonic gateway observations, sampled before downstream delivery or
/// archive settlement. These are not supplier-internal decoding timestamps.
#[derive(Default)]
pub(super) struct OutputTiming {
    first: Option<i64>,
    terminal: Option<i64>,
    last_content_at: Option<i64>,
}

impl OutputTiming {
    /// Use the ingress clock for both persisted latency and output observations.
    /// The separate admission clock still owns resource/deadline budgets.
    pub(super) fn finish_with_duration(
        &self,
        succeeded: bool,
        context: crate::api::proxy_diagnostics::Context,
        now: std::time::Instant,
    ) -> (i64, Option<i64>, Option<i64>) {
        let (first, generation) = self.finish(succeeded);
        (context.elapsed_millis_at(now), first, generation)
    }

    pub(super) fn observe(&mut self, frame: &[u8], terminal: bool, elapsed_ms: i64) {
        if self.terminal.is_some() {
            return;
        }
        if has_output_delta(frame) {
            self.first.get_or_insert(elapsed_ms);
            self.last_content_at = Some(elapsed_ms);
        }
        if terminal {
            self.terminal = Some(elapsed_ms);
        }
    }

    pub(super) fn finish(&self, succeeded: bool) -> (Option<i64>, Option<i64>) {
        let duration = if succeeded && self.terminal.is_some() {
            self.first
                .zip(self.last_content_at)
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
    let mut lines = frame.lines().filter_map(|line| {
        line.strip_prefix("data:")
            .map(|line| line.strip_prefix(' ').unwrap_or(line))
    });
    let Some(first) = lines.next() else {
        return false;
    };
    // Most events have one data line: borrow it instead of allocating a
    // Vec and joined String on every output observation.
    let folded;
    let data = if let Some(second) = lines.next() {
        folded = std::iter::once(first)
            .chain(std::iter::once(second))
            .chain(lines)
            .collect::<Vec<_>>()
            .join("\n");
        folded.as_str()
    } else {
        first
    };
    let Ok(value) = serde_json::from_str::<Value>(data) else {
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
    fn persisted_duration_includes_time_before_admission_without_changing_generation() {
        use std::time::{Duration, Instant};
        let ingress = Instant::now();
        let context = crate::api::proxy_diagnostics::Context::with_started_for_test(
            uuid::Uuid::nil(),
            ingress,
        );
        // Body ingestion/admission took 500 ms before the budget clock began.
        // Finalizing at 900 ms using that later clock would report 400 ms,
        // less than the 700 ms first-output observation.
        let admission = ingress + Duration::from_millis(500);
        let mut timing = OutputTiming::default();
        timing.observe(
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"x\"}\n\n",
            false,
            context.elapsed_millis_at(ingress + Duration::from_millis(700)),
        );
        timing.observe(
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"y\"}\n\n",
            false,
            context.elapsed_millis_at(ingress + Duration::from_millis(800)),
        );
        timing.observe(b"data: {}\n\n", true, 850);
        let finalized = ingress + Duration::from_millis(900);
        assert_eq!(
            timing.finish_with_duration(true, context, finalized),
            (900, Some(700), Some(100))
        );
        assert_eq!(
            timing.finish_with_duration(false, context, finalized),
            (900, Some(700), None)
        );
        assert_eq!(finalized.duration_since(admission).as_millis(), 400);
    }

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
        timing.observe(
            b"data: {\"type\":\"response.reasoning_text.delta\",\"delta\":\"x\"}\n\n",
            false,
            400,
        );
        timing.observe(b"data: {}\n\n", true, 500);
        timing.observe(b"data: {}\n\n", true, 900);
        assert_eq!(timing.finish(true), (Some(300), Some(100)));
        assert_eq!(timing.finish(false), (Some(300), None));
    }

    #[test]
    fn incomplete_or_single_content_observation_does_not_invent_generation_time() {
        let mut timing = OutputTiming::default();
        let delta = b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"x\"}\n\n";
        timing.observe(delta, false, 100);
        timing.observe(b"data: {}\n\n", true, 900);
        assert_eq!(timing.finish(true), (Some(100), None));
        let mut timing = OutputTiming::default();
        timing.observe(delta, false, 100);
        timing.observe(delta, false, 200);
        assert_eq!(timing.finish(true), (Some(100), None));
        timing.observe(b"data: {}\n\n", true, 900);
        assert_eq!(timing.finish(true), (Some(100), Some(100)));
    }

    #[test]
    fn late_usage_done_and_post_terminal_content_do_not_extend_output_window() {
        let mut timing = OutputTiming::default();
        timing.observe(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n",
            false,
            10,
        );
        // A Chat frame may contain both the final content and finish marker.
        timing.observe(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"y\"},\"finish_reason\":\"stop\"}]}\n\n",
            true,
            30,
        );
        timing.observe(
            b"data: {\"usage\":{\"completion_tokens\":40}}\n\n",
            false,
            500,
        );
        timing.observe(b"data: [DONE]\n\n", true, 900);
        timing.observe(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"z\"}}]}\n\n",
            false,
            1000,
        );
        assert_eq!(timing.finish(true), (Some(10), Some(20)));
    }

    #[test]
    fn heartbeats_and_comments_are_not_output_but_folded_data_is() {
        let mut timing = OutputTiming::default();
        timing.observe(b": heartbeat\n\n", false, 1);
        timing.observe(b"data: {}\n\n", false, 2);
        timing.observe(
            b"data: {\"type\":\"content_block_delta\",\n data: ignored\n",
            false,
            3,
        );
        assert_eq!(timing.finish(true), (None, None));
        timing.observe(
            b"data: {\"type\":\"content_block_delta\",\ndata: \"delta\":{\"thinking\":\"x\"}}\n\n",
            false,
            10,
        );
        timing.observe(
            b"data: {\"type\":\"content_block_delta\",\"delta\":{\"partial_json\":\"{}\"}}\n\n",
            false,
            20,
        );
        timing.observe(b"data: [DONE]\n\n", true, 30);
        assert_eq!(timing.finish(true), (Some(10), Some(10)));
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
