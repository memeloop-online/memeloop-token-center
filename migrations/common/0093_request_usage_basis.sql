-- Historical values stay unknown; never infer provider usage from a status or
-- rewrite prior financial settlements. The marker is terminal-transaction owned.
ALTER TABLE request_records ADD COLUMN usage_basis TEXT
    CHECK (usage_basis IN ('provider_reported', 'provider_estimated', 'contract_ceiling', 'not_observed'));
ALTER TABLE account_settlement_feed ADD COLUMN usage_basis TEXT
    CHECK (usage_basis IN ('provider_reported', 'provider_estimated', 'contract_ceiling', 'not_observed'));
