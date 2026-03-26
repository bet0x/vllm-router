import { useState, useEffect } from 'react';
import { usePolling } from '../../hooks/usePolling';
import { fetchConfig, reloadConfig, flushCache, drainWorker, fetchWorkers, fetchDrainStatus, type DrainStatus } from '../../api/admin';
import { apiPost, apiDelete, clearApiKey } from '../../api/client';

// ── Sub-tab definitions ──

const subTabs = [
  { id: 'config', label: 'Configuration' },
  { id: 'workers', label: 'Worker Management' },
  { id: 'actions', label: 'Actions' },
  { id: 'connection', label: 'Connection' },
] as const;
type SubTab = (typeof subTabs)[number]['id'];

// ── Config section definitions ──

const configSections: { title: string; keys: string[] }[] = [
  { title: 'Server', keys: ['host', 'port', 'connection_mode', 'max_payload_size', 'request_timeout_secs', 'log_level', 'log_dir', 'cors_allowed_origins', 'request_id_headers', 'enable_profiling', 'profile_timeout_secs'] },
  { title: 'Routing', keys: ['mode', 'policy', 'enable_igw', 'model_rules', 'expose_routing_headers', 'intra_node_data_parallel_size'] },
  { title: 'Workers', keys: ['worker_startup_timeout_secs', 'worker_startup_check_interval_secs', 'worker_api_keys'] },
  { title: 'Authentication', keys: ['admin_api_key', 'inbound_api_key', 'api_key', 'api_key_validation_urls', 'api_keys'] },
  { title: 'Health & Resilience', keys: ['health_check', 'retry', 'disable_retries', 'circuit_breaker', 'disable_circuit_breaker'] },
  { title: 'Concurrency & Rate Limiting', keys: ['max_concurrent_requests', 'queue_size', 'queue_timeout_secs', 'rate_limit_tokens_per_second'] },
  { title: 'Cache', keys: ['cache', 'semantic_cache', 'history_backend'] },
  { title: 'Advanced', keys: ['discovery', 'pre_routing_hooks', 'decision_log', 'prompt_cache', 'shared_prefix_routing', 'semantic_cluster', 'model_path', 'tokenizer_path', 'tokenizer_model_map', 'metrics'] },
];

// ── Value renderers ──

function isNull(v: unknown): boolean { return v === null || v === undefined; }
function isEmpty(v: unknown): boolean {
  if (isNull(v)) return true;
  if (Array.isArray(v)) return v.length === 0;
  if (typeof v === 'object') return Object.keys(v as object).length === 0;
  return false;
}

function ValueDisplay({ value }: { value: unknown }) {
  if (isNull(value)) return <span className="text-text-muted italic">not set</span>;
  if (typeof value === 'boolean') return <span className={value ? 'text-ok' : 'text-text-muted'}>{String(value)}</span>;
  if (typeof value === 'number') return <span className="text-accent">{value}</span>;
  if (typeof value === 'string') {
    if (value === '***') return <span className="text-warn">***</span>;
    return <span className="text-text">{value}</span>;
  }
  if (Array.isArray(value)) {
    if (value.length === 0) return <span className="text-text-muted italic">empty</span>;
    if (value.every((v) => typeof v === 'string')) {
      return (
        <div className="flex flex-wrap gap-1">
          {value.map((v, i) => <span key={i} className="rounded bg-surface-hover px-1.5 py-0.5 text-xs">{v as string}</span>)}
        </div>
      );
    }
    return (
      <div className="space-y-2 mt-1">
        {value.map((item, i) => (
          <div key={i} className="rounded border border-border bg-surface p-2">
            <ObjectDisplay obj={item as Record<string, unknown>} />
          </div>
        ))}
      </div>
    );
  }
  if (typeof value === 'object') return <ObjectDisplay obj={value as Record<string, unknown>} />;
  return <span>{String(value)}</span>;
}

function ObjectDisplay({ obj }: { obj: Record<string, unknown> }) {
  const entries = Object.entries(obj).filter(([, v]) => !isNull(v));
  if (entries.length === 0) return <span className="text-text-muted italic">empty</span>;
  return (
    <div className="space-y-1">
      {entries.map(([k, v]) => (
        <div key={k} className="flex gap-2 text-xs">
          <span className="min-w-32 text-text-muted shrink-0">{k}</span>
          <ValueDisplay value={v} />
        </div>
      ))}
    </div>
  );
}

// ── Flash message hook ──

function useFlash() {
  const [msg, setMsg] = useState<{ text: string; ok: boolean } | null>(null);
  const flash = (text: string, ok: boolean) => {
    setMsg({ text, ok });
    setTimeout(() => setMsg(null), 5000);
  };
  return { msg, flash };
}

