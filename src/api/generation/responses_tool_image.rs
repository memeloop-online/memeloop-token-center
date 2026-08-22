use std::{fmt, ops::Range};

use bytes::Bytes;
use serde::{
    Deserializer, Serialize,
    de::{MapAccess, SeqAccess, Visitor},
};
use serde_json::value::RawValue;

use super::super::{MAX_IMAGE_RESPONSE, MAX_REPORTED_TOKENS};
use super::synchronous_image::is_valid_bounded_base64;

const MAX_JSON_DEPTH: usize = 128;

#[derive(Debug, Serialize)]
struct SanitizedTokenDetails {
    #[serde(skip_serializing_if = "Option::is_none")]
    image_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text_tokens: Option<i64>,
}

impl SanitizedTokenDetails {
    fn is_empty(&self) -> bool {
        self.image_tokens.is_none() && self.text_tokens.is_none()
    }
}

#[derive(Debug, Serialize)]
pub(super) struct SanitizedImageUsage {
    #[serde(skip_serializing_if = "Option::is_none")]
    total_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    input_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    input_tokens_details: Option<SanitizedTokenDetails>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_tokens_details: Option<SanitizedTokenDetails>,
}

impl SanitizedImageUsage {
    fn is_empty(&self) -> bool {
        self.total_tokens.is_none()
            && self.input_tokens.is_none()
            && self.output_tokens.is_none()
            && self.input_tokens_details.is_none()
            && self.output_tokens_details.is_none()
    }
}

pub(super) struct ParsedResponsesToolImage {
    pub(super) image_range: Range<usize>,
    pub(super) usage: Option<SanitizedImageUsage>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ResponsesToolImageParseError {
    InvalidJson,
    InvalidPayload,
}

struct ImageScan<'input> {
    image: Option<&'input str>,
}

impl<'input> ImageScan<'input> {
    fn record(&mut self, image: &'input str) -> Result<(), ParseError> {
        if self.image.is_some() || !is_valid_bounded_base64(image, MAX_IMAGE_RESPONSE) {
            return Err(ParseError);
        }
        self.image = Some(image);
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
struct ParseError;

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid responses-tool image payload")
    }
}

impl std::error::Error for ParseError {}

pub(super) fn parse_responses_tool_image(
    bytes: &Bytes,
) -> Result<ParsedResponsesToolImage, ResponsesToolImageParseError> {
    let root: &RawValue =
        serde_json::from_slice(bytes).map_err(|_| ResponsesToolImageParseError::InvalidJson)?;
    let mut scan = ImageScan { image: None };
    scan_raw_value(root, &mut scan, 0).map_err(|_| ResponsesToolImageParseError::InvalidPayload)?;
    let image = scan
        .image
        .ok_or(ResponsesToolImageParseError::InvalidPayload)?;
    let base = bytes.as_ptr() as usize;
    let start = (image.as_ptr() as usize)
        .checked_sub(base)
        .filter(|start| start.saturating_add(image.len()) <= bytes.len())
        .ok_or(ResponsesToolImageParseError::InvalidPayload)?;
    let usage = parse_root_usage(root).map_err(|_| ResponsesToolImageParseError::InvalidPayload)?;
    Ok(ParsedResponsesToolImage {
        image_range: start..start + image.len(),
        usage,
    })
}

fn scan_raw_value<'input>(
    raw: &'input RawValue,
    scan: &mut ImageScan<'input>,
    depth: usize,
) -> Result<(), ParseError> {
    if depth > MAX_JSON_DEPTH {
        return Err(ParseError);
    }
    match raw.get().as_bytes().first().copied() {
        Some(b'{') => {
            let mut deserializer = serde_json::Deserializer::from_str(raw.get());
            deserializer
                .deserialize_map(ObjectScanVisitor { scan, depth })
                .map_err(|_| ParseError)
        }
        Some(b'[') => {
            let mut deserializer = serde_json::Deserializer::from_str(raw.get());
            deserializer
                .deserialize_seq(ArrayScanVisitor { scan, depth })
                .map_err(|_| ParseError)
        }
        Some(_) => Ok(()),
        None => Err(ParseError),
    }
}

struct ObjectScanVisitor<'state, 'input> {
    scan: &'state mut ImageScan<'input>,
    depth: usize,
}

