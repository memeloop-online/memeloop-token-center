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
  currency?: string | null;
  error_code: string | null;
}

export interface RequestDetail extends RequestView {
  request_body: unknown;
  response_body: unknown;
  archive_complete: boolean;
  provenance?: {
    source: string;
    disposition: 'exact' | 'unlinked';
    unlinked: boolean;
    external_request_id: string;
    proof_digest: string;
  };
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
  cost: string | null;
  costs: UsageAnalysisCost[];
}

export interface UsageAnalysisCost {
  currency: string;
  cost: string;
}

export interface UsageAnalysisGenerationUnitByModality {
  modality: 'image' | 'video' | string;
  currency: string;
  units: number;
}

export interface UsageAnalysisGenerationUnitByBillingUnit {
  billing_unit: 'job' | 'second' | 'image' | 'megapixel' | string;
  currency: string;
  units: number;
}

export interface UsageAnalysisMetrics {
  requests: number;
  success: number;
  failed: number;
  input_tokens: number;
  output_tokens: number;
  cached_input_tokens: number;
  cache_write_tokens: number;
  generation_units: number;
  avg_duration_ms: number | null;
  p95_duration_ms: number | null;
  costs: UsageAnalysisCost[];
}

export interface UsageAnalysisBucket extends UsageAnalysisMetrics {
  id: string;
  label: string;
}

export interface UsageAnalysisSessionBucket extends UsageAnalysisBucket {
  key_id: string;
  key_alias: string;
  unlinked: boolean;
}

export interface UsageAnalysisTimeBucket extends UsageAnalysisMetrics {
  bucket_start: number;
}

export interface UsageAnalysisHeatmapBucket extends UsageAnalysisMetrics {
  hour_of_week: number;
}

export interface SelfStats {
  key_id: string;
  summary: {
    total_requests: number;
    successful_requests: number;
    failed_requests: number;
    input_tokens: number;
    output_tokens: number;
    total_cost: string | null;
    costs: UsageAnalysisCost[];
  };
  by_model: StatsBucket[];
  by_day: StatsBucket[];
  errors: StatsBucket[];
}

export interface OperatorStats {
  summary: SelfStats['summary'];
  by_model: StatsBucket[];
  by_day: StatsBucket[];
  errors: StatsBucket[];
}

export interface OperatorUsageAnalysis {
  from_created_at: number;
  to_created_at: number;
  granularity: 'hour' | 'day';
  time_zone: 'UTC';
  p95_is_approximate: boolean;
  p95_method: string;
  upstream_grouping: 'stable_account';
  summary: UsageAnalysisMetrics;
  generation_units_by_modality?: UsageAnalysisGenerationUnitByModality[];
  generation_units_by_billing_unit?: UsageAnalysisGenerationUnitByBillingUnit[];
  time_series: UsageAnalysisTimeBucket[];
  by_model: UsageAnalysisBucket[];
  by_key: UsageAnalysisBucket[];
  by_session: UsageAnalysisSessionBucket[];
  by_upstream: UsageAnalysisBucket[];
  by_protocol: UsageAnalysisBucket[];
  by_status: UsageAnalysisBucket[];
  errors: UsageAnalysisBucket[];
  heatmap: UsageAnalysisHeatmapBucket[];
}

export interface TenantView { external_id: string }

export interface ModelPriceView {
  model: string;
  currency: string;
  input_per_million: string;
  output_per_million: string;
  source: 'manual' | 'models.dev' | 'litellm' | 'openrouter' | string;
  updated_at: number;
  tiers: Array<{
    service_tier: string;
    input_per_million: string;
    cached_input_per_million: string;
    cache_write_per_million: string;
    output_per_million: string;
    source: 'manual' | 'models.dev' | 'litellm' | 'openrouter' | string;
    updated_at: number;
    cache_price_estimated: boolean;
  }>;
}

export interface ModelPriceUsageSummary {
  models: Array<{ model: string; calls: number; input_tokens: number; output_tokens: number }>;
}

export interface ModelPriceSyncResult {
  source: string;
  sources: string[];
  imported: number;
  matched: string[];
  candidates: Array<{ model: string; candidates: Array<{ sourceModelId: string; source: string; reason: string; inputPerMillion: string; outputPerMillion: string; serviceTier: string }> }>;
  unmatched: string[];
  preserved: string[];
  sourceResults: Array<{ source: string; models: number; skipped: number; error?: string }>;
  prices: ModelPriceView[];
}

export interface KeyView {
  key_id: string;
  account_id?: string;
  tenant_external_id?: string;
  principal_external_id?: string;
  alias: string;
  status?: string;
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
  reserved_balance?: string;
  updated_at?: number;
  fingerprint?: string | null;
}

export interface ModelCatalogItem {
  id: string;
  object: 'model';
  owned_by: string;
  modalities?: Array<'text' | 'image' | 'video' | 'embedding' | string>;
  generation_schema?: Record<string, unknown>;
}

export interface ModelCatalogResponse {
  object: 'list';
  data: ModelCatalogItem[];
}

export type GroupKind = 'provider' | 'route' | 'credential';

export interface GroupView {
  id: string;
  tenant_id?: string;
  tenant_external_id?: string;
  name: string;
  member_count: number;
  member_ids: string[];
  created_at: number;
  updated_at: number;
}

export interface CredentialRoutingView {
  key_id: string;
  route_ids: string[];
  route_group_ids: string[];
  effective_route_ids: string[];
  updated_at: number;
  grant_revision: number;
}

