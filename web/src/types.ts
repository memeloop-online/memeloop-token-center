export interface RequestView {
  request_id: string;
  created_at: number;
  protocol: string;
  model: string;
  status_code: number | null;
  duration_ms: number | null;
  input_tokens: number;
  output_tokens: number;
  cost: string;
  error_code: string | null;
}

export interface RequestEvent {
  event_id: string;
  request_id: string;
  event_at: number;
  event_kind: 'started' | 'finished';
  key_id: string;
  protocol: string;
  model: string;
  status_code: number | null;
  duration_ms: number | null;
  input_tokens: number;
  output_tokens: number;
  cost: string;
  error_code: string | null;
}

export interface StatsBucket {
  name: string;
  requests: number;
  input_tokens: number;
  output_tokens: number;
  cost: string;
}

export interface SelfStats {
  key_id: string;
  summary: {
    total_requests: number;
    successful_requests: number;
    failed_requests: number;
    input_tokens: number;
    output_tokens: number;
    total_cost: string;
  };
  by_model: StatsBucket[];
  by_day: StatsBucket[];
  errors: StatsBucket[];
}

export interface ProviderType {
  id: string;
  display_name: string;
  protocols: string[];
  modalities: string[];
  config_schema: Record<string, unknown>;
  credential_schema: Record<string, unknown>;
  source: string;
}

export interface UpstreamAccount {
  id: string;
  name: string;
  driver: string;
  auth_kind: string;
  credential_generation: number;
  status: string;
  credential_expires_at: number | null;
  config: Record<string, unknown>;
}

export interface ConfigurationSchemas {
  core_config: Record<string, unknown>;
  key_create: Record<string, unknown>;
  key_policy: Record<string, unknown>;
  model_route: Record<string, unknown>;
  provider_account: Record<string, unknown>;
  service_token: Record<string, unknown>;
}
