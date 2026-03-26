const API_BASE = '/api';
const METRICS_PATH = '/metrics';

export function getApiKey(): string | null {
  return localStorage.getItem('vllm-router-api-key');
}

export function setApiKey(key: string) {
  localStorage.setItem('vllm-router-api-key', key);
}

export function clearApiKey() {
  localStorage.removeItem('vllm-router-api-key');
}

function authHeaders(): Record<string, string> {
  const key = getApiKey();
  if (!key) return {};
  // X-Admin-Key for /admin/* endpoints, Authorization for /workers and inference endpoints
  return { 'X-Admin-Key': key, 'Authorization': `Bearer ${key}` };
}

export async function apiFetch<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(`${API_BASE}${path}`, {
    ...init,
    headers: { ...authHeaders(), ...init?.headers },
  });
  if (!res.ok) {
    throw new Error(`${res.status} ${res.statusText}`);
  }
  return res.json();
}

export async function apiPost<T>(path: string, body?: unknown): Promise<T> {
  return apiFetch<T>(path, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: body ? JSON.stringify(body) : undefined,
  });
}

export async function apiDelete<T>(path: string): Promise<T> {
  return apiFetch<T>(path, { method: 'DELETE' });
}

export async function fetchMetricsText(): Promise<string> {
  const res = await fetch(METRICS_PATH);
  if (!res.ok) throw new Error(`metrics: ${res.status}`);
  return res.text();
}
