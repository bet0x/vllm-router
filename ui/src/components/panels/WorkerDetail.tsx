import { useEffect, useState } from 'react';
import type { WorkerInfo } from '../../lib/types';
import { parsePrometheus, getGauge, getSample, getSamples, histogramPercentile } from '../../lib/prometheus-parser';
import { fetchWorkerMetrics } from '../../api/admin';
import GaugeCard from '../charts/GaugeCard';

interface Props {
  worker: WorkerInfo;
  onClose: () => void;
}

interface VllmMetrics {
  requestsRunning: number;
  requestsWaiting: number;
  kvCacheUsage: number;
  ttftP50: number;
  ttftP95: number;
  e2eP50: number;
  e2eP95: number;
  interTokenP50: number;
  interTokenP95: number;
  promptTokens: number;
  generationTokens: number;
  prefixCacheHits: number;
  prefixCacheQueries: number;
  requestsStop: number;
  requestsLength: number;
  requestsAbort: number;
  requestsError: number;
  preemptions: number;
  residentMemoryMb: number;
}

function extractVllmMetrics(text: string): VllmMetrics {
  const p = parsePrometheus(text);

  const g = (name: string) => {
    // vLLM metrics have labels, get the first sample
    const samples = getSamples(p, name);
    return samples.length > 0 ? samples[0].value : 0;
  };

  const successByReason = (reason: string) =>
    getSample(p, 'vllm:request_success_total', { finished_reason: reason }) ?? 0;

  return {
    requestsRunning: g('vllm:num_requests_running'),
    requestsWaiting: g('vllm:num_requests_waiting'),
    kvCacheUsage: g('vllm:kv_cache_usage_perc') * 100,
    ttftP50: histogramPercentile(p, 'vllm:time_to_first_token_seconds', 50) ?? 0,
    ttftP95: histogramPercentile(p, 'vllm:time_to_first_token_seconds', 95) ?? 0,
    e2eP50: histogramPercentile(p, 'vllm:e2e_request_latency_seconds', 50) ?? 0,
    e2eP95: histogramPercentile(p, 'vllm:e2e_request_latency_seconds', 95) ?? 0,
    interTokenP50: histogramPercentile(p, 'vllm:inter_token_latency_seconds', 50) ?? 0,
    interTokenP95: histogramPercentile(p, 'vllm:inter_token_latency_seconds', 95) ?? 0,
    promptTokens: g('vllm:prompt_tokens_total'),
    generationTokens: g('vllm:generation_tokens_total'),
    prefixCacheHits: g('vllm:prefix_cache_hits_total'),
    prefixCacheQueries: g('vllm:prefix_cache_queries_total'),
    requestsStop: successByReason('stop'),
    requestsLength: successByReason('length'),
    requestsAbort: successByReason('abort'),
    requestsError: successByReason('error'),
    preemptions: g('vllm:num_preemptions_total'),
    residentMemoryMb: (getGauge(p, 'process_resident_memory_bytes') ?? 0) / (1024 * 1024),
  };
}

