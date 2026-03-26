const tabs = [
  { id: 'overview', label: 'Overview', icon: '{}' },
  { id: 'workers', label: 'Workers', icon: '[]' },
  { id: 'requests', label: 'Requests', icon: '>>' },
  { id: 'cache', label: 'Cache', icon: '##' },
  { id: 'tenants', label: 'Tenants', icon: '@@' },
  { id: 'decisions', label: 'Decisions', icon: '->' },
  { id: 'config', label: 'Settings', icon: '<>' },
] as const;

export type TabId = (typeof tabs)[number]['id'];

interface Props {
  active: TabId;
  onSelect: (id: TabId) => void;
}

export default function Sidebar({ active, onSelect }: Props) {
  return (
    <nav className="flex w-48 flex-col gap-1 border-r border-border bg-surface px-2 py-4">
      {tabs.map((t) => (
        <button
          key={t.id}
          onClick={() => onSelect(t.id)}
          className={`flex items-center gap-2 rounded-md px-3 py-2 text-sm transition-colors ${
            active === t.id
              ? 'bg-accent/15 text-accent font-medium'
              : 'text-text-muted hover:bg-surface-hover hover:text-text'
          }`}
        >
          <span className="font-mono text-xs opacity-60">{t.icon}</span>
          {t.label}
        </button>
      ))}
    </nav>
  );
}
