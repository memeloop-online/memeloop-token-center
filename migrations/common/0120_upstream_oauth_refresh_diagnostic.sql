ALTER TABLE upstream_credentials ADD COLUMN oauth_refresh_diagnostic_json TEXT
    CHECK (oauth_refresh_diagnostic_json IS NULL OR LENGTH(oauth_refresh_diagnostic_json) BETWEEN 2 AND 2048);
