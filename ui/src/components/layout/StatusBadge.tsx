interface Props {
  ok: boolean;
  label?: string;
}

export default function StatusBadge({ ok, label }: Props) {
  return (
    <span
      className={`inline-flex items-center gap-1.5 rounded-full px-2.5 py-0.5 text-xs font-medium ${
        ok
          ? 'bg-ok/15 text-ok'
          : 'bg-err/15 text-err'
      }`}
    >
      <span className={`h-1.5 w-1.5 rounded-full ${ok ? 'bg-ok' : 'bg-err'}`} />
      {label ?? (ok ? 'Healthy' : 'Down')}
    </span>
  );
}
