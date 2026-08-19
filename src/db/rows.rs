use sqlx::{Row, any::AnyRow};

use super::parse_uuid;
use crate::{
    error::AppError,
    model::{GenerationAssetDownload, GenerationAssetView},
};

pub(super) fn generation_asset_download(row: AnyRow) -> Result<GenerationAssetDownload, AppError> {
    Ok(GenerationAssetDownload {
        view: GenerationAssetView {
            asset_id: parse_uuid(row.try_get("id")?)?,
            index: row.try_get("asset_index")?,
            mime_type: row.try_get("mime_type")?,
            size_bytes: row.try_get("size_bytes")?,
            filename: row.try_get("filename")?,
        },
        object_locator: row.try_get("object_locator")?,
    })
}
