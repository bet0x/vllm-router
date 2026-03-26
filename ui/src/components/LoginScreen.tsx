import { useState } from 'react';

interface Props {
  onLogin: (key: string) => void;
  error: string | null;
}

export default function LoginScreen({ onLogin, error }: Props) {
  const [key, setKey] = useState('');

  const handleSubmit = (e: React.FormEvent) => {
    e.preventDefault();
    if (key.trim()) onLogin(key.trim());
  };

  return (
    <div className="flex h-screen items-center justify-center bg-surface">
      <form onSubmit={handleSubmit} className="w-full max-w-sm space-y-4 rounded-lg border border-border bg-surface-alt p-8">
        <div>
          <h1 className="text-lg font-semibold text-text">vllm-router</h1>
          <p className="text-sm text-text-muted">Enter admin API key to continue</p>
        </div>

        <input
          type="password"
          value={key}
          onChange={(e) => setKey(e.target.value)}
          placeholder="Admin API key"
          autoFocus
          className="w-full rounded-md border border-border bg-surface px-3 py-2 text-sm text-text placeholder-text-muted focus:border-accent focus:outline-none"
        />

        {error && (
          <p className="text-xs text-err">{error}</p>
        )}

        <button
          type="submit"
          disabled={!key.trim()}
          className="w-full rounded-md bg-accent px-3 py-2 text-sm font-medium text-surface hover:bg-accent-dim disabled:opacity-40"
        >
          Connect
        </button>
      </form>
    </div>
  );
}
