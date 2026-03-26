interface Props {
  title: string;
  value: string | number;
  subtitle?: string;
  color?: string;
}

export default function GaugeCard({ title, value, subtitle, color = 'text-accent' }: Props) {
  return (
    <div className="rounded-lg border border-border bg-surface-alt p-4">
      <p className="text-xs text-text-muted">{title}</p>
      <p className={`mt-1 text-2xl font-bold ${color}`}>{value}</p>
      {subtitle && <p className="mt-0.5 text-xs text-text-muted">{subtitle}</p>}
    </div>
  );
}
