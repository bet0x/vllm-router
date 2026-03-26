import SimpleLineChart from '../charts/SimpleLineChart';
import GaugeCard from '../charts/GaugeCard';
import type { ParsedMetrics, TimePoint } from '../../lib/types';
import { getSamples, histogramPercentile } from '../../lib/prometheus-parser';

interface Props {
  parsed: ParsedMetrics | null;
  overviewData: TimePoint[];
}

export default function RequestsPanel({ parsed, overviewData }: Props) {
  // Per-route breakdown from Prometheus counters (always available)
  const routeSamples = parsed ? getSamples(parsed, 'vllm_router_requests_total') : [];
  const routes = routeSamples
    .map((s) => ({ route: s.labels.route ?? 'unknown', total: s.value }))
    .sort((a, b) => b.total - a.total);
  const totalRequests = routes.reduce((s, r) => s + r.total, 0);

  // Error breakdown
  const errorSamples = parsed ? getSamples(parsed, 'vllm_router_request_errors_total') : [];
  const errors = errorSamples
    .map((s) => ({
      route: s.labels.route ?? 'unknown',
      type: s.labels.error_type ?? 'unknown',
      total: s.value,
    }))
    .filter((e) => e.total > 0);
  const totalErrors = errors.reduce((s, e) => s + e.total, 0);

  // Per-worker breakdown
  const workerSamples = parsed ? getSamples(parsed, 'vllm_router_worker_requests_total') : [];
  const workerTotals = new Map<string, number>();
  for (const s of workerSamples) {
    const w = s.labels.worker ?? 'unknown';
    workerTotals.set(w, (workerTotals.get(w) ?? 0) + s.value);
  }
  const workers = [...workerTotals.entries()]
    .map(([url, total]) => ({ url, total }))
    .sort((a, b) => b.total - a.total);

  // Retry stats
  const totalRetries = parsed
    ? getSamples(parsed, 'vllm_router_retries_total').reduce((s, x) => s + x.value, 0)
    : 0;
  const totalExhausted = parsed
    ? getSamples(parsed, 'vllm_router_retries_exhausted_total').reduce((s, x) => s + x.value, 0)
    : 0;

  // Latency (cumulative)
  const p50 = parsed ? histogramPercentile(parsed, 'vllm_router_generate_duration_seconds', 50) : undefined;
  const p95 = parsed ? histogramPercentile(parsed, 'vllm_router_generate_duration_seconds', 95) : undefined;
  const p99 = parsed ? histogramPercentile(parsed, 'vllm_router_generate_duration_seconds', 99) : undefined;

  const hasLiveData = overviewData.length >= 3;

  return (
    <div className="space-y-6">
      <h2 className="text-lg font-semibold">Requests</h2>

      {/* Summary cards */}
      <div className="grid grid-cols-2 gap-4 lg:grid-cols-4 xl:grid-cols-6">
        <GaugeCard title="Total Requests" value={totalRequests.toLocaleString()} subtitle="since start" />
        <GaugeCard title="Total Errors" value={totalErrors.toLocaleString()} subtitle="since start" color={totalErrors > 0 ? 'text-err' : 'text-ok'} />
        <GaugeCard title="Retries" value={totalRetries} subtitle={totalExhausted > 0 ? `${totalExhausted} exhausted` : 'none exhausted'} color={totalExhausted > 0 ? 'text-warn' : 'text-text'} />
        <GaugeCard title="P50" value={p50 !== undefined ? `${(p50 * 1000).toFixed(0)}ms` : '-'} subtitle="median latency" color="text-ok" />
        <GaugeCard title="P95" value={p95 !== undefined ? `${(p95 * 1000).toFixed(0)}ms` : '-'} subtitle="tail latency" color="text-warn" />
        <GaugeCard title="P99" value={p99 !== undefined ? `${(p99 * 1000).toFixed(0)}ms` : '-'} subtitle="worst case" color="text-err" />
      </div>

      {/* Route breakdown */}
      <div className="rounded-lg border border-border bg-surface-alt p-4">
        <h3 className="mb-3 text-sm font-medium text-text-muted">Requests by Route</h3>
        {routes.length === 0 ? (
          <p className="text-xs text-text-muted">No requests yet</p>
        ) : (
          <div className="space-y-2">
            {routes.map((r) => (
              <div key={r.route} className="flex items-center gap-3">
                <span className="min-w-48 font-mono text-xs text-text-muted">{r.route}</span>
                <div className="flex-1">
                  <div className="h-2 rounded-full bg-surface">
                    <div
                      className="h-2 rounded-full bg-accent"
                      style={{ width: `${Math.min(100, (r.total / Math.max(1, ...routes.map((x) => x.total))) * 100)}%` }}
                    />
                  </div>
                </div>
                <span className="text-xs font-medium">{r.total.toLocaleString()}</span>
              </div>
            ))}
          </div>
        )}
      </div>

      {/* Worker breakdown */}
      {workers.length > 0 && (
        <div className="rounded-lg border border-border bg-surface-alt p-4">
          <h3 className="mb-3 text-sm font-medium text-text-muted">Requests by Worker</h3>
          <div className="space-y-2">
            {workers.map((w) => (
              <div key={w.url} className="flex items-center gap-3">
                <span className="min-w-48 font-mono text-xs text-text-muted">{w.url}</span>
                <div className="flex-1">
                  <div className="h-2 rounded-full bg-surface">
                    <div
                      className="h-2 rounded-full bg-ok"
                      style={{ width: `${Math.min(100, (w.total / Math.max(1, ...workers.map((x) => x.total))) * 100)}%` }}
                    />
                  </div>
                </div>
                <span className="text-xs font-medium">{w.total.toLocaleString()}</span>
              </div>
            ))}
          </div>
        </div>
      )}

      {/* Policy decisions */}
      {(() => {
        const policySamples = parsed ? getSamples(parsed, 'vllm_router_policy_decisions_total') : [];
        if (policySamples.length === 0) return null;
        // Group by policy
        const byPolicy = new Map<string, { worker: string; total: number }[]>();
        for (const s of policySamples) {
          const policy = s.labels.policy ?? 'unknown';
          const worker = s.labels.worker ?? 'unknown';
          if (!byPolicy.has(policy)) byPolicy.set(policy, []);
          byPolicy.get(policy)!.push({ worker, total: s.value });
        }
        return (
          <div className="rounded-lg border border-border bg-surface-alt p-4">
            <h3 className="mb-3 text-sm font-medium text-text-muted">Policy Decisions</h3>
            <div className="space-y-4">
              {[...byPolicy.entries()].map(([policy, workers]) => {
                const policyTotal = workers.reduce((s, w) => s + w.total, 0);
                return (
                  <div key={policy}>
                    <div className="flex items-center gap-2 mb-2">
                      <span className="rounded bg-accent/15 px-1.5 py-0.5 text-xs font-medium text-accent">{policy}</span>
                      <span className="text-xs text-text-muted">{policyTotal.toLocaleString()} total</span>
                    </div>
                    <div className="space-y-1">
                      {workers.sort((a, b) => b.total - a.total).map((w) => (
                        <div key={w.worker} className="flex items-center gap-3">
                          <span className="min-w-48 font-mono text-xs text-text-muted">{w.worker}</span>
                          <div className="flex-1">
                            <div className="h-2 rounded-full bg-surface">
                              <div
                                className="h-2 rounded-full bg-accent"
                                style={{ width: `${Math.min(100, (w.total / Math.max(1, ...workers.map((x) => x.total))) * 100)}%` }}
                              />
                            </div>
                          </div>
                          <span className="text-xs font-medium">{w.total.toLocaleString()}</span>
                          <span className="text-xs text-text-muted">{policyTotal > 0 ? `${((w.total / policyTotal) * 100).toFixed(0)}%` : ''}</span>
                        </div>
                      ))}
                    </div>
                  </div>
                );
              })}
            </div>
          </div>
        );
      })()}

      {/* Errors breakdown */}
      {errors.length > 0 && (
        <div className="rounded-lg border border-border bg-surface-alt p-4">
          <h3 className="mb-3 text-sm font-medium text-text-muted">Errors by Type</h3>
          <div className="space-y-1">
            {errors.map((e, i) => (
              <div key={i} className="flex justify-between text-sm">
                <span className="text-text-muted">
                  <span className="font-mono text-xs">{e.route}</span>
                  <span className="mx-2 text-border">/</span>
                  <span className="text-xs">{e.type}</span>
                </span>
                <span className="font-medium text-err">{e.total}</span>
              </div>
            ))}
          </div>
        </div>
      )}

      {/* Live RPS chart */}
      <div className="rounded-lg border border-border bg-surface-alt p-4">
        <h3 className="mb-2 text-sm font-medium text-text-muted">Live Throughput</h3>
        {hasLiveData ? (
          <SimpleLineChart
            data={overviewData}
            series={[
              { dataKey: 'rps', color: '#38bdf8', label: 'RPS' },
              { dataKey: 'errRate', color: '#ef4444', label: 'Errors/s' },
            ]}
            height={220}
            yLabel="req/s"
          />
        ) : (
          <div className="flex h-[220px] items-center justify-center text-sm text-text-muted">
            Collecting data... send requests to see live throughput
          </div>
        )}
      </div>
    </div>
  );
}