export interface KeyLimitSnapshot {
  key_id: string;
  captured_at: number;
  currency: string;
  available_balance: string;
  reserved_balance: string;
  rpm: { limit: number; used: number; remaining: number; reset_at: number };
  tpm: { limit: number; used: number; remaining: number; reset_at: number };
  concurrency: { limit: number; active: number; remaining: number };
  daily_budget: BudgetLimitSnapshot;
  weekly_budget: BudgetLimitSnapshot;
  lifetime_budget: BudgetLimitSnapshot;
}

export interface BudgetLimitSnapshot {
  limit: string | null;
  settled: string;
  reserved: string;
  remaining: string | null;
  reset_at: number | null;
}

export interface ServiceTokenView {
  service_id: string;
  name: string;
  credential_generation: number;
  fingerprint: string;
  scopes: string[];
  tenant_external_id: string | null;
  status?: string;
  created_at?: number;
}

export interface ModelRouteView {
  id: string;
  tenant_id?: string;
  tenant_external_id?: string;
  public_model: string;
  upstream_account_id?: string;
  upstream_account_ids?: string[];
  included_provider_group_ids?: string[];
  excluded_provider_group_ids?: string[];
  route_group_ids?: string[];
  granted_credential_ids?: string[];
  custom_model_confirmed?: boolean;
  upstream_model: string;
  protocol: string;
  priority: number;
  enabled: boolean;
  created_at: number;
  updated_at: number;
  grant_revision: number;
}

export interface GenerationPriceView {
  id?: string;
  model: string;
  currency: string;
  billing_unit: string;
  price_per_unit: string;
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

export interface ConversationRequest extends RequestView {
  source: 'live' | 'session_archive';
  provenance: 'native' | 'archive_unlinked';
  unlinked: boolean;
  archive_source?: string;
  external_request_id?: string;
}

export interface ConversationDetail {
  cluster: ConversationCluster;
  requests: ConversationRequest[];
  edges: ConversationEdge[];
  has_more: boolean;
  next_cursor: { before_created_at: number; before_request_id: string } | null;
  edges_truncated: boolean;
}

export interface LogicalSessionSummary {
  session_id: string;
  cluster_id: string | null;
  unlinked: boolean;
  key_id: string;
  key_alias: string;
  model: string;
  protocol: string;
  last_status: 'active' | 'success' | 'error' | 'unknown';
  last_activity_at: number;
  active_requests: number;
  requests: number;
  archived_only_requests: number;
  archived_only_errors: number;
  archived_only_input_tokens: number;
  archived_only_output_tokens: number;
  archived_only_avg_duration_ms: number | null;
  errors: number;
  input_tokens: number;
  output_tokens: number;
  avg_duration_ms: number | null;
  costs: UsageAnalysisCost[];
}

export interface LogicalSessionCursor {
  before_last_activity_at: number;
  before_session_id: string;
}

export interface LogicalSessionListResponse {
  generated_at: number;
  sessions: LogicalSessionSummary[];
  next_cursor: LogicalSessionCursor | null;
}

export interface LogicalSessionDetail {
  session_id: string;
  cluster_id: string | null;
  unlinked: boolean;
  requests: ConversationRequest[];
  edges: ConversationEdge[];
  has_more: boolean;
  next_cursor: { before_created_at: number; before_request_id: string } | null;
  edges_truncated: boolean;
}

export interface GenerationJob {
  job_id: string;
  created_at: number;
  updated_at: number;
  completed_at: number | null;
  model: string;
  driver: string;
  billing_unit: 'job' | 'second' | 'image' | 'megapixel';
  status: 'queued' | 'running' | 'cancelling' | 'succeeded' | 'failed' | 'cancelled';
  upstream_job_id: string | null;
  estimated_units: number;
  billed_units: number | null;
  cost: string;
  error_code: string | null;
  result: unknown | null;
  assets: GenerationAsset[];
}

export interface GenerationAsset {
  asset_id: string;
  index: number;
  mime_type: string;
  size_bytes: number;
  filename: string;
}

export interface ProviderType {
  id: string;
  display_name: string;
  protocols: string[];
  modalities: string[];
  config_schema: Record<string, unknown>;
  credential_schema: Record<string, unknown>;
  oauth_adapter?: {
    api_version: 'oauth-adapter-v1';
    flow_kind: 'cursor_pkce';
    login_url: string;
    poll_url: string;
    refresh_url: string;
  };
  component_adapter?: {
    api_version: 'buffered-v1';
    max_response_bytes: number;
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
    request_rewrite?: boolean;
    configuration?: {
      schema: Record<string, unknown>;
      default: unknown;
    } | null;
    providers?: ProviderType[];
  };
}

export interface PluginConfiguration {
  plugin_id: string;
  tenant_external_id: string | null;
  value: unknown;
  source: 'default' | 'global' | 'tenant';
  scope_version: number;
  updated_at: number | null;
  schema_digest: string;
}

export interface UpstreamAccount {
  id: string;
  tenant_id: string;
  tenant_external_id?: string;
  name: string;
  driver: string;
  auth_kind: string;
  connection_method: 'api_key' | 'oauth' | 'legacy' | 'none' | string;
  credential_generation: number;
  status: string;
  credential_expires_at: number | null;
  can_refresh: boolean;
  can_rotate: boolean;
  can_reauthorize: boolean;
  route_count: number;
  config: Record<string, unknown>;
  created_at: number;
  updated_at: number;
}

export interface UpstreamHealth {
  account_id: string;
  status: 'healthy' | 'unhealthy';
  error_code?: string | null;
  upstream_status?: number;
  latency_ms?: number;
  checked_at: number;
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
