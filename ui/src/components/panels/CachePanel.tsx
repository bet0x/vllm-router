import { PieChart, Pie, Cell, ResponsiveContainer, Tooltip } from 'recharts';
import type { AdminStats, ParsedMetrics } from '../../lib/types';
import { getGauge } from '../../lib/prometheus-parser';
import GaugeCard from '../charts/GaugeCard';

interface Props {
  stats: AdminStats | null;
  parsed: ParsedMetrics | null;
}

const COLORS = ['#38bdf8', '#334155'];

export default function CachePanel({ stats, parsed }: Props) {
  const hits = parsed ? (getGauge(parsed, 'vllm_router_cache_hits_total') ?? 0) : 0;
  const misses = parsed ? (getGauge(parsed, 'vllm_router_cache_misses_total') ?? 0) : 0;
  const total = hits + misses;
  const hitRate = total > 0 ? (hits / total) * 100 : 0;

  const pieData = [
    { name: 'Hits', value: hits },
    { name: 'Misses', value: misses },
  ];

  return (
    <div className="space-y-6">
      <h2 className="text-lg font-semibold">Cache</h2>

      <div className="grid grid-cols-2 gap-4 lg:grid-cols-4">
        <GaugeCard title="Backend" value={stats?.cache.backend ?? '-'} color="text-text" />
        <GaugeCard title="Exact Entries" value={stats?.cache.exact_entries ?? 0} />
        <GaugeCard title="Semantic Entries" value={stats?.cache.semantic_entries ?? 0} />
        <GaugeCard
          title="Hit Rate"
          value={`${hitRate.toFixed(1)}%`}
          color={hitRate > 50 ? 'text-ok' : 'text-warn'}
        />
      </div>

      <div className="grid grid-cols-1 gap-4 lg:grid-cols-2">
        {/* Hit/Miss Ratio */}
        <div className="rounded-lg border border-border bg-surface-alt p-4">
          <h3 className="mb-2 text-sm font-medium text-text-muted">Hit / Miss Ratio</h3>
          {total === 0 ? (
            <p className="flex items-center justify-center text-xs text-text-muted h-48">No cache activity</p>
          ) : (
            <ResponsiveContainer width="100%" height={200}>
              <PieChart>
                <Pie data={pieData} cx="50%" cy="50%" innerRadius={50} outerRadius={80} dataKey="value" label={({ name, percent }) => `${name} ${((percent ?? 0) * 100).toFixed(0)}%`}>
                  {pieData.map((_, i) => (
                    <Cell key={i} fill={COLORS[i]} />
                  ))}
                </Pie>
                <Tooltip contentStyle={{ backgroundColor: '#1e293b', border: '1px solid #334155', borderRadius: 6, fontSize: 12 }} />
              </PieChart>
            </ResponsiveContainer>
          )}
        </div>

        {/* Totals */}
        <div className="rounded-lg border border-border bg-surface-alt p-4">
          <h3 className="mb-4 text-sm font-medium text-text-muted">Cache Totals</h3>
          <div className="space-y-3">
            <div className="flex justify-between text-sm">
              <span className="text-text-muted">Cache Hits</span>
              <span className="font-medium text-ok">{hits}</span>
            </div>
            <div className="flex justify-between text-sm">
              <span className="text-text-muted">Cache Misses</span>
              <span className="font-medium">{misses}</span>
            </div>
            <div className="flex justify-between text-sm border-t border-border pt-3">
              <span className="text-text-muted">Total Lookups</span>
              <span className="font-medium">{total}</span>
            </div>
          </div>
        </div>
      </div>
    </div>
  );
}
