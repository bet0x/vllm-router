import { apiFetch, apiPost } from './client';
import type { AdminStats, DecisionsResponse, TenantsResponse, WorkersResponse } from '../lib/types';

export const fetchStats = () => apiFetch<AdminStats>('/admin/stats');
export const fetchDecisions = (limit = 100) =>
  apiFetch<DecisionsResponse>(`/admin/decisions?limit=${limit}`);
export const fetchTenants = () => apiFetch<TenantsResponse>('/admin/tenants');
export const fetchWorkers = () => apiFetch<WorkersResponse>('/workers');
export const fetchConfig = () => apiFetch<Record<string, unknown>>('/admin/config');
export const reloadConfig = () => apiPost<Record<string, unknown>>('/admin/reload');
export const flushCache = () => apiPost<Record<string, unknown>>('/flush_cache');
export const drainWorker = (url: string, timeout_secs = 300) =>
  apiPost<Record<string, unknown>>('/admin/drain', { url, timeout_secs });

export interface DrainStatus {
  url: string;
  draining: boolean;
  current_load: number;
  healthy: boolean;
}

export const fetchDrainStatus = (url: string) =>
  apiFetch<DrainStatus>(`/admin/drain/status?url=${encodeURIComponent(url)}`);

export interface ModelInfo {
  id: string;
  object: string;
  created: number;
  owned_by: string;
  max_model_len?: number;
  root?: string;
}

export interface ModelsResponse {
  object: string;
  data: ModelInfo[];
}

export const fetchModels = () => apiFetch<ModelsResponse>('/v1/models');

export async function fetchWorkerMetrics(workerUrl: string): Promise<string> {
  const encoded = encodeURIComponent(workerUrl);
  const headers: Record<string, string> = {};
  const key = localStorage.getItem('vllm-router-api-key');
  if (key) {
    headers['X-Admin-Key'] = key;
    headers['Authorization'] = `Bearer ${key}`;
  }
  const res = await fetch(`/api/workers/${encoded}/metrics`, { headers });
  if (!res.ok) throw new Error(`${res.status}`);
  return res.text();
}
