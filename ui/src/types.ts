export interface KeyRow {
  token: string
  token_id?: string
  key_name: string | null
  key_alias: string | null
  user_id: string | null
  team_id: string | null
  team_alias?: string | null
  spend: number
  max_budget: number | null
  soft_budget?: number | null
  budget_duration: string | null
  budget_reset_at: number | null
  expires: number | null
  models: string[]
  tpm_limit: number | null
  rpm_limit: number | null
  max_parallel_requests: number | null
  blocked: boolean
  created_at: number
  updated_at: number
  last_active_at: number | null
  metadata: Record<string, unknown>
  key?: string
}

export interface TeamRow {
  team_id: string
  team_alias: string | null
  spend: number
  max_budget: number | null
  budget_duration: string | null
  models: string[]
  tpm_limit: number | null
  rpm_limit: number | null
  blocked: boolean
  members_with_roles: Array<Record<string, unknown>>
  created_at: number
}

export interface UserRow {
  user_id: string
  user_email: string | null
  user_alias: string | null
  user_role: string
  spend: number
  max_budget: number | null
  models: string[]
  created_at: number
}

export interface Paged<T> {
  items: T[]
  total: number
  page: number
  totalPages: number
}

export interface SpendLog {
  request_id: string
  call_type: string
  api_key: string
  key_alias: string | null
  user_id: string | null
  team_id: string | null
  spend: number
  total_tokens: number
  prompt_tokens: number
  completion_tokens: number
  start_time: number
  end_time: number
  startTime?: string
  endTime?: string
  request_duration_ms: number | null
  model: string
  model_id: string | null
  model_group: string | null
  custom_llm_provider: string | null
  status: string
  error: unknown
  stream: boolean
  request_tags: unknown
  metadata: unknown
}

export interface SpendLogsPage {
  data: SpendLog[]
  total: number
  page: number
  page_size: number
  total_pages: number
}

export interface Metrics {
  spend: number
  prompt_tokens: number
  completion_tokens: number
  total_tokens: number
  api_requests: number
  successful_requests: number
  failed_requests: number
  cache_read_input_tokens?: number
  cache_creation_input_tokens?: number
}

export interface DailyActivity {
  results: Array<{
    date: string
    metrics: Metrics
    breakdown: Record<string, Record<string, { metrics: Metrics }>>
  }>
  metadata?: {
    total_spend?: number
    total_tokens?: number
    total_api_requests?: number
    [key: string]: unknown
  }
}

export interface EndpointHealth {
  model_name: string
  model_id: string
  status: 'unknown' | 'healthy' | 'unhealthy'
  latency_ms?: number
  checked_at_epoch?: number
  error?: string
  cooldown_remaining_s?: number
  cooldown_reason?: string
  in_flight: number
}

export interface HealthReport {
  status: string
  healthy_count: number
  unhealthy_count: number
  healthy_endpoints: EndpointHealth[]
  unhealthy_endpoints: EndpointHealth[]
  unknown_endpoints: EndpointHealth[]
}

export interface ModelInfoEntry {
  model_name: string
  litellm_params: {
    model: string
    api_base: string
    api_key: string | null
    rpm: number | null
    tpm: number | null
    weight: number | null
    order: number | null
    tags: string[]
    timeout: number
  }
  model_info: {
    id: string
    mode: string
    base_model: string | null
    provider: string
    pricing_key: string | null
    max_input_tokens: number | null
    max_output_tokens: number | null
    input_cost_per_token: number | null
    output_cost_per_token: number | null
    blocked: boolean
    health: EndpointHealth | null
  }
}

export interface ModelGroup {
  model_group: string
  providers: string[]
  deployments: number
  healthy: number
  cooled_down: number
  max_input_tokens: number | null
  max_output_tokens: number | null
  input_cost_per_token: number | null
  output_cost_per_token: number | null
  mode: string
  supports_responses_api: boolean
  supports_messages_api: boolean
}

export interface Readiness {
  status: string
  db: string
  spend_events_dropped: number
  version: string
  uptime_s: number
  deployments: number
  config_path: string
}