function Flash({ msg }: { msg: { text: string; ok: boolean } | null }) {
  if (!msg) return null;
  return (
    <div className={`rounded-lg border px-3 py-2 text-sm ${msg.ok ? 'border-ok/30 bg-ok/10 text-ok' : 'border-err/30 bg-err/10 text-err'}`}>
      {msg.text}
    </div>
  );
}

// ── Configuration tab ──

function ConfigTab() {
  const { data: config, error, loading } = usePolling(fetchConfig, 30000);
  const [reloading, setReloading] = useState(false);
  const { msg, flash } = useFlash();

  const handleReload = async () => {
    if (!confirm('Reload configuration from YAML file?')) return;
    setReloading(true);
    try {
      const res = await reloadConfig();
      flash(res.status === 'ok' ? 'Config reloaded successfully' : JSON.stringify(res), true);
    } catch (err) {
      flash(err instanceof Error ? err.message : 'Reload failed', false);
    } finally {
      setReloading(false);
    }
  };

  if (loading) return <p className="text-text-muted">Loading config...</p>;
  if (error) return <p className="text-err">Failed to load config: {error}</p>;
  if (!config) return null;

  const coveredKeys = new Set(configSections.flatMap((s) => s.keys));
  const uncovered = Object.keys(config).filter((k) => !coveredKeys.has(k));

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <span className="text-xs text-text-muted">Secrets are redacted</span>
        <button onClick={handleReload} disabled={reloading}
          className="rounded bg-accent/15 px-3 py-1 text-xs font-medium text-accent hover:bg-accent/25 disabled:opacity-40">
          {reloading ? 'Reloading...' : 'Reload Config'}
        </button>
      </div>
      <Flash msg={msg} />
      <div className="grid grid-cols-1 gap-4 lg:grid-cols-2">
        {configSections.map((section) => {
          const entries = section.keys.filter((k) => k in config && !isEmpty(config[k])).map((k) => [k, config[k]] as const);
          if (entries.length === 0) return null;
          return (
            <div key={section.title} className="rounded-lg border border-border bg-surface-alt p-4">
              <h3 className="mb-3 text-sm font-medium text-accent">{section.title}</h3>
              <div className="space-y-2">
                {entries.map(([k, v]) => (
                  <div key={k}>
                    <div className="text-xs font-medium text-text-muted mb-0.5">{k}</div>
                    <div className="text-sm pl-2"><ValueDisplay value={v} /></div>
                  </div>
                ))}
              </div>
            </div>
          );
        })}
        {uncovered.length > 0 && (
          <div className="rounded-lg border border-border bg-surface-alt p-4">
            <h3 className="mb-3 text-sm font-medium text-accent">Other</h3>
            <div className="space-y-2">
              {uncovered.filter((k) => !isEmpty(config[k])).map((k) => (
                <div key={k}>
                  <div className="text-xs font-medium text-text-muted mb-0.5">{k}</div>
                  <div className="text-sm pl-2"><ValueDisplay value={config[k]} /></div>
                </div>
              ))}
            </div>
          </div>
        )}
      </div>
    </div>
  );
}

// ── Worker Management tab ──

function DrainStatusBadge({ url }: { url: string }) {
  const [status, setStatus] = useState<DrainStatus | null>(null);

  useEffect(() => {
    let active = true;
    const poll = async () => {
      try {
        const s = await fetchDrainStatus(url);
        if (active) setStatus(s);
      } catch {
        if (active) setStatus(null); // worker already removed
      }
    };
    poll();
    const id = setInterval(poll, 2000);
    return () => { active = false; clearInterval(id); };
  }, [url]);

  if (!status) return <span className="text-xs text-text-muted">removed</span>;
  if (!status.draining) return null;

  return (
    <div className="flex items-center gap-2 mt-1">
      <div className="h-1 flex-1 rounded-full bg-surface">
        <div className="h-1 rounded-full bg-warn animate-pulse" style={{ width: status.current_load > 0 ? '60%' : '100%' }} />
      </div>
      <span className="text-xs text-warn">load: {status.current_load}</span>
    </div>
  );
}

