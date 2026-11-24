//! Results live only for one materialization page and one archive backend.
use super::*;

#[derive(Default)]
pub(crate) struct SummaryBatch {
    values: std::collections::BTreeMap<(Uuid, Uuid), Option<crate::supplier_error::SupplierError>>,
}
impl SummaryBatch {
    pub(crate) async fn get_or_load<F, Fut>(
        &mut self,
        tenant: Uuid,
        request: Uuid,
        load: F,
    ) -> Result<Option<crate::supplier_error::SupplierError>, AppError>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<
                Output = Result<Option<crate::supplier_error::SupplierError>, AppError>,
            >,
    {
        if let Some(value) = self.values.get(&(tenant, request)) {
            return Ok(value.clone());
        }
        let value = load().await?;
        self.values.insert((tenant, request), value.clone());
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn summary_batch_reuses_absence_and_keeps_scope_and_errors() {
        let mut batch = SummaryBatch::default();
        let tenant = Uuid::now_v7();
        let request = Uuid::now_v7();
        let calls = std::cell::Cell::new(0);
        for _ in 0..64 {
            assert!(
                batch
                    .get_or_load(tenant, request, || async {
                        calls.set(calls.get() + 1);
                        Ok(None)
                    })
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        assert_eq!(calls.get(), 1);
        assert!(
            batch
                .get_or_load(Uuid::now_v7(), request, || async {
                    Err(AppError::storage("fixture_failure"))
                })
                .await
                .is_err()
        );
        let other = Uuid::now_v7();
        assert!(
            batch
                .get_or_load(tenant, other, || async {
                    Err(AppError::storage("fixture_failure"))
                })
                .await
                .is_err()
        );
        batch
            .get_or_load(tenant, other, || async {
                calls.set(calls.get() + 1);
                Ok(None)
            })
            .await
            .unwrap();
        assert_eq!(calls.get(), 2);
    }
}
