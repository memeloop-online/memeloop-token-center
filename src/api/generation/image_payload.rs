use std::ops::Range;

use bytes::Bytes;

use super::super::MAX_IMAGE_RESPONSE;
use super::{
    responses_tool_image::SanitizedImageUsage, synchronous_image::is_valid_bounded_base64,
};

pub(super) struct ParsedSingleImage {
    pub(super) image_range: Range<usize>,
    pub(super) usage: Option<SanitizedImageUsage>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ImageParseError {
    InvalidJson,
    InvalidPayload,
}

#[derive(Default)]
pub(super) struct SingleImageScan<'input> {
    image: Option<&'input str>,
}

impl<'input> SingleImageScan<'input> {
    pub(super) fn record(&mut self, image: &'input str) -> Result<(), ImageParseError> {
        if self.image.is_some() || !is_valid_bounded_base64(image, MAX_IMAGE_RESPONSE) {
            return Err(ImageParseError::InvalidPayload);
        }
        self.image = Some(image);
        Ok(())
    }

    pub(super) fn finish(
        self,
        bytes: &'input Bytes,
        usage: Option<SanitizedImageUsage>,
    ) -> Result<ParsedSingleImage, ImageParseError> {
        let image = self.image.ok_or(ImageParseError::InvalidPayload)?;
        Ok(ParsedSingleImage {
            image_range: borrowed_image_range(bytes, image)?,
            usage,
        })
    }
}

impl ImageParseError {
    pub(super) fn from_json(error: serde_json::Error) -> Self {
        match error.classify() {
            serde_json::error::Category::Syntax | serde_json::error::Category::Eof => {
                Self::InvalidJson
            }
            serde_json::error::Category::Data | serde_json::error::Category::Io => {
                Self::InvalidPayload
            }
        }
    }
}

pub(super) fn borrowed_image_range(
    bytes: &Bytes,
    encoded: &str,
) -> Result<Range<usize>, ImageParseError> {
    let start = (encoded.as_ptr() as usize)
        .checked_sub(bytes.as_ptr() as usize)
        .filter(|start| start.saturating_add(encoded.len()) <= bytes.len())
        .ok_or(ImageParseError::InvalidPayload)?;
    Ok(start..start + encoded.len())
}
