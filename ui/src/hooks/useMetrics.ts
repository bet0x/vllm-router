import { useCallback, useEffect, useRef, useState } from 'react';
import { fetchMetricsText } from '../api/client';
import { parsePrometheus } from '../lib/prometheus-parser';
import { MetricStore } from '../lib/metric-store';
import type { ParsedMetrics, TimePoint } from '../lib/types';
import {
  getGauge,
  getSamples,
  histogramPercentile,
} from '../lib/prometheus-parser';

/**
 * Poll /metrics and maintain time-series history.
 * Returns a tick counter that increments on every scrape so consumers re-render.
 */
export function useMetrics(intervalMs = 5000) {
  const storeRef = useRef(new MetricStore());
  const [parsed, setParsed] = useState<ParsedMetrics | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [tick, setTick] = useState(0);
  const prevCounters = useRef<Record<string, number>>({});

  const scrape = useCallback(async () => {
    try {
      const text = await fetchMetricsText();
      const p = parsePrometheus(text);
      setParsed(p);
      setError(null);

      const now = Date.now();
      const store = storeRef.current;

      // Compute request rate (delta of counter)
      const totalRequests =
        getSamples(p, 'vllm_router_requests_total').reduce(
          (sum, s) => sum + s.value,
          0,
        );
      const totalErrors =
        getSamples(p, 'vllm_router_request_errors_total').reduce(
          (sum, s) => sum + s.value,
          0,
        );
      const prev = prevCounters.current;
      const dt = intervalMs / 1000;

      const rps =
        prev.requests !== undefined
          ? Math.max(0, (totalRequests - prev.requests) / dt)
          : 0;
      const errRate =
        prev.errors !== undefined
          ? Math.max(0, (totalErrors - prev.errors) / dt)
          : 0;

      prev.requests = totalRequests;
      prev.errors = totalErrors;

      // Latency percentiles
      const p50 =
        histogramPercentile(p, 'vllm_router_generate_duration_seconds', 50) ?? 0;
      const p95 =
        histogramPercentile(p, 'vllm_router_generate_duration_seconds', 95) ?? 0;
      const p99 =
        histogramPercentile(p, 'vllm_router_generate_duration_seconds', 99) ?? 0;

      const point: TimePoint = { time: now, rps, errRate, p50, p95, p99 };
      store.push('overview', point);

      // Per-route RPS
      const routeSamples = getSamples(p, 'vllm_router_requests_total');
      for (const s of routeSamples) {
        const route = s.labels.route ?? 'unknown';
        const prevKey = `route:${route}`;
        const prevVal = prev[prevKey];
        const routeRps =
          prevVal !== undefined ? Math.max(0, (s.value - prevVal) / dt) : 0;
        prev[prevKey] = s.value;
        store.push('routes', { time: now, [route]: routeRps } as TimePoint);
      }

      // Cache hits/misses
      const cacheHits = getGauge(p, 'vllm_router_cache_hits_total') ?? 0;
      const cacheMisses = getGauge(p, 'vllm_router_cache_misses_total') ?? 0;
      store.push('cache', { time: now, hits: cacheHits, misses: cacheMisses });

      // Bump tick so consumers re-render with fresh store data
      setTick((t) => t + 1);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }, [intervalMs]);

  useEffect(() => {
    scrape();
    const id = setInterval(scrape, intervalMs);
    return () => clearInterval(id);
  }, [scrape, intervalMs]);

  return { parsed, store: storeRef.current, error, tick };
}
