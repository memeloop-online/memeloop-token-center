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

export interface KeyView {
  key_id: string;
  alias: string;
  currency: string;
  credential_generation: number;
  created_at: number;
  policy: {
    allowed_models: string[];
    requests_per_minute: number;
    tokens_per_minute: number;
    max_concurrency: number;
    daily_budget: string | null;
    weekly_budget: string | null;
    lifetime_budget: string | null;
  };
  available_balance: string;
}

export interface ConversationCluster {
  cluster_id: string;
  explicit_session_id: string | null;
  updated_at: number;
  request_count: number;
  candidate_edge_count: number;
}

export interface ConversationEdge {
  from_request_id: string | null;
  to_request_id: string;
  relation: 'continues' | 'retry' | 'edit' | 'branch' | 'compacts' | 'subagent' | 'candidate';
  confidence: number;
  evidence: unknown;
}

export interface ConversationDetail {
  cluster: ConversationCluster;
  requests: RequestView[];
  edges: ConversationEdge[];
}

export interface GenerationJob {
  job_id: string;
  created_at: number;
  updated_at: number;
  completed_at: number | null;
  model: string;
  driver: string;
  status: 'queued' | 'running' | 'succeeded' | 'failed' | 'cancelled';
  upstream_job_id: string | null;
  estimated_units: number;
  billed_units: number | null;
  cost: string;
  error_code: string | null;
  result: unknown | null;
}

export interface ProviderType {
  id: string;
  display_name: string;
  protocols: string[];
  modalities: string[];
  config_schema: Record<string, unknown>;
  credential_schema: Record<string, unknown>;
  oauth_adapter?: {
    login_url: string;
    poll_url: string;
    refresh_url: string;
  };
  source: string;
}

export interface PluginManifest {
  id: string;
  version: string;
  wit_version: string;
  capabilities: Array<
    | { kind: 'log' | 'kv' }
    | { kind: 'http'; allowed_origins: string[] }
  >;
  contributions: {
    traffic_policy?: boolean;
    providers?: ProviderType[];
  };
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
  plugin_manifest: Record<string, unknown>;
  provider_account: Record<string, unknown>;
  service_token: Record<string, unknown>;
  generation_create: Record<string, unknown>;
  generation_price: Record<string, unknown>;
  model_price: Record<string, unknown>;
}
