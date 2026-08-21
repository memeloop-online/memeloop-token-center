-- Existing explicit route/account choices predate model discovery and remain
-- usable for migration continuity. New associations must opt into a policy at
-- insertion time, and the control API writes `required` by default.
ALTER TABLE model_route_upstream_accounts
    ADD COLUMN catalog_policy TEXT NOT NULL DEFAULT 'explicit_custom'
    CHECK (catalog_policy IN ('required', 'explicit_custom'));

CREATE INDEX model_route_upstream_accounts_catalog_policy_idx
    ON model_route_upstream_accounts
       (tenant_id, model_route_id, catalog_policy, upstream_account_id);

CREATE VIEW model_route_eligible_upstream_accounts AS
SELECT candidate.tenant_id,
       candidate.model_route_id,
       candidate.upstream_account_id,
       candidate.upstream_model,
       candidate.scheduling_weight
FROM (
    SELECT direct.tenant_id,
           direct.model_route_id,
           direct.upstream_account_id,
           direct.upstream_model,
           direct.scheduling_weight
    FROM model_route_upstream_accounts direct
    JOIN model_routes route
      ON route.tenant_id = direct.tenant_id
     AND route.id = direct.model_route_id
    WHERE direct.catalog_policy = 'explicit_custom'
       OR EXISTS (
           SELECT 1
           FROM upstream_model_catalog_state catalog
           JOIN upstream_accounts account
             ON account.tenant_id = catalog.tenant_id
            AND account.id = catalog.upstream_account_id
            AND account.credential_generation = catalog.credential_generation
           JOIN upstream_model_catalog_snapshots snapshot
             ON snapshot.tenant_id = catalog.tenant_id
            AND snapshot.upstream_account_id = catalog.upstream_account_id
            AND snapshot.id = catalog.current_snapshot_id
            AND snapshot.credential_generation = account.credential_generation
           JOIN upstream_models model
             ON model.tenant_id = catalog.tenant_id
            AND model.upstream_account_id = catalog.upstream_account_id
            AND model.snapshot_id = catalog.current_snapshot_id
           WHERE catalog.tenant_id = direct.tenant_id
             AND catalog.upstream_account_id = direct.upstream_account_id
             AND catalog.status IN ('ready', 'stale', 'syncing')
             AND model.model_id = direct.upstream_model
             AND (model.protocol = 'any' OR model.protocol = route.protocol)
       )
    UNION ALL
    SELECT included.tenant_id,
           included.model_route_id,
           member.upstream_account_id,
           route.upstream_model,
           100 AS scheduling_weight
    FROM model_route_included_provider_groups included
    JOIN upstream_account_provider_groups member
      ON member.tenant_id = included.tenant_id
     AND member.provider_group_id = included.provider_group_id
    JOIN model_routes route
      ON route.tenant_id = included.tenant_id
     AND route.id = included.model_route_id
    JOIN upstream_model_catalog_state catalog
      ON catalog.tenant_id = member.tenant_id
     AND catalog.upstream_account_id = member.upstream_account_id
     AND catalog.current_snapshot_id IS NOT NULL
     AND catalog.status IN ('ready', 'stale', 'syncing')
    JOIN upstream_accounts account
      ON account.tenant_id = catalog.tenant_id
     AND account.id = catalog.upstream_account_id
     AND account.credential_generation = catalog.credential_generation
    JOIN upstream_model_catalog_snapshots snapshot
      ON snapshot.tenant_id = catalog.tenant_id
     AND snapshot.upstream_account_id = catalog.upstream_account_id
     AND snapshot.id = catalog.current_snapshot_id
     AND snapshot.credential_generation = account.credential_generation
    JOIN upstream_models model
      ON model.tenant_id = catalog.tenant_id
     AND model.upstream_account_id = catalog.upstream_account_id
     AND model.snapshot_id = catalog.current_snapshot_id
     AND model.model_id = route.upstream_model
     AND (model.protocol = 'any' OR model.protocol = route.protocol)
) candidate
WHERE NOT EXISTS (
    SELECT 1
    FROM model_route_excluded_provider_groups excluded
    JOIN upstream_account_provider_groups blocked
      ON blocked.tenant_id = excluded.tenant_id
     AND blocked.provider_group_id = excluded.provider_group_id
    WHERE excluded.tenant_id = candidate.tenant_id
      AND excluded.model_route_id = candidate.model_route_id
      AND blocked.upstream_account_id = candidate.upstream_account_id
);
