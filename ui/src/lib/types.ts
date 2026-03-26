// ── Admin API types ──

export interface AdminStats {
  uptime_secs: number;
  cache: {
    backend: string;
    exact_entries: number;
    semantic_entries: number;
  };
  workers: {
    total: number;
    healthy: number;
    draining: number;
  };
  policies: {
    default: string;
    per_model: Record<string, string>;
  };
  decisions_logged: number;
  tenants?: {
    count: number;
    entries: string[];
  };
}

export interface WorkerInfo {
  url: string;
  model_id: string;
  worker_type: string;
  is_healthy: boolean;
  draining: boolean;
  load: number;
  connection_mode: string;
  priority: number;
  cost: number;
}

export interface WorkersResponse {
  workers: WorkerInfo[];
  total: number;
  stats: {
    prefill_count: number;
    decode_count: number;
    regular_count: number;
  };
}

export interface Decision {
  schema_version?: number;
  timestamp: string;
  route: string;
  model?: string;
  method?: string;
  policy?: string;
  cluster?: string;
  worker?: string;
  cache_status?: string;
  status: number;
  duration_ms: number;
  tenant?: string;
  hooks_ran?: string[];
  request_text?: string;
}

export interface DecisionsResponse {
  decisions: Decision[];
}

export interface TenantInfo {
  name: string;
  enabled: boolean;
  rate_limit_rps: number;
  max_concurrent: number;
  allowed_models: string[];
  total_requests: number;
  total_rate_limited: number;
  metadata?: Record<string, unknown>;
}

export interface TenantsResponse {
  tenants: TenantInfo[] | string[];
  message?: string;
}

// ── Prometheus types ──

export interface MetricSample {
  name: string;
  labels: Record<string, string>;
  value: number;
}

export interface ParsedMetrics {
  samples: MetricSample[];
  types: Record<string, string>;
}

// ── Time-series point ──

export interface TimePoint {
  time: number; // epoch ms
  [key: string]: number;
}
