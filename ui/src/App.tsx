import { useCallback, useState } from 'react';
import Header from './components/layout/Header';
import Sidebar, { type TabId } from './components/layout/Sidebar';
import LoginScreen from './components/LoginScreen';
import OverviewPanel from './components/panels/OverviewPanel';
import WorkersPanel from './components/panels/WorkersPanel';
import RequestsPanel from './components/panels/RequestsPanel';
import CachePanel from './components/panels/CachePanel';
import TenantsPanel from './components/panels/TenantsPanel';
import DecisionsPanel from './components/panels/DecisionsPanel';
import ConfigPanel from './components/panels/ConfigPanel';
import { usePolling } from './hooks/usePolling';
import { useMetrics } from './hooks/useMetrics';
import { fetchStats, fetchWorkers, fetchDecisions, fetchTenants } from './api/admin';
import { getApiKey, setApiKey, clearApiKey } from './api/client';

export default function App() {
  const [authed, setAuthed] = useState(!!getApiKey());
  const [loginError, setLoginError] = useState<string | null>(null);
  const [tab, setTab] = useState<TabId>('overview');

  const handleLogin = useCallback(async (key: string) => {
    setApiKey(key);
    try {
      // Validate the key by calling /admin/stats
      const res = await fetch('/api/admin/stats', {
        headers: { 'X-Admin-Key': key },
      });
      if (res.status === 401) {
        clearApiKey();
        setLoginError('Invalid API key');
        return;
      }
      if (!res.ok) {
        clearApiKey();
        setLoginError(`Connection failed: ${res.status}`);
        return;
      }
      setLoginError(null);
      setAuthed(true);
    } catch (err) {
      clearApiKey();
      setLoginError(err instanceof Error ? err.message : 'Connection failed');
    }
  }, []);

  const handleLogout = useCallback(() => {
    clearApiKey();
    setAuthed(false);
  }, []);

  if (!authed) {
    return <LoginScreen onLogin={handleLogin} error={loginError} />;
  }

  return <Dashboard tab={tab} setTab={setTab} onLogout={handleLogout} />;
}

// Separate component so hooks only run when authed
function Dashboard({
  tab,
  setTab,
  onLogout,
}: {
  tab: TabId;
  setTab: (t: TabId) => void;
  onLogout: () => void;
}) {
  const { data: stats } = usePolling(fetchStats, 5000);
  const { data: workers } = usePolling(fetchWorkers, 5000);
  const { data: decisions } = usePolling(() => fetchDecisions(100), 5000);
  const { data: tenants } = usePolling(fetchTenants, 10000);
  const { parsed, store, tick } = useMetrics(5000);

  // tick changes every scrape — we read store snapshots here so React sees new arrays
  const overviewData = tick >= 0 ? store.get('overview') : [];
  const connected = !!stats && !!parsed;

  return (
    <div className="flex h-screen flex-col">
      <Header connected={connected} stats={stats} onLogout={onLogout} />
      <div className="flex flex-1 overflow-hidden">
        <Sidebar active={tab} onSelect={setTab} />
        <main className="flex-1 overflow-y-auto p-6">
          {tab === 'overview' && (
            <OverviewPanel stats={stats} parsed={parsed} overviewData={overviewData} />
          )}
          {tab === 'workers' && (
            <WorkersPanel workers={workers} parsed={parsed} />
          )}
          {tab === 'requests' && (
            <RequestsPanel parsed={parsed} overviewData={overviewData} />
          )}
          {tab === 'cache' && (
            <CachePanel stats={stats} parsed={parsed} />
          )}
          {tab === 'tenants' && (
            <TenantsPanel tenants={tenants} parsed={parsed} />
          )}
          {tab === 'decisions' && (
            <DecisionsPanel decisions={decisions?.decisions ?? []} />
          )}
          {tab === 'config' && (
            <ConfigPanel />
          )}
        </main>
      </div>
    </div>
  );
}
