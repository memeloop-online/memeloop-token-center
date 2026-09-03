use std::{fmt, ops::Range};

use bytes::Bytes;
use serde::{
    Deserializer, Serialize,
    de::{MapAccess, SeqAccess, Visitor},
};
use serde_json::value::RawValue;

use crate::model::ArchivedGenerationAsset;

use super::super::MAX_IMAGE_RESPONSE;
use super::{
    responses_tool_image::{SanitizedImageUsage, parse_image_usage},
    synchronous_image::is_valid_bounded_base64,
};

const MAX_REVISED_PROMPT_BYTES: usize = 32_000;
const MAX_REVISED_PROMPT_JSON_BYTES: usize = MAX_REVISED_PROMPT_BYTES * 6 + 2;
const MAX_PROVIDER_URL_JSON_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum OpenAiImageParseError {
    InvalidJson,
    InvalidPayload,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum OpenAiImageBuildError {
    TooLarge,
    InvalidAssets,
    Internal,
}

#[derive(Debug)]
pub(super) enum OpenAiImageItem {
    Base64 {
        range: Range<usize>,
        revised_prompt: Option<String>,
    },
    Url {
        url: String,
        revised_prompt: Option<String>,
    },
}

#[derive(Debug)]
pub(super) struct ParsedOpenAiImageResponse {
    created: Option<i64>,
    pub(super) items: Vec<OpenAiImageItem>,
    usage: Option<SanitizedImageUsage>,
}

impl ParsedOpenAiImageResponse {
    pub(super) fn url_assets(&self) -> impl Iterator<Item = (usize, &str)> {
        self.items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| match item {
                OpenAiImageItem::Url { url, .. } => Some((index, url.as_str())),
                OpenAiImageItem::Base64 { .. } => None,
            })
    }
}

#[derive(Clone, Copy, Debug)]
struct PayloadError;

impl fmt::Display for PayloadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid OpenAI image response")
    }
}

impl std::error::Error for PayloadError {}

pub(super) fn parse_openai_image_response(
    bytes: &Bytes,
    expected_count: i64,
) -> Result<ParsedOpenAiImageResponse, OpenAiImageParseError> {
    let root: &RawValue =
        serde_json::from_slice(bytes).map_err(|_| OpenAiImageParseError::InvalidJson)?;
    if !root.get().starts_with('{') {
        return Err(OpenAiImageParseError::InvalidPayload);
    }
    let mut deserializer = serde_json::Deserializer::from_str(root.get());
    let fields = deserializer
        .deserialize_map(RootVisitor)
        .map_err(|_| OpenAiImageParseError::InvalidPayload)?;
    let expected_count = usize::try_from(expected_count)
        .ok()
        .filter(|count| (1..=10).contains(count))
        .ok_or(OpenAiImageParseError::InvalidPayload)?;
    let data = fields.data.ok_or(OpenAiImageParseError::InvalidPayload)?;
    let mut data_deserializer = serde_json::Deserializer::from_str(data.get());
    let raw_items = data_deserializer
        .deserialize_seq(DataVisitor)
        .map_err(|_| OpenAiImageParseError::InvalidPayload)?;
    if raw_items.len() != expected_count {
        return Err(OpenAiImageParseError::InvalidPayload);
    }

    let mut items = Vec::with_capacity(raw_items.len());
    for raw in raw_items {
        let mut item_deserializer = serde_json::Deserializer::from_str(raw.get());
        let item = item_deserializer
            .deserialize_map(ItemVisitor)
            .map_err(|_| OpenAiImageParseError::InvalidPayload)?;
        items.push(item.into_item(bytes)?);
    }
    Ok(ParsedOpenAiImageResponse {
        created: fields.created,
        items,
        usage: fields.usage,
    })
}

struct RootFields<'input> {
    created: Option<i64>,
    data: Option<&'input RawValue>,
    usage: Option<SanitizedImageUsage>,
}

struct RootVisitor;

impl<'input> Visitor<'input> for RootVisitor {
    type Value = RootFields<'input>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an OpenAI image response object")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'input>,
    {
        let mut created = None;
        let mut data = None;
        let mut usage = None;
        while let Some(key) = map.next_key::<&'input str>()? {
            let value = map.next_value::<&'input RawValue>()?;
            match key {
                "created" => {
                    created = serde_json::from_str::<i64>(value.get())
                        .ok()
                        .filter(|created| *created >= 0);
                }
                "data" if data.replace(value).is_some() => {
                    return Err(serde::de::Error::custom(PayloadError));
                }
                "usage" => usage = parse_image_usage(value),
                _ => {}
            }
        }
        Ok(RootFields {
            created,
            data,
            usage,
        })
    }
}

