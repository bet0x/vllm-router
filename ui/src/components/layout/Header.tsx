import StatusBadge from './StatusBadge';
import type { AdminStats } from '../../lib/types';

interface Props {
  connected: boolean;
  stats?: AdminStats | null;
  onLogout?: () => void;
}

function fmtUptime(secs: number): string {
  const h = Math.floor(secs / 3600);
  const m = Math.floor((secs % 3600) / 60);
  const s = secs % 60;
  if (h > 0) return `${h}h ${m}m`;
  if (m > 0) return `${m}m ${s}s`;
  return `${s}s`;
}

export default function Header({ connected, stats, onLogout }: Props) {
  return (
    <header className="flex items-center justify-between border-b border-border bg-surface px-6 py-3">
      <div className="flex items-center gap-3">
        <h1 className="text-lg font-semibold text-text">vllm-router</h1>
        {stats && (
          <span className="rounded bg-accent/15 px-2 py-0.5 text-xs font-medium text-accent">
            {stats.policies.default}
          </span>
        )}
      </div>
      <div className="flex items-center gap-4">
        {stats && (
          <span className="text-xs text-text-muted">uptime {fmtUptime(stats.uptime_secs)}</span>
        )}
        <StatusBadge ok={connected} label={connected ? 'Connected' : 'Disconnected'} />
        {onLogout && (
          <button
            onClick={onLogout}
            className="rounded px-2 py-1 text-xs text-text-muted hover:bg-surface-hover hover:text-text"
          >
            Logout
          </button>
        )}
      </div>
    </header>
  );
}
