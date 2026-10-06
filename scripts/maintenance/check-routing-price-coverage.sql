BEGIN TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY;
SET LOCAL statement_timeout = '60s';
SET LOCAL lock_timeout = '2s';
SET LOCAL idle_in_transaction_session_timeout = '10s';

WITH configured_candidates AS (
    SELECT tenant_id, model_route_id, upstream_account_id, upstream_model
    FROM model_route_upstream_accounts
    UNION
    SELECT included.tenant_id, included.model_route_id,
           member.upstream_account_id, route.upstream_model
    FROM model_route_included_provider_groups included
    JOIN upstream_account_provider_groups member
      ON member.tenant_id = included.tenant_id
     AND member.provider_group_id = included.provider_group_id
    JOIN model_routes route
      ON route.tenant_id = included.tenant_id AND route.id = included.model_route_id
), candidates AS (
    SELECT candidate.*, route.public_model
    FROM configured_candidates candidate
    JOIN model_routes route
      ON route.tenant_id = candidate.tenant_id AND route.id = candidate.model_route_id
    JOIN tenants tenant ON tenant.id = route.tenant_id AND tenant.status = 'active'
    JOIN upstream_accounts account
      ON account.tenant_id = candidate.tenant_id AND account.id = candidate.upstream_account_id
     AND account.status = 'active'
    WHERE route.enabled = 1 AND route.archived_at IS NULL AND route.protocol <> 'generation'
      AND NOT EXISTS (
          SELECT 1 FROM model_route_excluded_provider_groups excluded
          JOIN upstream_account_provider_groups blocked
            ON blocked.tenant_id = excluded.tenant_id
           AND blocked.provider_group_id = excluded.provider_group_id
          WHERE excluded.tenant_id = candidate.tenant_id
            AND excluded.model_route_id = candidate.model_route_id
            AND blocked.upstream_account_id = candidate.upstream_account_id
      )
), currencies AS (
    SELECT tenant_id, upper(currency) AS currency FROM key_records
    UNION
    SELECT tenant_id, upper(currency) FROM credit_accounts
    UNION
    SELECT tenant.id, extra.currency
    FROM tenants tenant
    CROSS JOIN jsonb_array_elements_text(:'currencies'::jsonb) extra(currency)
), checked AS (
    SELECT candidate.tenant_id, candidate.model_route_id, candidate.upstream_account_id,
           currency.currency,
           EXISTS (SELECT 1 FROM model_prices alias_price
                   WHERE alias_price.model = candidate.public_model
                     AND alias_price.currency = currency.currency) AS public_alias_price_present,
           CASE
             WHEN currency.currency IS NULL THEN 'currency_scope_unknown'
             WHEN currency.currency NOT IN ('USD', 'CNY') THEN 'invalid_currency'
             WHEN price.id IS NULL THEN 'missing_actual_price'
             WHEN NOT (
                 price.id ~* '^[0-9a-f]{32}$'
                 OR price.id ~* '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
                 OR price.id ~ '^urn:uuid:[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$'
                 OR price.id ~* '^\{[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\}$'
             ) OR price.input_micros_per_million < 0 OR price.output_micros_per_million < 0
             OR EXISTS (
                 SELECT 1 FROM model_price_tiers tier
                 WHERE tier.model = candidate.upstream_model AND tier.currency = currency.currency
                   AND (tier.input_micros_per_million < 0 OR tier.output_micros_per_million < 0
                        OR tier.cached_input_micros_per_million < 0 OR tier.cache_write_micros_per_million < 0)
             ) THEN 'invalid_actual_price'
             ELSE NULL
           END AS reason
    FROM candidates candidate
    LEFT JOIN currencies currency ON currency.tenant_id = candidate.tenant_id
    LEFT JOIN model_prices price
      ON price.model = candidate.upstream_model AND price.currency = currency.currency
), gaps AS (
    SELECT tenant_id, model_route_id AS route_id, upstream_account_id,
           CASE WHEN currency IN ('USD', 'CNY') THEN currency ELSE NULL END AS currency,
           reason, public_alias_price_present
    FROM checked WHERE reason IS NOT NULL
), samples AS (
    SELECT * FROM gaps ORDER BY tenant_id, route_id, upstream_account_id, currency LIMIT 200
)
SELECT jsonb_build_object(
    'contract', '474-actual-upstream-price-v1',
    'checked_at', CURRENT_TIMESTAMP,
    'read_only', current_setting('transaction_read_only') = 'on',
    'candidate_count', (SELECT count(*) FROM candidates),
    'checked_pair_count', (SELECT count(*) FROM checked),
    'gap_count', (SELECT count(*) FROM gaps),
    'gap_samples', COALESCE((SELECT jsonb_agg(to_jsonb(samples)) FROM samples), '[]'::jsonb),
    'samples_truncated', (SELECT count(*) FROM gaps) > 200
);
ROLLBACK;