struct DataVisitor;

impl<'input> Visitor<'input> for DataVisitor {
    type Value = Vec<&'input RawValue>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an OpenAI image data array")
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'input>,
    {
        let mut items = Vec::with_capacity(sequence.size_hint().unwrap_or(1).min(10));
        while let Some(item) = sequence.next_element::<&'input RawValue>()? {
            if items.len() == 10 {
                return Err(serde::de::Error::custom(PayloadError));
            }
            items.push(item);
        }
        Ok(items)
    }
}

struct RawItem<'input> {
    url: Option<&'input RawValue>,
    b64: Option<&'input RawValue>,
    revised_prompt: Option<String>,
}

impl RawItem<'_> {
    fn into_item(self, bytes: &Bytes) -> Result<OpenAiImageItem, OpenAiImageParseError> {
        let b64 = match self.b64 {
            Some(encoded)
                if serde_json::from_str::<&str>(encoded.get()).is_ok_and(str::is_empty) =>
            {
                None
            }
            value => value,
        };
        match (self.url, b64) {
            (Some(url), None) => {
                if url.get().len() > MAX_PROVIDER_URL_JSON_BYTES {
                    return Err(OpenAiImageParseError::InvalidPayload);
                }
                let url = serde_json::from_str::<String>(url.get())
                    .ok()
                    .filter(|url| !url.trim().is_empty())
                    .ok_or(OpenAiImageParseError::InvalidPayload)?;
                Ok(OpenAiImageItem::Url {
                    url,
                    revised_prompt: self.revised_prompt,
                })
            }
            (None, Some(encoded)) => {
                // `&str` deserialization succeeds only for an unescaped JSON string.
                // Base64 has no reason to contain escapes, so rejecting them prevents
                // an otherwise unavoidable second allocation for the largest field.
                let encoded = serde_json::from_str::<&str>(encoded.get())
                    .ok()
                    .filter(|encoded| is_valid_bounded_base64(encoded, MAX_IMAGE_RESPONSE))
                    .ok_or(OpenAiImageParseError::InvalidPayload)?;
                let base = bytes.as_ptr() as usize;
                let start = (encoded.as_ptr() as usize)
                    .checked_sub(base)
                    .filter(|start| start.saturating_add(encoded.len()) <= bytes.len())
                    .ok_or(OpenAiImageParseError::InvalidPayload)?;
                Ok(OpenAiImageItem::Base64 {
                    range: start..start + encoded.len(),
                    revised_prompt: self.revised_prompt,
                })
            }
            _ => Err(OpenAiImageParseError::InvalidPayload),
        }
    }
}

struct ItemVisitor;

impl<'input> Visitor<'input> for ItemVisitor {
    type Value = RawItem<'input>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an OpenAI image result object")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'input>,
    {
        let mut url = None;
        let mut b64 = None;
        let mut url_seen = false;
        let mut b64_seen = false;
        let mut revised_prompt = None;
        while let Some(key) = map.next_key::<&'input str>()? {
            let value = map.next_value::<&'input RawValue>()?;
            match key {
                "url" => {
                    if std::mem::replace(&mut url_seen, true) {
                        return Err(serde::de::Error::custom(PayloadError));
                    }
                    if value.get() != "null" {
                        url = Some(value);
                    }
                }
                "b64_json" => {
                    if std::mem::replace(&mut b64_seen, true) {
                        return Err(serde::de::Error::custom(PayloadError));
                    }
                    if value.get() != "null" {
                        b64 = Some(value);
                    }
                }
                "revised_prompt" => {
                    revised_prompt = (value.get().len() <= MAX_REVISED_PROMPT_JSON_BYTES)
                        .then(|| serde_json::from_str::<String>(value.get()).ok())
                        .flatten()
                        .filter(|prompt| {
                            prompt.len() <= MAX_REVISED_PROMPT_BYTES && !prompt.contains('\0')
                        });
                }
                _ => {}
            }
        }
        Ok(RawItem {
            url,
            b64,
            revised_prompt,
        })
    }
}

#[derive(Serialize)]
struct ArchivedAssetMetadata<'asset> {
    asset_id: uuid::Uuid,
    index: i64,
    mime_type: &'asset str,
    size_bytes: i64,
    filename: &'asset str,
}

#[derive(Serialize)]
struct SanitizedUrlItem<'asset> {
    url: String,
    archived_asset: ArchivedAssetMetadata<'asset>,
    #[serde(skip_serializing_if = "Option::is_none")]
    revised_prompt: Option<&'asset str>,
}

