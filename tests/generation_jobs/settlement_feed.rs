use memeloop_token_center::{db::Database, model::AuthenticatedKey};
use uuid::Uuid;

pub(super) async fn assert_zero_settlement(
    database: &Database,
    key: &AuthenticatedKey,
    request_id: Uuid,
) -> Uuid {
    let page = database
        .list_account_settlements(key.account_id, 100, None, None)
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert!(page.next_cursor.is_none());
    let row = &page.items[0];
    assert_eq!(row.request_id, request_id);
    assert_eq!(row.key_id, key.key_id);
    assert_eq!(row.account_id, key.account_id);
    assert_eq!(row.settlement_sequence, 1);
    assert_eq!(row.cost, "0");
    assert_eq!(
        row.kind,
        memeloop_token_center::model::AccountSettlementKind::Generation
    );
    assert!(row.input_tokens.is_none() && row.output_tokens.is_none());
    row.settlement_id
}
