import {
  LineChart,
  Line,
  XAxis,
  YAxis,
  Tooltip,
  ResponsiveContainer,
  CartesianGrid,
} from 'recharts';

interface Series {
  dataKey: string;
  color: string;
  label?: string;
}

interface Props {
  data: Record<string, number>[];
  series: Series[];
  height?: number;
  yLabel?: string;
}

function fmtTime(ts: number) {
  const d = new Date(ts);
  return `${d.getHours().toString().padStart(2, '0')}:${d.getMinutes().toString().padStart(2, '0')}:${d.getSeconds().toString().padStart(2, '0')}`;
}

export default function SimpleLineChart({ data, series, height = 200, yLabel }: Props) {
  if (data.length === 0) {
    return (
      <div className="flex items-center justify-center text-text-muted text-xs" style={{ height }}>
        Waiting for data...
      </div>
    );
  }

  return (
    <ResponsiveContainer width="100%" height={height}>
      <LineChart data={data} margin={{ top: 4, right: 8, bottom: 0, left: 0 }}>
        <CartesianGrid strokeDasharray="3 3" stroke="#334155" />
        <XAxis
          dataKey="time"
          tickFormatter={fmtTime}
          tick={{ fontSize: 10, fill: '#94a3b8' }}
          stroke="#334155"
        />
        <YAxis
          tick={{ fontSize: 10, fill: '#94a3b8' }}
          stroke="#334155"
          label={yLabel ? { value: yLabel, angle: -90, position: 'insideLeft', style: { fontSize: 10, fill: '#94a3b8' } } : undefined}
        />
        <Tooltip
          contentStyle={{ backgroundColor: '#1e293b', border: '1px solid #334155', borderRadius: 6, fontSize: 12 }}
          labelFormatter={(v) => fmtTime(Number(v))}
          formatter={(value) => Number(value).toFixed(3)}
        />
        {series.map((s) => (
          <Line
            key={s.dataKey}
            type="monotone"
            dataKey={s.dataKey}
            stroke={s.color}
            strokeWidth={2}
            dot={false}
            name={s.label ?? s.dataKey}
          />
        ))}
      </LineChart>
    </ResponsiveContainer>
  );
}