impl<'input, 'state> Visitor<'input> for ObjectScanVisitor<'state, 'input> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON object")
    }

    fn visit_map<A>(self, mut map: A) -> Result<(), A::Error>
    where
        A: MapAccess<'input>,
    {
        let mut object_type = None;
        let mut result = None;
        while let Some(key) = map.next_key::<&'input str>()? {
            let value = map.next_value::<&'input RawValue>()?;
            match key {
                "type" => object_type = Some(value),
                "result" => result = Some(value),
                _ => {}
            }
            scan_raw_value(value, self.scan, self.depth + 1).map_err(serde::de::Error::custom)?;
        }
        let is_image_call = object_type
            .and_then(|value| serde_json::from_str::<&'input str>(value.get()).ok())
            .is_some_and(|value| value == "image_generation_call");
        if is_image_call {
            // Base64 never needs JSON escaping. Requiring a borrowed string is
            // fail-closed and retains only the ingress allocation.
            let image = result
                .and_then(|value| serde_json::from_str::<&'input str>(value.get()).ok())
                .ok_or_else(|| serde::de::Error::custom(ParseError))?;
            self.scan.record(image).map_err(serde::de::Error::custom)?;
        }
        Ok(())
    }
}

struct ArrayScanVisitor<'state, 'input> {
    scan: &'state mut ImageScan<'input>,
    depth: usize,
}

impl<'input, 'state> Visitor<'input> for ArrayScanVisitor<'state, 'input> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON array")
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<(), A::Error>
    where
        A: SeqAccess<'input>,
    {
        while let Some(value) = sequence.next_element::<&'input RawValue>()? {
            scan_raw_value(value, self.scan, self.depth + 1).map_err(serde::de::Error::custom)?;
        }
        Ok(())
    }
}

fn parse_root_usage(raw: &RawValue) -> Result<Option<SanitizedImageUsage>, ParseError> {
    if !raw.get().starts_with('{') {
        return Err(ParseError);
    }
    let mut deserializer = serde_json::Deserializer::from_str(raw.get());
    deserializer
        .deserialize_map(RootUsageVisitor)
        .map_err(|_| ParseError)
}

struct RootUsageVisitor;

impl<'de> Visitor<'de> for RootUsageVisitor {
    type Value = Option<SanitizedImageUsage>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a responses API object")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut usage = None;
        while let Some(key) = map.next_key::<&'de str>()? {
            let value = map.next_value::<&'de RawValue>()?;
            if key == "usage" {
                usage = parse_usage(value).map_err(serde::de::Error::custom)?;
            }
        }
        Ok(usage)
    }
}

fn bounded_token(raw: &RawValue) -> Option<i64> {
    serde_json::from_str::<i64>(raw.get())
        .ok()
        .filter(|tokens| (0..=MAX_REPORTED_TOKENS).contains(tokens))
}

fn parse_usage(raw: &RawValue) -> Result<Option<SanitizedImageUsage>, ParseError> {
    if !raw.get().starts_with('{') {
        return Ok(None);
    }
    let mut deserializer = serde_json::Deserializer::from_str(raw.get());
    let usage = deserializer
        .deserialize_map(UsageVisitor)
        .map_err(|_| ParseError)?;
    Ok((!usage.is_empty()).then_some(usage))
}

struct UsageVisitor;

impl<'de> Visitor<'de> for UsageVisitor {
    type Value = SanitizedImageUsage;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a usage object")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut usage = SanitizedImageUsage {
            total_tokens: None,
            input_tokens: None,
            output_tokens: None,
            input_tokens_details: None,
            output_tokens_details: None,
        };
        while let Some(key) = map.next_key::<&'de str>()? {
            let value = map.next_value::<&'de RawValue>()?;
            match key {
                "total_tokens" => usage.total_tokens = bounded_token(value),
                "input_tokens" => usage.input_tokens = bounded_token(value),
                "output_tokens" => usage.output_tokens = bounded_token(value),
                "input_tokens_details" => usage.input_tokens_details = parse_details(value),
                "output_tokens_details" => usage.output_tokens_details = parse_details(value),
                _ => {}
            }
        }
        Ok(usage)
    }
}

fn parse_details(raw: &RawValue) -> Option<SanitizedTokenDetails> {
    if !raw.get().starts_with('{') {
        return None;
    }
    let mut deserializer = serde_json::Deserializer::from_str(raw.get());
    let details = deserializer.deserialize_map(TokenDetailsVisitor).ok()?;
    (!details.is_empty()).then_some(details)
}

