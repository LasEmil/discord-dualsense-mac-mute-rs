import { useEffect, useRef, useState } from "react";
import type { StatusSnapshot } from "./api";

export type ConnectionState = "connecting" | "open" | "closed";

export function useLiveStatus() {
  const [status, setStatus] = useState<StatusSnapshot | null>(null);
  const [connection, setConnection] = useState<ConnectionState>("connecting");
  const retryDelay = useRef(500);

  useEffect(() => {
    let socket: WebSocket | null = null;
    let retryTimer: number | undefined;
    let cancelled = false;

    const connect = () => {
      if (cancelled) return;
      setConnection("connecting");

      const protocol = window.location.protocol === "https:" ? "wss:" : "ws:";
      socket = new WebSocket(`${protocol}//${window.location.host}/ws`);

      socket.onopen = () => {
        retryDelay.current = 500;
        setConnection("open");
      };

      socket.onmessage = (event) => {
        try {
          setStatus(JSON.parse(event.data));
        } catch {
          // Ignore malformed frames rather than crashing the UI.
        }
      };

      socket.onclose = () => {
        setConnection("closed");
        if (cancelled) return;
        retryTimer = window.setTimeout(connect, retryDelay.current);
        retryDelay.current = Math.min(retryDelay.current * 2, 8000);
      };

      socket.onerror = () => {
        socket?.close();
      };
    };

    connect();

    return () => {
      cancelled = true;
      window.clearTimeout(retryTimer);
      socket?.close();
    };
  }, []);

  return { status, connection };
}
