import type { WorkersResponse, WorkerInfo, ParsedMetrics } from '../../lib/types';
import { getSample } from '../../lib/prometheus-parser';
import StatusBadge from '../layout/StatusBadge';
import WorkerDetail from './WorkerDetail';
import { useState } from 'react';

interface Props {
  workers: WorkersResponse | null;
  parsed: ParsedMetrics | null;
}

function cbLabel(state: number | undefined): { text: string; color: string } {
  switch (state) {
    case 0: return { text: 'Closed', color: 'text-ok' };
    case 1: return { text: 'Open', color: 'text-err' };
    case 2: return { text: 'Half-Open', color: 'text-warn' };
    default: return { text: '-', color: 'text-text-muted' };
  }
}

export default function WorkersPanel({ workers, parsed }: Props) {
  const [selected, setSelected] = useState<WorkerInfo | null>(null);

  if (!workers) return <p className="text-text-muted">Loading workers...</p>;

  if (selected) {
    return <WorkerDetail worker={selected} onClose={() => setSelected(null)} />;
  }

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <h2 className="text-lg font-semibold">Workers</h2>
        <div className="flex gap-2 text-xs text-text-muted">
          <span>{workers.stats.regular_count} regular</span>
          {workers.stats.prefill_count > 0 && <span>{workers.stats.prefill_count} prefill</span>}
          {workers.stats.decode_count > 0 && <span>{workers.stats.decode_count} decode</span>}
        </div>
      </div>

      <div className="overflow-x-auto rounded-lg border border-border">
        <table className="w-full text-sm">
          <thead className="bg-surface-alt text-left text-xs text-text-muted">
            <tr>
              <th className="px-4 py-2">URL</th>
              <th className="px-4 py-2">Model</th>
              <th className="px-4 py-2">Type</th>
              <th className="px-4 py-2">Health</th>
              <th className="px-4 py-2">Circuit Breaker</th>
              <th className="px-4 py-2">Load</th>
              <th className="px-4 py-2">Requests</th>
              <th className="px-4 py-2">Priority</th>
            </tr>
          </thead>
          <tbody>
            {workers.workers.map((w) => {
              const cbState = parsed ? getSample(parsed, 'vllm_router_cb_state', { worker: w.url }) : undefined;
              const cb = cbLabel(cbState);
              const processed = parsed ? getSample(parsed, 'vllm_router_processed_requests_total', { worker: w.url }) : undefined;
              const running = parsed ? (getSample(parsed, 'vllm_router_running_requests', { worker: w.url }) ?? 0) : w.load;

              return (
                <tr
                  key={w.url}
                  onClick={() => setSelected(w)}
                  className="border-t border-border hover:bg-surface-hover/50 transition-colors cursor-pointer"
                >
                  <td className="px-4 py-2 font-mono text-xs text-accent">{w.url}</td>
                  <td className="px-4 py-2 text-xs truncate max-w-48" title={w.model_id}>{w.model_id}</td>
                  <td className="px-4 py-2">
                    <span className="rounded bg-surface-hover px-1.5 py-0.5 text-xs">{w.worker_type}</span>
                  </td>
                  <td className="px-4 py-2">
                    <StatusBadge ok={w.is_healthy} label={w.draining ? 'Draining' : undefined} />
                  </td>
                  <td className={`px-4 py-2 text-xs ${cb.color}`}>{cb.text}</td>
                  <td className="px-4 py-2">
                    <div className="flex items-center gap-2">
                      <div className="h-1.5 w-16 rounded-full bg-surface">
                        <div
                          className="h-1.5 rounded-full bg-accent"
                          style={{ width: `${Math.min(100, running * 10)}%` }}
                        />
                      </div>
                      <span className="text-xs text-text-muted">{running}</span>
                    </div>
                  </td>
                  <td className="px-4 py-2 text-xs">{processed ?? '-'}</td>
                  <td className="px-4 py-2 text-xs">{w.priority}</td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>

      <p className="text-xs text-text-muted">Click a worker row to see vLLM engine metrics. Manage workers in Settings.</p>
    </div>
  );
}
