use bytes::Bytes;
use serde::Deserialize;

use super::super::MAX_IMAGE_RESPONSE;
use super::{
    responses_tool_image::{ParsedResponsesToolImage, ResponsesToolImageParseError},
    synchronous_image::is_valid_bounded_base64,
};

#[derive(Deserialize)]
struct Envelope<'a> {
    #[serde(borrow)]
    response: NativeResponse<'a>,
}

#[derive(Deserialize)]
struct NativeResponse<'a> {
    #[serde(borrow)]
    candidates: Vec<Candidate<'a>>,
}

#[derive(Deserialize)]
struct Candidate<'a> {
    #[serde(borrow)]
    content: Content<'a>,
}

#[derive(Deserialize)]
struct Content<'a> {
    #[serde(borrow)]
    parts: Vec<Part<'a>>,
}

#[derive(Deserialize)]
struct Part<'a> {
    #[serde(borrow, rename = "inlineData", alias = "inline_data")]
    inline: Option<Inline<'a>>,
}

#[derive(Deserialize)]
struct Inline<'a> {
    #[serde(rename = "mimeType", alias = "mime_type")]
    mime: &'a str,
    data: &'a str,
}

/// Borrows base64 directly from the bounded upstream body. The common response
/// staging/settlement path streams this range without a decoded/re-encoded copy.
pub(super) fn parse(
    bytes: &Bytes,
) -> Result<ParsedResponsesToolImage, ResponsesToolImageParseError> {
    let envelope: Envelope<'_> = serde_json::from_slice(bytes).map_err(|error| {
        if error.is_syntax() || error.is_eof() {
            ResponsesToolImageParseError::InvalidJson
        } else {
            ResponsesToolImageParseError::InvalidPayload
        }
    })?;
    let mut image = None;
    for candidate in envelope.response.candidates {
        for part in candidate.content.parts {
            let Some(inline) = part.inline else { continue };
            if image.is_some()
                || !matches!(inline.mime, "image/png" | "image/jpeg" | "image/webp")
                || !is_valid_bounded_base64(inline.data, MAX_IMAGE_RESPONSE)
            {
                return Err(ResponsesToolImageParseError::InvalidPayload);
            }
            image = Some(inline.data);
        }
    }
    let image = image.ok_or(ResponsesToolImageParseError::InvalidPayload)?;
    let start = (image.as_ptr() as usize)
        .checked_sub(bytes.as_ptr() as usize)
        .filter(|start| start.saturating_add(image.len()) <= bytes.len())
        .ok_or(ResponsesToolImageParseError::InvalidPayload)?;
    Ok(ParsedResponsesToolImage {
        image_range: start..start + image.len(),
        usage: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn borrows_one_image_and_rejects_duplicate_payload() {
        let bytes = Bytes::from_static(br#"{"response":{"candidates":[{"content":{"parts":[{"text":"ok"},{"inlineData":{"mimeType":"image/png","data":"aW1hZ2U="}}]}}]}}"#);
        let parsed = parse(&bytes).unwrap();
        assert_eq!(&bytes[parsed.image_range], b"aW1hZ2U=");
        let invalid = Bytes::from_static(br#"{"response":{"candidates":[{"content":{"parts":[{"inlineData":{"mimeType":"image/png","data":"aW1hZ2U="}},{"inlineData":{"mimeType":"image/png","data":"aW1hZ2U="}}]}}]}}"#);
        assert!(parse(&invalid).is_err());
    }
}
