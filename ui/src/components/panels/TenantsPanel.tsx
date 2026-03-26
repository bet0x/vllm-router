import type { TenantsResponse, TenantInfo, ParsedMetrics } from '../../lib/types';
import { getSample, getSamples } from '../../lib/prometheus-parser';
import GaugeCard from '../charts/GaugeCard';
import { BarChart, Bar, XAxis, YAxis, Tooltip, ResponsiveContainer, Cell } from 'recharts';

interface Props {
  tenants: TenantsResponse | null;
  parsed: ParsedMetrics | null;
}

function isTenantInfoArray(arr: unknown[]): arr is TenantInfo[] {
  return arr.length > 0 && typeof arr[0] === 'object';
}

const COLORS = ['#38bdf8', '#22c55e', '#f59e0b', '#ef4444', '#a78bfa', '#fb923c', '#14b8a6', '#f472b6'];

export default function TenantsPanel({ tenants, parsed }: Props) {
  if (!tenants) return <p className="text-text-muted">Loading tenants...</p>;

  if (tenants.message || tenants.tenants.length === 0) {
    return (
      <div className="space-y-4">
        <h2 className="text-lg font-semibold">Tenants</h2>
        <div className="rounded-lg border border-border bg-surface-alt p-8 text-center">
          <p className="text-text-muted">{tenants.message ?? 'No tenants configured'}</p>
          <p className="mt-2 text-xs text-text-muted">
            Configure <code className="rounded bg-surface px-1.5 py-0.5">api_keys</code> in your router config to enable multi-tenant mode.
          </p>
        </div>
      </div>
    );
  }

  const richTenants = isTenantInfoArray(tenants.tenants);

  // Build Prometheus-based per-tenant stats
  const tenantNames = richTenants
    ? (tenants.tenants as TenantInfo[]).map((t) => t.name)
    : (tenants.tenants as string[]);

  const promStats = tenantNames.map((name) => {
    const requests = parsed
      ? getSamples(parsed, 'vllm_router_tenant_requests_total')
          .filter((s) => s.labels.tenant === name)
          .reduce((sum, s) => sum + s.value, 0)
      : 0;
    const rateLimited = parsed
      ? (getSample(parsed, 'vllm_router_tenant_rate_limited_total', { tenant: name }) ?? 0)
      : 0;
    const errors = parsed
      ? getSamples(parsed, 'vllm_router_tenant_errors_total')
          .filter((s) => s.labels.tenant === name)
          .reduce((sum, s) => sum + s.value, 0)
      : 0;
    return { name, requests, rateLimited, errors };
  });

  const totalRequests = promStats.reduce((s, t) => s + t.requests, 0);
  const totalRateLimited = promStats.reduce((s, t) => s + t.rateLimited, 0);
  const totalErrors = promStats.reduce((s, t) => s + t.errors, 0);

  // Chart data for requests distribution
  const requestsChartData = promStats
    .filter((t) => t.requests > 0)
    .sort((a, b) => b.requests - a.requests);

  const rateLimitedChartData = promStats
    .filter((t) => t.rateLimited > 0)
    .sort((a, b) => b.rateLimited - a.rateLimited);

  return (
    <div className="space-y-6">
      <h2 className="text-lg font-semibold">Tenants</h2>

      {/* Summary cards */}
      <div className="grid grid-cols-2 gap-4 lg:grid-cols-4">
        <GaugeCard title="Tenants" value={tenantNames.length} subtitle="configured" />
        <GaugeCard title="Total Requests" value={totalRequests.toLocaleString()} subtitle="all tenants" />
        <GaugeCard title="Rate Limited" value={totalRateLimited.toLocaleString()} subtitle="429 responses" color={totalRateLimited > 0 ? 'text-warn' : 'text-ok'} />
        <GaugeCard title="Errors" value={totalErrors.toLocaleString()} subtitle="all tenants" color={totalErrors > 0 ? 'text-err' : 'text-ok'} />
      </div>

      {/* Charts */}
      <div className="grid grid-cols-1 gap-4 lg:grid-cols-2">
        {requestsChartData.length > 0 && (
          <div className="rounded-lg border border-border bg-surface-alt p-4">
            <h3 className="mb-3 text-sm font-medium text-text-muted">Requests by Tenant</h3>
            <ResponsiveContainer width="100%" height={200}>
              <BarChart data={requestsChartData} layout="vertical" margin={{ left: 80 }}>
                <XAxis type="number" tick={{ fontSize: 10, fill: '#94a3b8' }} stroke="#334155" />
                <YAxis type="category" dataKey="name" tick={{ fontSize: 11, fill: '#e2e8f0' }} stroke="#334155" width={75} />
                <Tooltip contentStyle={{ backgroundColor: '#1e293b', border: '1px solid #334155', borderRadius: 6, fontSize: 12 }} />
                <Bar dataKey="requests" name="Requests" radius={[0, 4, 4, 0]}>
                  {requestsChartData.map((_, i) => (
                    <Cell key={i} fill={COLORS[i % COLORS.length]} />
                  ))}
                </Bar>
              </BarChart>
            </ResponsiveContainer>
          </div>
        )}

        {rateLimitedChartData.length > 0 && (
          <div className="rounded-lg border border-border bg-surface-alt p-4">
            <h3 className="mb-3 text-sm font-medium text-text-muted">Rate Limited by Tenant</h3>
            <ResponsiveContainer width="100%" height={200}>
              <BarChart data={rateLimitedChartData} layout="vertical" margin={{ left: 80 }}>
                <XAxis type="number" tick={{ fontSize: 10, fill: '#94a3b8' }} stroke="#334155" />
                <YAxis type="category" dataKey="name" tick={{ fontSize: 11, fill: '#e2e8f0' }} stroke="#334155" width={75} />
                <Tooltip contentStyle={{ backgroundColor: '#1e293b', border: '1px solid #334155', borderRadius: 6, fontSize: 12 }} />
                <Bar dataKey="rateLimited" name="Rate Limited" fill="#f59e0b" radius={[0, 4, 4, 0]} />
              </BarChart>
            </ResponsiveContainer>
          </div>
        )}
      </div>

      {/* Tenant table */}
      <div className="overflow-x-auto rounded-lg border border-border">
        <table className="w-full text-sm">
          <thead className="bg-surface-alt text-left text-xs text-text-muted">
            <tr>
              <th className="px-4 py-2">Name</th>
              {richTenants && (
                <>
                  <th className="px-4 py-2">Status</th>
                  <th className="px-4 py-2">Rate Limit</th>
                  <th className="px-4 py-2">Max Concurrent</th>
                  <th className="px-4 py-2">Models</th>
                </>
              )}
              <th className="px-4 py-2">Requests</th>
              <th className="px-4 py-2">Rate Limited</th>
              <th className="px-4 py-2">Errors</th>
            </tr>
          </thead>
          <tbody>
            {richTenants
              ? (tenants.tenants as TenantInfo[]).map((t) => {
                  const ps = promStats.find((p) => p.name === t.name);
                  return (
                    <tr key={t.name} className="border-t border-border hover:bg-surface-hover/50">
                      <td className="px-4 py-2 font-medium">{t.name}</td>
                      <td className="px-4 py-2">
                        <span className={`rounded-full px-2 py-0.5 text-xs ${t.enabled ? 'bg-ok/15 text-ok' : 'bg-err/15 text-err'}`}>
                          {t.enabled ? 'Active' : 'Disabled'}
                        </span>
                      </td>
                      <td className="px-4 py-2 text-xs">{t.rate_limit_rps} rps</td>
                      <td className="px-4 py-2 text-xs">{t.max_concurrent}</td>
                      <td className="px-4 py-2">
                        <div className="flex flex-wrap gap-1">
                          {t.allowed_models.map((m) => (
                            <span key={m} className="rounded bg-surface-hover px-1.5 py-0.5 text-xs">{m}</span>
                          ))}
                        </div>
                      </td>
                      <td className="px-4 py-2 text-xs font-medium">{(ps?.requests ?? t.total_requests).toLocaleString()}</td>
                      <td className={`px-4 py-2 text-xs font-medium ${(ps?.rateLimited ?? t.total_rate_limited) > 0 ? 'text-warn' : ''}`}>
                        {(ps?.rateLimited ?? t.total_rate_limited).toLocaleString()}
                      </td>
                      <td className={`px-4 py-2 text-xs font-medium ${(ps?.errors ?? 0) > 0 ? 'text-err' : ''}`}>
                        {(ps?.errors ?? 0).toLocaleString()}
                      </td>
                    </tr>
                  );
                })
              : (tenants.tenants as string[]).map((name) => {
                  const ps = promStats.find((p) => p.name === name);
                  return (
                    <tr key={name} className="border-t border-border hover:bg-surface-hover/50">
                      <td className="px-4 py-2 font-medium">{name}</td>
                      <td className="px-4 py-2 text-xs font-medium">{(ps?.requests ?? 0).toLocaleString()}</td>
                      <td className={`px-4 py-2 text-xs font-medium ${(ps?.rateLimited ?? 0) > 0 ? 'text-warn' : ''}`}>
                        {(ps?.rateLimited ?? 0).toLocaleString()}
                      </td>
                      <td className={`px-4 py-2 text-xs font-medium ${(ps?.errors ?? 0) > 0 ? 'text-err' : ''}`}>
                        {(ps?.errors ?? 0).toLocaleString()}
                      </td>
                    </tr>
                  );
                })}
          </tbody>
        </table>
      </div>
    </div>
  );
}