fn push_segment(
    segments: &mut Vec<Bytes>,
    total: &mut usize,
    segment: Bytes,
    limit: usize,
) -> Result<(), OpenAiImageBuildError> {
    *total = total
        .checked_add(segment.len())
        .filter(|length| *length <= limit)
        .ok_or(OpenAiImageBuildError::TooLarge)?;
    segments.push(segment);
    Ok(())
}

pub(super) fn build_openai_image_segments(
    bytes: Bytes,
    parsed: ParsedOpenAiImageResponse,
    request_id: uuid::Uuid,
    assets: &[ArchivedGenerationAsset],
    default_created: i64,
) -> Result<(Vec<Bytes>, usize), OpenAiImageBuildError> {
    build_openai_image_segments_with_limit(
        bytes,
        parsed,
        request_id,
        assets,
        default_created,
        MAX_IMAGE_RESPONSE,
    )
}

fn build_openai_image_segments_with_limit(
    bytes: Bytes,
    parsed: ParsedOpenAiImageResponse,
    request_id: uuid::Uuid,
    assets: &[ArchivedGenerationAsset],
    default_created: i64,
    limit: usize,
) -> Result<(Vec<Bytes>, usize), OpenAiImageBuildError> {
    let mut expected_asset_indexes = parsed
        .url_assets()
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let mut actual_asset_indexes = assets
        .iter()
        .map(|asset| usize::try_from(asset.index).ok())
        .collect::<Option<Vec<_>>>()
        .ok_or(OpenAiImageBuildError::InvalidAssets)?;
    expected_asset_indexes.sort_unstable();
    actual_asset_indexes.sort_unstable();
    if actual_asset_indexes != expected_asset_indexes {
        return Err(OpenAiImageBuildError::InvalidAssets);
    }
    let mut segments = Vec::with_capacity(parsed.items.len().saturating_mul(3).saturating_add(2));
    let mut total = 0;
    push_segment(
        &mut segments,
        &mut total,
        Bytes::from(format!(
            "{{\"created\":{},\"data\":[",
            parsed.created.unwrap_or(default_created)
        )),
        limit,
    )?;
    for (index, item) in parsed.items.into_iter().enumerate() {
        if index != 0 {
            push_segment(&mut segments, &mut total, Bytes::from_static(b","), limit)?;
        }
        match item {
            OpenAiImageItem::Base64 {
                range,
                revised_prompt,
            } => {
                push_segment(
                    &mut segments,
                    &mut total,
                    Bytes::from_static(b"{\"b64_json\":\""),
                    limit,
                )?;
                push_segment(&mut segments, &mut total, bytes.slice(range), limit)?;
                let mut suffix =
                    Vec::with_capacity(revised_prompt.as_ref().map_or(2, |value| value.len() + 24));
                suffix.push(b'"');
                if let Some(prompt) = revised_prompt {
                    suffix.extend_from_slice(b",\"revised_prompt\":");
                    serde_json::to_writer(&mut suffix, &prompt)
                        .map_err(|_| OpenAiImageBuildError::Internal)?;
                }
                suffix.push(b'}');
                push_segment(&mut segments, &mut total, Bytes::from(suffix), limit)?;
            }
            OpenAiImageItem::Url { revised_prompt, .. } => {
                let asset = assets
                    .iter()
                    .find(|asset| usize::try_from(asset.index).ok() == Some(index))
                    .ok_or(OpenAiImageBuildError::InvalidAssets)?;
                let item = SanitizedUrlItem {
                    url: format!("/self/v1/requests/{request_id}/assets/{}", asset.asset_id),
                    archived_asset: ArchivedAssetMetadata {
                        asset_id: asset.asset_id,
                        index: asset.index,
                        mime_type: &asset.mime_type,
                        size_bytes: asset.size_bytes,
                        filename: &asset.filename,
                    },
                    revised_prompt: revised_prompt.as_deref(),
                };
                let rendered =
                    serde_json::to_vec(&item).map_err(|_| OpenAiImageBuildError::Internal)?;
                push_segment(&mut segments, &mut total, Bytes::from(rendered), limit)?;
            }
        }
    }
    let mut suffix = Vec::with_capacity(256);
    suffix.push(b']');
    if let Some(usage) = parsed.usage {
        suffix.extend_from_slice(b",\"usage\":");
        serde_json::to_writer(&mut suffix, &usage).map_err(|_| OpenAiImageBuildError::Internal)?;
    }
    suffix.push(b'}');
    push_segment(&mut segments, &mut total, Bytes::from(suffix), limit)?;
    Ok((segments, total))
}

