import { FormEvent, useEffect, useState } from "react";
import { api, ConfigStatus } from "./api";
import { useLiveStatus } from "./useLiveStatus";

function useAsyncAction() {
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const run = async (fn: () => Promise<void>) => {
    setPending(true);
    setError(null);
    try {
      await fn();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setPending(false);
    }
  };

  return { run, pending, error, setError };
}

function formatUptime(seconds: number): string {
  const h = Math.floor(seconds / 3600);
  const m = Math.floor((seconds % 3600) / 60);
  const s = seconds % 60;
  return [h, m, s].map((n) => String(n).padStart(2, "0")).join(":");
}

function LightBar({
  muted,
  listenerRunning,
  pulseKey,
}: {
  muted: boolean | null;
  listenerRunning: boolean;
  pulseKey: number;
}) {
  const mode = muted ? "muted" : listenerRunning ? "listening" : "idle";
  return (
    <div className={`light-bar light-bar--${mode}`} key={pulseKey} aria-hidden="true">
      <span className="light-bar__sweep" />
    </div>
  );
}

function Card({
  title,
  eyebrow,
  children,
}: {
  title: string;
  eyebrow: string;
  children: React.ReactNode;
}) {
  return (
    <section className="card">
      <header className="card__header">
        <p className="card__eyebrow">{eyebrow}</p>
        <h2 className="card__title">{title}</h2>
      </header>
      <div className="card__body">{children}</div>
    </section>
  );
}

export default function App() {
  const { status, connection } = useLiveStatus();
  const [pulseKey, setPulseKey] = useState(0);

  const [config, setConfig] = useState<ConfigStatus | null>(null);
  const [clientId, setClientId] = useState("");
  const [clientSecret, setClientSecret] = useState("");
  const configAction = useAsyncAction();

  const muteAction = useAsyncAction();
  const listenerAction = useAsyncAction();

  useEffect(() => {
    api.getConfig().then(setConfig).catch(() => {});
  }, []);

  // Flash the light bar signature briefly whenever mute state flips.
  useEffect(() => {
    setPulseKey((k) => k + 1);
  }, [status?.muted]);

  function saveConfig(event: FormEvent) {
    event.preventDefault();
    configAction.run(async () => {
      const res = await api.saveConfig(clientId, clientSecret);
      setConfig((prev) => (prev ? { ...prev, configured: true, configPath: res.path } : prev));
      setClientId("");
      setClientSecret("");
    });
  }

  function toggleMute() {
    muteAction.run(async () => {
      await api.toggleMute();
    });
  }

  function startListener() {
    listenerAction.run(async () => {
      await api.startListener();
    });
  }

  const listener = status?.listener ?? null;
  const muted = status?.muted ?? null;
  const controllerConnected = Boolean(status?.controllerConnected);

  return (
    <div className="page">
      <LightBar muted={muted} listenerRunning={Boolean(listener?.running)} pulseKey={pulseKey} />

      <header className="topbar">
        <div className="topbar__title">
          <span className="topbar__mark">◈</span>
          <div>
            <h1>discord-mute-rs</h1>
            <p className="topbar__subtitle">DualSense &amp; Discord mic control</p>
          </div>
        </div>
        <div className={`conn conn--${connection}`}>
          <span className="conn__dot" />
          {connection === "open" ? "live" : connection === "connecting" ? "connecting…" : "offline"}
        </div>
      </header>

      <main className="grid">
        <Card eyebrow="Mic" title="Mute control">
          <button
            className={`mute-button ${muted ? "mute-button--muted" : ""}`}
            onClick={toggleMute}
            disabled={muteAction.pending}
          >
            <span className="mute-button__icon">{muted ? "🔇" : "🎙️"}</span>
            {muted === null ? "Toggle mute" : muted ? "Muted — tap to unmute" : "Live — tap to mute"}
          </button>
          {muteAction.error && <p className="error-text">{muteAction.error}</p>}
          <p className="hint">
            Also flips whenever the DualSense mic button is pressed while a listener is
            running.
          </p>
        </Card>

        <Card eyebrow="Controller" title="Mic-button listener">
          <div className="listener-modes">
            <button
              className={`mode-button ${listener?.running ? "mode-button--active" : ""}`}
              onClick={startListener}
              disabled={listenerAction.pending || Boolean(listener?.running)}
            >
              {listener?.running ? "Listening" : "Start listening"}
            </button>
          </div>
          <div className="listener-status">
            <span
              className={`status-pill ${listener?.running && controllerConnected ? "status-pill--on" : ""}`}
            >
              {!listener?.running
                ? "stopped"
                : controllerConnected
                  ? "running · controller connected"
                  : "running · waiting for controller…"}
            </span>
          </div>
          {listener?.lastError && <p className="error-text">{listener.lastError}</p>}
          {listenerAction.error && <p className="error-text">{listenerAction.error}</p>}
          <p className="hint">
            The listener survives unplugging the controller — reconnect it and it picks
            up again. Stop the listener by stopping the server (Ctrl-C).
          </p>
        </Card>

        <Card eyebrow="Server" title="Status">
          {status ? (
            <dl className="kv">
              <dt>uptime</dt>
              <dd className="mono">{formatUptime(status.uptimeSeconds)}</dd>
              <dt>pid</dt>
              <dd className="mono">{status.pid}</dd>
              <dt>listening on</dt>
              <dd className="mono">{status.api}</dd>
            </dl>
          ) : (
            <p className="hint">Waiting for the server…</p>
          )}
        </Card>

        <Card eyebrow="Discord" title="Application credentials">
          {config?.configured ? (
            <p className="hint">
              Configured. Stored at <span className="mono">{config.configPath}</span>
            </p>
          ) : (
            <p className="hint">Not configured yet — paste your Discord app keys below.</p>
          )}
          <form className="config-form" onSubmit={saveConfig}>
            <label>
              Client ID
              <input
                value={clientId}
                onChange={(e) => setClientId(e.target.value)}
                placeholder="1234567890123456789"
                autoComplete="off"
                required
              />
            </label>
            <label>
              Client secret
              <input
                type="password"
                value={clientSecret}
                onChange={(e) => setClientSecret(e.target.value)}
                placeholder="••••••••••••••••"
                autoComplete="off"
                required
              />
            </label>
            <button type="submit" disabled={configAction.pending}>
              Save credentials
            </button>
          </form>
          {configAction.error && <p className="error-text">{configAction.error}</p>}
        </Card>
      </main>
    </div>
  );
}
