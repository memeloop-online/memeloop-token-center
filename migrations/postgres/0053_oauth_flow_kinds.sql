ALTER TABLE oauth_login_sessions
    DROP CONSTRAINT IF EXISTS oauth_login_sessions_flow_kind_check;

ALTER TABLE oauth_login_sessions
    ADD CONSTRAINT oauth_login_sessions_flow_kind_check
    CHECK (flow_kind IN (
        'openai_codex_device',
        'cursor_pkce',
        'provider_adapter_cursor_pkce',
        'claude_manual_pkce',
        'github_copilot_device'
    ));