function WorkerMgmtTab() {
  const { data: workers } = usePolling(fetchWorkers, 5000);
  const [addUrl, setAddUrl] = useState('');
  const [drainingUrls, setDrainingUrls] = useState<Set<string>>(new Set());
  const [drainTimeout, setDrainTimeout] = useState(300);
  const { msg, flash } = useFlash();

  const handleAdd = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!addUrl.trim()) return;
    try {
      await apiPost('/workers', { url: addUrl.trim() });
      flash(`Added ${addUrl.trim()}`, true);
      setAddUrl('');
    } catch (err) {
      flash(err instanceof Error ? err.message : 'Add failed', false);
    }
  };

  const handleRemove = async (url: string) => {
    if (!confirm(`Remove worker ${url} immediately?`)) return;
    try {
      await apiDelete(`/workers/${encodeURIComponent(url)}`);
      flash(`Removed ${url}`, true);
    } catch (err) {
      flash(err instanceof Error ? err.message : 'Remove failed', false);
    }
  };

  const handleDrain = async (url: string) => {
    if (!confirm(`Drain worker ${url} (timeout: ${drainTimeout}s)?`)) return;
    setDrainingUrls((prev) => new Set(prev).add(url));
    try {
      await drainWorker(url, drainTimeout);
      flash(`Draining ${url} (timeout: ${drainTimeout}s)`, true);
    } catch (err) {
      flash(err instanceof Error ? err.message : 'Drain failed', false);
      setDrainingUrls((prev) => { const next = new Set(prev); next.delete(url); return next; });
    }
  };

  return (
    <div className="space-y-4">
      <Flash msg={msg} />

      {/* Add worker */}
      <div className="rounded-lg border border-border bg-surface-alt p-4">
        <h3 className="mb-3 text-sm font-medium text-accent">Add Worker</h3>
        <form onSubmit={handleAdd} className="flex gap-2">
          <input type="text" value={addUrl} onChange={(e) => setAddUrl(e.target.value)}
            placeholder="http://worker-host:8080"
            className="flex-1 rounded border border-border bg-surface px-2 py-1.5 text-sm text-text placeholder-text-muted focus:border-accent focus:outline-none" />
          <button type="submit" disabled={!addUrl.trim()}
            className="rounded bg-accent px-3 py-1.5 text-xs font-medium text-surface hover:bg-accent-dim disabled:opacity-40">
            Add
          </button>
        </form>
      </div>

      {/* Drain timeout setting */}
      <div className="rounded-lg border border-border bg-surface-alt p-4">
        <h3 className="mb-3 text-sm font-medium text-accent">Drain Settings</h3>
        <div className="flex items-center gap-2">
          <label className="text-xs text-text-muted">Drain timeout (seconds):</label>
          <input type="number" value={drainTimeout} onChange={(e) => setDrainTimeout(Number(e.target.value))} min={10} max={3600}
            className="w-24 rounded border border-border bg-surface px-2 py-1 text-sm text-text focus:border-accent focus:outline-none" />
        </div>
      </div>

      {/* Worker list with actions */}
      <div className="rounded-lg border border-border bg-surface-alt p-4">
        <h3 className="mb-3 text-sm font-medium text-accent">Current Workers</h3>
        {!workers ? (
          <p className="text-xs text-text-muted">Loading...</p>
        ) : workers.workers.length === 0 ? (
          <p className="text-xs text-text-muted">No workers registered</p>
        ) : (
          <div className="space-y-2">
            {workers.workers.map((w) => (
              <div key={w.url} className="rounded border border-border bg-surface p-3">
                <div className="flex items-center justify-between">
                  <div className="flex items-center gap-3">
                    <span className={`h-2 w-2 rounded-full ${w.is_healthy ? 'bg-ok' : 'bg-err'}`} />
                    <div>
                      <span className="font-mono text-xs">{w.url}</span>
                      <div className="flex gap-2 mt-0.5">
                        <span className="text-xs text-text-muted">{w.model_id}</span>
                        <span className="rounded bg-surface-hover px-1 py-0 text-xs text-text-muted">{w.worker_type}</span>
                        {w.draining && <span className="rounded bg-warn/15 px-1 py-0 text-xs text-warn">draining</span>}
                      </div>
                    </div>
                  </div>
                  <div className="flex gap-2">
                    <button onClick={() => handleDrain(w.url)} disabled={w.draining || drainingUrls.has(w.url)}
                      className="rounded bg-warn/15 px-2 py-1 text-xs text-warn hover:bg-warn/25 disabled:opacity-40">
                      {drainingUrls.has(w.url) ? '...' : 'Drain'}
                    </button>
                    <button onClick={() => handleRemove(w.url)}
                      className="rounded bg-err/15 px-2 py-1 text-xs text-err hover:bg-err/25">
                      Remove
                    </button>
                  </div>
                </div>
                {/* Drain status polling */}
                {(w.draining || drainingUrls.has(w.url)) && <DrainStatusBadge url={w.url} />}
              </div>
            ))}
          </div>
        )}
      </div>
    </div>
  );
}

// ── Actions tab ──

