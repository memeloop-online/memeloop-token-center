-- Keep the request list on narrow B-tree pages.  Observation rows may carry
-- large replay metadata in TOAST/overflow storage; the list needs only these
-- four labels and must not visit the wide heap once per visible request.
--
-- PostgreSQL uses INCLUDE columns for the projection.  SQLite has no INCLUDE
-- syntax, so the same portable statement places the projection after the join
-- keys.  `request_id` is unique, therefore the extra key columns do not change
-- lookup cardinality or request isolation.
CREATE INDEX IF NOT EXISTS conversation_observations_request_list_cover_idx
    ON conversation_observations (
        request_id,
        key_id,
        cluster_id,
        session_name,
        task_kind,
        agent_id,
        metadata_source
    );

-- Price pages are ordered within one currency.  The original unique key starts
-- with `model`, which forces the database to filter every currency while it
-- walks model order.  This inverse key supports bounded/keyset pages directly.
CREATE INDEX IF NOT EXISTS model_prices_currency_model_idx
    ON model_prices (currency, model);
