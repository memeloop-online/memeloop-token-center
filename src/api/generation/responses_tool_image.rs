use std::{fmt, ops::Range};

use bytes::Bytes;
use serde::{
    Deserializer, Serialize,
    de::{DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor},
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
    let mut scan = ImageScan { image: None };
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let usage = RootSeed { scan: &mut scan }
        .deserialize(&mut deserializer)
        .and_then(|usage| {
            deserializer.end()?;
            Ok(usage)
        })
        .map_err(|error| match error.classify() {
            serde_json::error::Category::Syntax | serde_json::error::Category::Eof => {
                ResponsesToolImageParseError::InvalidJson
            }
            serde_json::error::Category::Data | serde_json::error::Category::Io => {
                ResponsesToolImageParseError::InvalidPayload
            }
        })?;
    let image = scan
        .image
        .ok_or(ResponsesToolImageParseError::InvalidPayload)?;
    let base = bytes.as_ptr() as usize;
    let start = (image.as_ptr() as usize)
        .checked_sub(base)
        .filter(|start| start.saturating_add(image.len()) <= bytes.len())
        .ok_or(ResponsesToolImageParseError::InvalidPayload)?;
    Ok(ParsedResponsesToolImage {
        image_range: start..start + image.len(),
        usage,
    })
}

struct RootSeed<'state, 'input> {
    scan: &'state mut ImageScan<'input>,
}

impl<'input, 'state> DeserializeSeed<'input> for RootSeed<'state, 'input> {
    type Value = Option<SanitizedImageUsage>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'input>,
    {
        deserializer.deserialize_map(RootVisitor { scan: self.scan })
    }
}

struct RootVisitor<'state, 'input> {
    scan: &'state mut ImageScan<'input>,
}

impl<'input, 'state> Visitor<'input> for RootVisitor<'state, 'input> {
    type Value = Option<SanitizedImageUsage>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a responses API object")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'input>,
    {
        let mut usage = None;
        while let Some(key) = map.next_key::<&'input str>()? {
            match key {
                "output" => {
                    map.next_value_seed(ValueScanSeed {
                        scan: self.scan,
                        depth: 1,
                    })?;
                }
                "usage" => {
                    let raw = map.next_value::<&'input RawValue>()?;
                    usage = parse_usage(raw).map_err(serde::de::Error::custom)?;
                }
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(usage)
    }
}

struct ValueScanSeed<'state, 'input> {
    scan: &'state mut ImageScan<'input>,
    depth: usize,
}

impl<'input, 'state> DeserializeSeed<'input> for ValueScanSeed<'state, 'input> {
    type Value = Option<&'input str>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'input>,
    {
        if self.depth > MAX_JSON_DEPTH {
            return Err(serde::de::Error::custom(ParseError));
        }
        deserializer.deserialize_any(ValueScanVisitor {
            scan: self.scan,
            depth: self.depth,
        })
    }
}

struct ValueScanVisitor<'state, 'input> {
    scan: &'state mut ImageScan<'input>,
    depth: usize,
}

impl<'input, 'state> Visitor<'input> for ValueScanVisitor<'state, 'input> {
    type Value = Option<&'input str>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_borrowed_str<E>(self, value: &'input str) -> Result<Self::Value, E> {
        Ok(Some(value))
    }

    fn visit_str<E>(self, _value: &str) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_string<E>(self, _value: String) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_bool<E>(self, _value: bool) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_i64<E>(self, _value: i64) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_u64<E>(self, _value: u64) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_f64<E>(self, _value: f64) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'input>,
    {
        while sequence
            .next_element_seed(ValueScanSeed {
                scan: self.scan,
                depth: self.depth + 1,
            })?
            .is_some()
        {}
        Ok(None)
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'input>,
    {
        let mut object_type = None;
        let mut result = None;
        while let Some(key) = map.next_key::<&'input str>()? {
            let value = map.next_value_seed(ValueScanSeed {
                scan: self.scan,
                depth: self.depth + 1,
            })?;
            match key {
                "type" => object_type = value,
                "result" => result = value,
                _ => {}
            }
        }
        if object_type == Some("image_generation_call") {
            let image = result.ok_or_else(|| serde::de::Error::custom(ParseError))?;
            self.scan.record(image).map_err(serde::de::Error::custom)?;
        }
        Ok(None)
    }
}

fn bounded_token(raw: &RawValue) -> Option<i64> {
    serde_json::from_str::<i64>(raw.get())
        .ok()
        .filter(|tokens| (0..=MAX_REPORTED_TOKENS).contains(tokens))
}

pub(super) fn parse_image_usage(raw: &RawValue) -> Option<SanitizedImageUsage> {
    parse_usage(raw).ok().flatten()
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
            body(json!({"output": [{"type": "image_generation_call", "result": "not-base64"}]})),
            body(json!({
                "output": [
                    {"type": "image_generation_call", "result": encoded},
                    {"type": "image_generation_call", "result": encoded}
                ]
            })),
            Bytes::from_static(
                br#"{"output":[{"type":"image_generation_call","result":"cG\u0035n"}]}"#,
            ),
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
                "output":[{
                    "type":"image_generation_call",
                    "result":"cG5n",
                    "nested":{"type":"image_generation_call","result":"cG5n"}
                }]
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

    #[test]
    fn skips_unrelated_root_subtrees_without_scanning_or_linking_images() {
        let response = body(json!({
            "ignored": {
                "padding": "x".repeat(1024 * 1024),
                "type": "image_generation_call",
                "result": "ZmFrZQ=="
            },
            "output": [{"type": "image_generation_call", "result": "cG5n"}]
        }));
        let parsed = parse_responses_tool_image(&response)
            .expect("only the normative root output participates in image extraction");
        assert_eq!(&response[parsed.image_range], b"cG5n");
    }
}