function ActionsTab() {
  const [flushing, setFlushing] = useState(false);
  const [reloading, setReloading] = useState(false);
  const { msg, flash } = useFlash();

  const handleFlush = async () => {
    if (!confirm('Flush cache on all workers?')) return;
    setFlushing(true);
    try {
      await flushCache();
      flash('Cache flushed on all workers', true);
    } catch (err) {
      flash(err instanceof Error ? err.message : 'Flush failed', false);
    } finally {
      setFlushing(false);
    }
  };

  const handleReload = async () => {
    if (!confirm('Reload configuration from YAML file?')) return;
    setReloading(true);
    try {
      const res = await reloadConfig();
      flash(res.status === 'ok' ? 'Config reloaded successfully' : JSON.stringify(res), true);
    } catch (err) {
      flash(err instanceof Error ? err.message : 'Reload failed', false);
    } finally {
      setReloading(false);
    }
  };

  return (
    <div className="space-y-4">
      <Flash msg={msg} />
      <div className="grid grid-cols-1 gap-4 lg:grid-cols-2">
        <div className="rounded-lg border border-border bg-surface-alt p-4">
          <h3 className="text-sm font-medium text-accent">Reload Configuration</h3>
          <p className="mt-1 text-xs text-text-muted">Re-read the YAML config file and apply changes (API keys, worker list) without restarting the router.</p>
          <button onClick={handleReload} disabled={reloading}
            className="mt-3 rounded bg-accent/15 px-3 py-1.5 text-xs font-medium text-accent hover:bg-accent/25 disabled:opacity-40">
            {reloading ? 'Reloading...' : 'Reload Config'}
          </button>
        </div>
        <div className="rounded-lg border border-border bg-surface-alt p-4">
          <h3 className="text-sm font-medium text-accent">Flush Cache</h3>
          <p className="mt-1 text-xs text-text-muted">Clear the exact-match and semantic response caches on all workers.</p>
          <button onClick={handleFlush} disabled={flushing}
            className="mt-3 rounded bg-err/15 px-3 py-1.5 text-xs font-medium text-err hover:bg-err/25 disabled:opacity-40">
            {flushing ? 'Flushing...' : 'Flush Cache'}
          </button>
        </div>
      </div>
    </div>
  );
}

// ── Connection tab ──

function ConnectionTab() {
  const [newKey, setNewKey] = useState('');
  const { msg, flash } = useFlash();

  const handleChangeKey = (e: React.FormEvent) => {
    e.preventDefault();
    if (!newKey.trim()) return;
    localStorage.setItem('vllm-router-api-key', newKey.trim());
    flash('API key updated. Refresh to apply.', true);
    setNewKey('');
  };

  const handleLogout = () => {
    clearApiKey();
    window.location.reload();
  };

  return (
    <div className="space-y-4">
      <Flash msg={msg} />
      <div className="rounded-lg border border-border bg-surface-alt p-4">
        <h3 className="mb-3 text-sm font-medium text-accent">API Key</h3>
        <p className="text-xs text-text-muted mb-3">Change the admin API key used for all dashboard requests.</p>
        <form onSubmit={handleChangeKey} className="flex gap-2">
          <input type="password" value={newKey} onChange={(e) => setNewKey(e.target.value)}
            placeholder="New API key"
            className="flex-1 rounded border border-border bg-surface px-2 py-1.5 text-sm text-text placeholder-text-muted focus:border-accent focus:outline-none" />
          <button type="submit" disabled={!newKey.trim()}
            className="rounded bg-accent px-3 py-1.5 text-xs font-medium text-surface hover:bg-accent-dim disabled:opacity-40">
            Update
          </button>
        </form>
      </div>
      <div className="rounded-lg border border-border bg-surface-alt p-4">
        <h3 className="mb-3 text-sm font-medium text-accent">Session</h3>
        <p className="text-xs text-text-muted mb-3">Clear the stored API key and return to the login screen.</p>
        <button onClick={handleLogout}
          className="rounded bg-err/15 px-3 py-1.5 text-xs font-medium text-err hover:bg-err/25">
          Logout
        </button>
      </div>
    </div>
  );
}

// ── Main Settings panel ──

export default function ConfigPanel() {
  const [tab, setTab] = useState<SubTab>('config');

  return (
    <div className="space-y-4">
      <h2 className="text-lg font-semibold">Settings</h2>

      {/* Sub-tabs */}
      <div className="flex gap-1 border-b border-border">
        {subTabs.map((t) => (
          <button key={t.id} onClick={() => setTab(t.id)}
            className={`px-3 py-2 text-sm transition-colors ${
              tab === t.id
                ? 'border-b-2 border-accent text-accent font-medium'
                : 'text-text-muted hover:text-text'
            }`}>
            {t.label}
          </button>
        ))}
      </div>

      {/* Tab content */}
      {tab === 'config' && <ConfigTab />}
      {tab === 'workers' && <WorkerMgmtTab />}
      {tab === 'actions' && <ActionsTab />}
      {tab === 'connection' && <ConnectionTab />}
    </div>
  );
}
