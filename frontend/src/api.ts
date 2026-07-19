export interface ListenerStatus {
  running: boolean;
  lastError: string | null;
}

export interface StatusSnapshot {
  ok: boolean;
  pid: number;
  uptimeSeconds: number;
  api: string;
  muted: boolean | null;
  controllerConnected: boolean;
  listener: ListenerStatus | null;
}

export interface ConfigStatus {
  ok: boolean;
  configured: boolean;
  configPath: string;
  tokenPath: string;
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(path, {
    headers: { "content-type": "application/json" },
    ...init,
  });
  const body = await res.json();
  if (!res.ok || body.ok === false) {
    throw new Error(body.error ?? `Request to ${path} failed (${res.status})`);
  }
  return body as T;
}

export const api = {
  status: () => request<StatusSnapshot>("/status"),

  getConfig: () => request<ConfigStatus>("/config"),

  saveConfig: (clientId: string, clientSecret: string) =>
    request<{ ok: boolean; path: string }>("/config", {
      method: "PUT",
      body: JSON.stringify({ clientId, clientSecret }),
    }),

  toggleMute: () =>
    request<{ ok: boolean; muted: boolean }>("/discord/toggle", { method: "POST" }),

  startListener: () =>
    request<{ ok: boolean; listener: ListenerStatus }>("/listeners/mute", {
      method: "POST",
    }),
};