export default function WorkerDetail({ worker, onClose }: Props) {
  const [metrics, setMetrics] = useState<VllmMetrics | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let active = true;

    const tick = async () => {
      try {
        const text = await fetchWorkerMetrics(worker.url);
        if (active) {
          setMetrics(extractVllmMetrics(text));
          setError(null);
        }
      } catch (err) {
        if (active) setError(err instanceof Error ? err.message : String(err));
      }
    };

    tick();
    const id = setInterval(tick, 5000);
    return () => { active = false; clearInterval(id); };
  }, [worker.url]);

  const prefixHitRate = metrics && metrics.prefixCacheQueries > 0
    ? ((metrics.prefixCacheHits / metrics.prefixCacheQueries) * 100).toFixed(1)
    : '0';

  return (
    <div className="space-y-4">
      {/* Header */}
      <div className="flex items-center justify-between">
        <div>
          <button onClick={onClose} className="text-xs text-accent hover:underline">
            &larr; Back to workers
          </button>
          <h2 className="mt-1 text-lg font-semibold font-mono">{worker.url}</h2>
          <p className="text-xs text-text-muted">{worker.model_id}</p>
        </div>
        <span className={`rounded-full px-2.5 py-0.5 text-xs font-medium ${
          worker.is_healthy ? 'bg-ok/15 text-ok' : 'bg-err/15 text-err'
        }`}>
          {worker.draining ? 'Draining' : worker.is_healthy ? 'Healthy' : 'Down'}
        </span>
      </div>

      {error && (
        <div className="rounded-lg border border-err/30 bg-err/10 p-3 text-sm text-err">
          Failed to fetch worker metrics: {error}
        </div>
      )}

      {!metrics && !error && (
        <p className="text-sm text-text-muted">Loading vLLM metrics...</p>
      )}

      {metrics && (
        <>
          {/* Engine status */}
          <div className="grid grid-cols-2 gap-4 lg:grid-cols-4">
            <GaugeCard
              title="Requests Running"
              value={metrics.requestsRunning}
              color={metrics.requestsRunning > 0 ? 'text-accent' : 'text-text'}
            />
            <GaugeCard
              title="Requests Waiting"
              value={metrics.requestsWaiting}
              color={metrics.requestsWaiting > 0 ? 'text-warn' : 'text-text'}
            />
            <GaugeCard
              title="KV Cache Usage"
              value={`${metrics.kvCacheUsage.toFixed(1)}%`}
              color={metrics.kvCacheUsage > 80 ? 'text-err' : metrics.kvCacheUsage > 50 ? 'text-warn' : 'text-ok'}
            />
            <GaugeCard
              title="Process Memory"
              value={`${(metrics.residentMemoryMb / 1024).toFixed(1)} GB`}
            />
          </div>

          {/* Latency */}
          <div className="rounded-lg border border-border bg-surface-alt p-4">
            <h3 className="mb-3 text-sm font-medium text-text-muted">Latency</h3>
            <div className="grid grid-cols-3 gap-4">
              <div>
                <p className="text-xs text-text-muted">Time to First Token</p>
                <p className="text-sm">P50: <span className="font-medium">{(metrics.ttftP50 * 1000).toFixed(0)}ms</span></p>
                <p className="text-sm">P95: <span className="font-medium text-warn">{(metrics.ttftP95 * 1000).toFixed(0)}ms</span></p>
              </div>
              <div>
                <p className="text-xs text-text-muted">Inter-Token Latency</p>
                <p className="text-sm">P50: <span className="font-medium">{(metrics.interTokenP50 * 1000).toFixed(0)}ms</span></p>
                <p className="text-sm">P95: <span className="font-medium text-warn">{(metrics.interTokenP95 * 1000).toFixed(0)}ms</span></p>
              </div>
              <div>
                <p className="text-xs text-text-muted">End-to-End</p>
                <p className="text-sm">P50: <span className="font-medium">{(metrics.e2eP50 * 1000).toFixed(0)}ms</span></p>
                <p className="text-sm">P95: <span className="font-medium text-warn">{(metrics.e2eP95 * 1000).toFixed(0)}ms</span></p>
              </div>
            </div>
          </div>

          {/* Tokens & Cache */}
          <div className="grid grid-cols-1 gap-4 lg:grid-cols-2">
            <div className="rounded-lg border border-border bg-surface-alt p-4">
              <h3 className="mb-3 text-sm font-medium text-text-muted">Tokens Processed</h3>
              <div className="space-y-2">
                <div className="flex justify-between text-sm">
                  <span className="text-text-muted">Prompt tokens</span>
                  <span className="font-medium">{metrics.promptTokens.toLocaleString()}</span>
                </div>
                <div className="flex justify-between text-sm">
                  <span className="text-text-muted">Generation tokens</span>
                  <span className="font-medium">{metrics.generationTokens.toLocaleString()}</span>
                </div>
                <div className="flex justify-between text-sm border-t border-border pt-2">
                  <span className="text-text-muted">Preemptions</span>
                  <span className={`font-medium ${metrics.preemptions > 0 ? 'text-warn' : ''}`}>{metrics.preemptions}</span>
                </div>
              </div>
            </div>

            <div className="rounded-lg border border-border bg-surface-alt p-4">
              <h3 className="mb-3 text-sm font-medium text-text-muted">Prefix Cache</h3>
              <div className="space-y-2">
                <div className="flex justify-between text-sm">
                  <span className="text-text-muted">Hit rate</span>
                  <span className="font-medium">{prefixHitRate}%</span>
                </div>
                <div className="flex justify-between text-sm">
                  <span className="text-text-muted">Hits / Queries</span>
                  <span className="font-medium">{metrics.prefixCacheHits} / {metrics.prefixCacheQueries}</span>
                </div>
              </div>
            </div>
          </div>

          {/* Request outcomes */}
          <div className="rounded-lg border border-border bg-surface-alt p-4">
            <h3 className="mb-3 text-sm font-medium text-text-muted">Request Outcomes</h3>
            <div className="flex gap-6 text-sm">
              <div>
                <span className="text-text-muted">Stop: </span>
                <span className="font-medium text-ok">{metrics.requestsStop}</span>
              </div>
              <div>
                <span className="text-text-muted">Length: </span>
                <span className="font-medium">{metrics.requestsLength}</span>
              </div>
              <div>
                <span className="text-text-muted">Abort: </span>
                <span className={`font-medium ${metrics.requestsAbort > 0 ? 'text-warn' : ''}`}>{metrics.requestsAbort}</span>
              </div>
              <div>
                <span className="text-text-muted">Error: </span>
                <span className={`font-medium ${metrics.requestsError > 0 ? 'text-err' : ''}`}>{metrics.requestsError}</span>
              </div>
            </div>
          </div>
        </>
      )}
    </div>
  );
}
