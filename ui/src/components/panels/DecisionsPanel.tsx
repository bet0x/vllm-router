import type { Decision } from '../../lib/types';

interface Props {
  decisions: Decision[];
}

function statusColor(status: number): string {
  if (status >= 200 && status < 300) return 'text-ok';
  if (status >= 400 && status < 500) return 'text-warn';
  return 'text-err';
}

function rowBg(status: number): string {
  if (status >= 500) return 'bg-err/5';
  if (status >= 400) return 'bg-warn/5';
  return '';
}

function fmtTime(ts: string): string {
  try {
    const d = new Date(ts);
    return d.toLocaleTimeString();
  } catch {
    return ts;
  }
}

export default function DecisionsPanel({ decisions }: Props) {
  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <h2 className="text-lg font-semibold">Routing Decisions</h2>
        <span className="text-xs text-text-muted">{decisions.length} entries</span>
      </div>

      {decisions.length === 0 ? (
        <div className="rounded-lg border border-border bg-surface-alt p-8 text-center text-text-muted">
          No routing decisions yet. Send some requests to the router.
        </div>
      ) : (
        <div className="overflow-x-auto rounded-lg border border-border">
          <table className="w-full text-sm">
            <thead className="bg-surface-alt text-left text-xs text-text-muted">
              <tr>
                <th className="px-3 py-2">Time</th>
                <th className="px-3 py-2">Route</th>
                <th className="px-3 py-2">Model</th>
                <th className="px-3 py-2">Method</th>
                <th className="px-3 py-2">Policy</th>
                <th className="px-3 py-2">Worker</th>
                <th className="px-3 py-2">Cache</th>
                <th className="px-3 py-2">Status</th>
                <th className="px-3 py-2">Duration</th>
                <th className="px-3 py-2">Tenant</th>
              </tr>
            </thead>
            <tbody>
              {decisions.map((d, i) => (
                <tr key={i} className={`border-t border-border hover:bg-surface-hover/50 transition-colors ${rowBg(d.status)}`}>
                  <td className="px-3 py-1.5 text-xs text-text-muted whitespace-nowrap">{fmtTime(d.timestamp)}</td>
                  <td className="px-3 py-1.5 font-mono text-xs">{d.route}</td>
                  <td className="px-3 py-1.5 text-xs truncate max-w-32" title={d.model}>{d.model ?? '-'}</td>
                  <td className="px-3 py-1.5">
                    <span className="rounded bg-surface-hover px-1.5 py-0.5 text-xs">{d.method ?? '-'}</span>
                  </td>
                  <td className="px-3 py-1.5 text-xs">{d.policy ?? '-'}</td>
                  <td className="px-3 py-1.5 font-mono text-xs truncate max-w-40" title={d.worker}>{d.worker ?? '-'}</td>
                  <td className="px-3 py-1.5 text-xs">{d.cache_status ?? '-'}</td>
                  <td className={`px-3 py-1.5 text-xs font-medium ${statusColor(d.status)}`}>{d.status}</td>
                  <td className="px-3 py-1.5 text-xs">{d.duration_ms}ms</td>
                  <td className="px-3 py-1.5 text-xs">{d.tenant ?? '-'}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}
