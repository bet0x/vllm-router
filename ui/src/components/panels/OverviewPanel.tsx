import GaugeCard from '../charts/GaugeCard';
import SimpleLineChart from '../charts/SimpleLineChart';
import type { AdminStats, ParsedMetrics, TimePoint } from '../../lib/types';
import { getGauge, getSamples, histogramPercentile } from '../../lib/prometheus-parser';
import { usePolling } from '../../hooks/usePolling';
import { fetchModels } from '../../api/admin';

interface Props {
  stats: AdminStats | null;
  parsed: ParsedMetrics | null;
  overviewData: TimePoint[];
}

export default function OverviewPanel({ stats, parsed, overviewData }: Props) {
  const { data: models } = usePolling(fetchModels, 30000);
  const latest = overviewData[overviewData.length - 1];

  const cacheHits = parsed ? (getGauge(parsed, 'vllm_router_cache_hits_total') ?? 0) : 0;
  const cacheMisses = parsed ? (getGauge(parsed, 'vllm_router_cache_misses_total') ?? 0) : 0;
  const cacheTotal = cacheHits + cacheMisses;
  const hitRate = cacheTotal > 0 ? ((cacheHits / cacheTotal) * 100).toFixed(1) : '0';

  const p50 = parsed ? histogramPercentile(parsed, 'vllm_router_generate_duration_seconds', 50) : undefined;
  const p95 = parsed ? histogramPercentile(parsed, 'vllm_router_generate_duration_seconds', 95) : undefined;

  // Totals from Prometheus counters (always available, even without live traffic)
  const totalRequests = parsed
    ? getSamples(parsed, 'vllm_router_requests_total').reduce((s, m) => s + m.value, 0)
    : 0;
  const totalErrors = parsed
    ? getSamples(parsed, 'vllm_router_request_errors_total').reduce((s, m) => s + m.value, 0)
    : 0;

  const hasLiveData = overviewData.length >= 3;

  return (
    <div className="space-y-6">
      <h2 className="text-lg font-semibold">Overview</h2>

      <div className="grid grid-cols-2 gap-4 lg:grid-cols-4 xl:grid-cols-8">
        <GaugeCard
          title="Workers"
          value={stats ? `${stats.workers.healthy}/${stats.workers.total}` : '-'}
          subtitle={stats && stats.workers.draining > 0 ? `${stats.workers.draining} draining` : 'all healthy'}
          color={stats?.workers.healthy === stats?.workers.total ? 'text-ok' : 'text-warn'}
        />
        <GaugeCard
          title="Total Requests"
          value={totalRequests.toLocaleString()}
          subtitle="since start"
        />
        <GaugeCard
          title="Total Errors"
          value={totalErrors.toLocaleString()}
          subtitle="since start"
          color={totalErrors > 0 ? 'text-err' : 'text-ok'}
        />
        <GaugeCard
          title="Request Rate"
          value={latest ? `${latest.rps.toFixed(1)}/s` : '-'}
          subtitle="live (5s window)"
        />
        <GaugeCard
          title="Cache Hit Rate"
          value={`${hitRate}%`}
          subtitle={`${cacheHits} hits / ${cacheMisses} misses`}
        />
        <GaugeCard
          title="P50 Latency"
          value={p50 !== undefined ? `${(p50 * 1000).toFixed(0)}ms` : '-'}
          subtitle="median"
        />
        <GaugeCard
          title="P95 Latency"
          value={p95 !== undefined ? `${(p95 * 1000).toFixed(0)}ms` : '-'}
          subtitle="tail"
          color={p95 !== undefined && p95 > 10 ? 'text-warn' : 'text-accent'}
        />
        <GaugeCard
          title="Decisions Logged"
          value={stats?.decisions_logged ?? 0}
          subtitle="ring buffer"
        />
      </div>

      <div className="rounded-lg border border-border bg-surface-alt p-4">
        <h3 className="mb-2 text-sm font-medium text-text-muted">Throughput (live)</h3>
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

      <div className="rounded-lg border border-border bg-surface-alt p-4">
        <h3 className="mb-2 text-sm font-medium text-text-muted">Latency Percentiles (cumulative since start)</h3>
        <div className="grid grid-cols-3 gap-6 py-4">
          <div className="text-center">
            <p className="text-2xl font-bold text-ok">{p50 !== undefined ? `${(p50 * 1000).toFixed(0)}ms` : '-'}</p>
            <p className="text-xs text-text-muted">P50</p>
          </div>
          <div className="text-center">
            <p className="text-2xl font-bold text-warn">{p95 !== undefined ? `${(p95 * 1000).toFixed(0)}ms` : '-'}</p>
            <p className="text-xs text-text-muted">P95</p>
          </div>
          <div className="text-center">
            <p className="text-2xl font-bold text-err">
              {parsed ? `${((histogramPercentile(parsed, 'vllm_router_generate_duration_seconds', 99) ?? 0) * 1000).toFixed(0)}ms` : '-'}
            </p>
            <p className="text-xs text-text-muted">P99</p>
          </div>
        </div>
        {hasLiveData && (
          <SimpleLineChart
            data={overviewData}
            series={[
              { dataKey: 'p50', color: '#22c55e', label: 'P50' },
              { dataKey: 'p95', color: '#f59e0b', label: 'P95' },
              { dataKey: 'p99', color: '#ef4444', label: 'P99' },
            ]}
            height={180}
            yLabel="seconds"
          />
        )}
      </div>

      {/* Models */}
      {models && models.data.length > 0 && (
        <div className="rounded-lg border border-border bg-surface-alt p-4">
          <h3 className="mb-3 text-sm font-medium text-text-muted">Available Models</h3>
          <div className="space-y-2">
            {models.data.map((m) => (
              <div key={m.id} className="flex items-center justify-between rounded border border-border bg-surface p-3">
                <div>
                  <span className="text-sm font-medium">{m.id}</span>
                  <div className="flex gap-3 mt-0.5 text-xs text-text-muted">
                    <span>owned by {m.owned_by}</span>
                    {m.max_model_len && <span>ctx: {m.max_model_len.toLocaleString()}</span>}
                  </div>
                </div>
              </div>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}