#[cfg(test)]
mod tests {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use serde_json::{Value, json};

    use super::*;

    fn body(value: Value) -> Bytes {
        Bytes::from(serde_json::to_vec(&value).expect("test JSON serializes"))
    }

    fn render(
        bytes: Bytes,
        count: i64,
        assets: &[ArchivedGenerationAsset],
    ) -> (Vec<Bytes>, Vec<u8>) {
        let parsed = parse_openai_image_response(&bytes, count).expect("payload parses");
        let (segments, _) =
            build_openai_image_segments(bytes, parsed, uuid::Uuid::nil(), assets, 99)
                .expect("segments build");
        let rendered = segments.clone().concat();
        (segments, rendered)
    }

    #[test]
    fn borrows_ten_base64_items_without_copying_their_contents() {
        let values = (0..10)
            .map(|index| json!({"b64_json": STANDARD.encode(format!("image-{index}"))}))
            .collect::<Vec<_>>();
        let bytes = body(json!({"data": values}));
        let parsed = parse_openai_image_response(&bytes, 10).expect("ten items parse");
        let expected_pointers = parsed
            .items
            .iter()
            .map(|item| {
                let OpenAiImageItem::Base64 { range, .. } = item else {
                    panic!("expected base64")
                };
                bytes[range.start..].as_ptr()
            })
            .collect::<Vec<_>>();
        let (segments, _) = build_openai_image_segments(bytes, parsed, uuid::Uuid::nil(), &[], 99)
            .expect("segments build");
        for pointer in expected_pointers {
            assert!(segments.iter().any(|segment| segment.as_ptr() == pointer));
        }
        let rendered = segments.concat();
        assert_eq!(
            serde_json::from_slice::<Value>(&rendered).unwrap()["data"]
                .as_array()
                .unwrap()
                .len(),
            10
        );
    }

    #[test]
    fn mixed_results_keep_the_original_url_asset_index() {
        let bytes = body(json!({"data": [
            {"b64_json": "b25l"},
            {"url": "https://provider.invalid/secret", "revised_prompt": "quoted \"prompt\""},
            {"b64_json": "dHdv"}
        ]}));
        let asset = ArchivedGenerationAsset {
            asset_id: uuid::Uuid::now_v7(),
            index: 1,
            object_locator: "staging/object".into(),
            mime_type: "image/png".into(),
            size_bytes: 3,
            filename: "asset-1.png".into(),
        };
        let (_, rendered) = render(bytes, 3, std::slice::from_ref(&asset));
        let value: Value = serde_json::from_slice(&rendered).unwrap();
        assert_eq!(value["data"][1]["archived_asset"]["index"], 1);
        assert_eq!(value["data"][1]["revised_prompt"], "quoted \"prompt\"");
        let text = String::from_utf8(rendered).unwrap();
        assert!(!text.contains("provider.invalid"));
    }

    #[test]
    fn rejects_xor_null_wrong_types_bad_padding_and_escaped_base64() {
        for invalid in [
            br#"{"data":[{}]}"#.as_slice(),
            br#"{"data":[{"url":null}]}"#,
            br#"{"data":[{"b64_json":7}]}"#,
            br#"{"data":[{"url":"https://example.test/a","b64_json":"cG5n"}]}"#,
            br#"{"data":[{"b64_json":"Y=Jj"}]}"#,
            br#"{"data":[{"b64_json":"cG\u0035n"}]}"#,
        ] {
            assert_eq!(
                parse_openai_image_response(&Bytes::copy_from_slice(invalid), 1).err(),
                Some(OpenAiImageParseError::InvalidPayload)
            );
        }
    }

    #[test]
    fn explicit_null_is_compatible_with_the_other_valid_variant() {
        for response in [
            br#"{"data":[{"url":null,"b64_json":"cG5n"}]}"#.as_slice(),
            br#"{"data":[{"url":"https://example.test/image","b64_json":null}]}"#,
        ] {
            parse_openai_image_response(&Bytes::copy_from_slice(response), 1)
                .expect("null is equivalent to an absent alternate field");
        }
    }

