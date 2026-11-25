//! Synthetic single-PUT transport that separates remote backing from caller RAM.
//! Multipart is deliberately delegated unchanged; these fixtures use 64KiB PUTs.
use std::fmt;

use bytes::Bytes;
use futures_util::stream::BoxStream;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMultipartOptions, PutOptions, PutPayload, PutResult, Result, memory::InMemory, path::Path,
};

#[derive(Debug, Default)]
pub(super) struct SinglePutMemoryTransport(InMemory);

impl fmt::Display for SinglePutMemoryTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("single-put synthetic transport")
    }
}

#[async_trait::async_trait]
impl ObjectStore for SinglePutMemoryTransport {
    async fn put_opts(
        &self,
        path: &Path,
        payload: PutPayload,
        opts: PutOptions,
    ) -> Result<PutResult> {
        let detached: PutPayload = payload
            .iter()
            .map(|bytes| Bytes::copy_from_slice(bytes))
            .collect();
        drop(payload);
        self.0.put_opts(path, detached, opts).await
    }

    async fn put_multipart_opts(
        &self,
        path: &Path,
        opts: PutMultipartOptions,
    ) -> Result<Box<dyn MultipartUpload>> {
        self.0.put_multipart_opts(path, opts).await
    }

    async fn get_opts(&self, path: &Path, opts: GetOptions) -> Result<GetResult> {
        self.0.get_opts(path, opts).await
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, Result<Path>>,
    ) -> BoxStream<'static, Result<Path>> {
        self.0.delete_stream(locations)
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, Result<ObjectMeta>> {
        self.0.list(prefix)
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> Result<ListResult> {
        self.0.list_with_delimiter(prefix).await
    }

    async fn copy_opts(&self, from: &Path, to: &Path, opts: CopyOptions) -> Result<()> {
        self.0.copy_opts(from, to, opts).await
    }
}