struct TokenDetailsVisitor;

impl<'de> Visitor<'de> for TokenDetailsVisitor {
    type Value = SanitizedTokenDetails;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a token details object")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut details = SanitizedTokenDetails {
            image_tokens: None,
            text_tokens: None,
        };
        while let Some(key) = map.next_key::<&'de str>()? {
            let value = map.next_value::<&'de RawValue>()?;
            match key {
                "image_tokens" => details.image_tokens = bounded_token(value),
                "text_tokens" => details.text_tokens = bounded_token(value),
                _ => {}
            }
        }
        Ok(details)
    }
}

#[cfg(test)]
mod tests {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use serde_json::json;

    use super::*;

    fn body(value: serde_json::Value) -> Bytes {
        Bytes::from(serde_json::to_vec(&value).expect("test response is JSON"))
    }

    #[test]
    fn borrows_the_only_arbitrarily_nested_image() {
        let encoded = STANDARD.encode(b"png");
        let response = body(json!({
            "output": [{"nested": {"type": "image_generation_call", "result": encoded}}],
            "usage": {
                "total_tokens": 7,
                "input_tokens_details": {"image_tokens": 3, "secret": "drop"},
                "secret": "drop"
            },
            "secret": "drop"
        }));
        let parsed = parse_responses_tool_image(&response).expect("one image is valid");
        assert_eq!(&response[parsed.image_range], encoded.as_bytes());
        assert_eq!(
            serde_json::to_value(parsed.usage).expect("usage serializes"),
            json!({
                "total_tokens": 7,
                "input_tokens_details": {"image_tokens": 3}
            })
        );
    }

    #[test]
    fn rejects_duplicate_missing_invalid_and_escaped_results() {
        let encoded = STANDARD.encode(b"png");
        for response in [
            body(json!({"output": []})),
            body(json!({"type": "image_generation_call", "result": "not-base64"})),
            body(json!({
                "a": {"type": "image_generation_call", "result": encoded},
                "b": {"type": "image_generation_call", "result": encoded}
            })),
            Bytes::from_static(br#"{"type":"image_generation_call","result":"cG\u0035n"}"#),
        ] {
            assert!(parse_responses_tool_image(&response).is_err());
        }
    }

    #[test]
    fn field_order_does_not_change_detection() {
        let response = Bytes::from_static(
            br#"{"output":[{"result":"cG5n","other":true,"type":"image_generation_call"}]}"#,
        );
        let parsed = parse_responses_tool_image(&response).expect("result may precede type");
        assert_eq!(&response[parsed.image_range], b"cG5n");
    }

    #[test]
    fn malformed_json_and_trailing_data_are_classified_separately() {
        for response in [
            Bytes::from_static(br#"{"output":["#),
            Bytes::from_static(br#"{"type":"image_generation_call","result":"cG5n"} trailing"#),
        ] {
            assert_eq!(
                parse_responses_tool_image(&response).err(),
                Some(ResponsesToolImageParseError::InvalidJson)
            );
        }
    }

    #[test]
    fn ancestor_and_descendant_candidates_are_duplicates() {
        let response = Bytes::from_static(
            br#"{
                "type":"image_generation_call",
                "result":"cG5n",
                "nested":{"type":"image_generation_call","result":"cG5n"}
            }"#,
        );
        assert_eq!(
            parse_responses_tool_image(&response).err(),
            Some(ResponsesToolImageParseError::InvalidPayload)
        );
    }

    #[test]
    fn usage_is_bounded_and_never_forwards_unknown_fields() {
        let response = body(json!({
            "output": [{"type": "image_generation_call", "result": "cG5n"}],
            "usage": {
                "total_tokens": MAX_REPORTED_TOKENS + 1,
                "input_tokens": -1,
                "output_tokens": 4,
                "input_tokens_details": {
                    "image_tokens": 2,
                    "text_tokens": MAX_REPORTED_TOKENS + 1,
                    "secret": "drop"
                },
                "secret": "drop"
            }
        }));
        let parsed = parse_responses_tool_image(&response).expect("image remains valid");
        assert_eq!(
            serde_json::to_value(parsed.usage).expect("usage serializes"),
            json!({
                "output_tokens": 4,
                "input_tokens_details": {"image_tokens": 2}
            })
        );
    }
}