    #[test]
    fn empty_base64_is_compatible_with_a_valid_url() {
        let response =
            Bytes::from_static(br#"{"data":[{"url":"https://example.test/image","b64_json":""}]}"#);
        let parsed = parse_openai_image_response(&response, 1)
            .expect("an empty alternate representation is equivalent to absence");
        assert!(matches!(
            parsed.items.as_slice(),
            [OpenAiImageItem::Url { .. }]
        ));
    }

    #[test]
    fn rejects_oversized_provider_url_before_allocating_it() {
        let url = format!(
            "https://example.test/{}",
            "x".repeat(MAX_PROVIDER_URL_JSON_BYTES)
        );
        let bytes = body(json!({"data": [{"url": url}]}));
        assert_eq!(
            parse_openai_image_response(&bytes, 1).err(),
            Some(OpenAiImageParseError::InvalidPayload)
        );
    }

    #[test]
    fn malformed_json_is_classified_separately() {
        assert_eq!(
            parse_openai_image_response(&Bytes::from_static(br#"{"data":["#), 1).err(),
            Some(OpenAiImageParseError::InvalidJson)
        );
    }

    #[test]
    fn whitelists_created_usage_prompt_and_drops_secrets() {
        let bytes = body(json!({
            "created": 42,
            "provider_url": "https://provider.invalid/SECRET",
            "data": [{
                "b64_json": "cG5n",
                "revised_prompt": "line\n\"quoted\"",
                "provider_secret": "SECRET"
            }],
            "usage": {
                "total_tokens": 7,
                "input_tokens_details": {"image_tokens": 3, "secret": "SECRET"},
                "secret": "SECRET"
            }
        }));
        let (_, rendered) = render(bytes, 1, &[]);
        let value: Value = serde_json::from_slice(&rendered).unwrap();
        assert_eq!(value["created"], 42);
        assert_eq!(
            value["usage"],
            json!({"total_tokens": 7, "input_tokens_details": {"image_tokens": 3}})
        );
        assert_eq!(value["data"][0]["revised_prompt"], "line\n\"quoted\"");
        assert!(!String::from_utf8(rendered).unwrap().contains("SECRET"));
    }

    #[test]
    fn invalid_revised_prompts_are_omitted() {
        for prompt in ["x\0y".to_owned(), "x".repeat(MAX_REVISED_PROMPT_BYTES + 1)] {
            let bytes = body(json!({"data": [{"b64_json": "cG5n", "revised_prompt": prompt}]}));
            let (_, rendered) = render(bytes, 1, &[]);
            assert!(
                serde_json::from_slice::<Value>(&rendered).unwrap()["data"][0]
                    .get("revised_prompt")
                    .is_none()
            );
        }
    }

    #[test]
    fn exact_response_cap_passes_and_one_more_byte_fails() {
        let bytes = body(json!({"data": [{"b64_json": "cG5n"}]}));
        let parsed = parse_openai_image_response(&bytes, 1).unwrap();
        let (_, exact) = build_openai_image_segments_with_limit(
            bytes.clone(),
            parsed,
            uuid::Uuid::nil(),
            &[],
            0,
            usize::MAX,
        )
        .unwrap();
        let parsed = parse_openai_image_response(&bytes, 1).unwrap();
        assert!(
            build_openai_image_segments_with_limit(
                bytes.clone(),
                parsed,
                uuid::Uuid::nil(),
                &[],
                0,
                exact
            )
            .is_ok()
        );
        let parsed = parse_openai_image_response(&bytes, 1).unwrap();
        assert!(
            build_openai_image_segments_with_limit(
                bytes,
                parsed,
                uuid::Uuid::nil(),
                &[],
                0,
                exact - 1
            )
            .is_err()
        );
    }

    #[test]
    fn archive_and_first_response_segments_are_byte_identical() {
        let bytes = body(json!({"data": [{"b64_json": "cG5n"}], "usage": {"output_tokens": 1}}));
        let (segments, first_response) = render(bytes, 1, &[]);
        let archived = segments.concat();
        assert_eq!(archived, first_response);
        serde_json::from_slice::<Value>(&archived).expect("segments are valid JSON");
    }

    #[test]
    fn rejects_duplicate_or_wrong_asset_indexes() {
        let bytes = body(json!({"data": [
            {"url": "https://example.test/one"},
            {"url": "https://example.test/two"}
        ]}));
        let parsed = parse_openai_image_response(&bytes, 2).unwrap();
        let asset = |index| ArchivedGenerationAsset {
            asset_id: uuid::Uuid::now_v7(),
            index,
            object_locator: format!("staging/{index}"),
            mime_type: "image/png".into(),
            size_bytes: 3,
            filename: format!("asset-{index}.png"),
        };
        assert_eq!(
            build_openai_image_segments(
                bytes,
                parsed,
                uuid::Uuid::nil(),
                &[asset(0), asset(0)],
                0,
            )
            .err(),
            Some(OpenAiImageBuildError::InvalidAssets)
        );
    }
}
