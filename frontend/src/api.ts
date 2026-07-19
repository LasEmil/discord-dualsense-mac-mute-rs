export type ListenerMode = "mute" | "pushToTalk";

export interface ListenerStatus {
  mode: ListenerMode;
  running: boolean;
  lastError: string | null;
}

export interface StatusSnapshot {
  ok: boolean;
  pid: number;
  uptimeSeconds: number;
  api: string;
  muted: boolean | null;
  listener: ListenerStatus | null;
}

export interface ConfigStatus {
  ok: boolean;
  configured: boolean;
  configPath: string;
  tokenPath: string;
}

export interface DeviceInfo {
  [key: string]: unknown;
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

  devices: () => request<{ ok: boolean; devices: DeviceInfo[] }>("/devices"),

  toggleMute: () =>
    request<{ ok: boolean; muted: boolean }>("/discord/toggle", { method: "POST" }),

  setLed: (muted: boolean) =>
    request<{ ok: boolean }>("/controller/led", {
      method: "POST",
      body: JSON.stringify({ muted }),
    }),

  startListener: (mode: ListenerMode) =>
    request<{ ok: boolean; listener: ListenerStatus }>(
      mode === "mute" ? "/listeners/mute" : "/listeners/ptt",
      { method: "POST" },
    ),

  stopListener: () =>
    request<{ ok: boolean; stopped: boolean }>("/listeners/current", {
      method: "DELETE",
    }),
};
