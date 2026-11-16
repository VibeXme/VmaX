import { useState } from "react";
import type { SavedConnections, SavedConnectionUpdate, SavedDevice } from "../types";

interface Props {
  saved: SavedConnections;
  endpoint: string;
  peerRole: "host" | "client" | null;
  canHost: boolean;
  canConnect: boolean;
  paused: boolean;
  friendAccess: Record<string, boolean>;
  onUpdate: (update: SavedConnectionUpdate) => Promise<void>;
  onSelect: (endpoint: string) => void;
  onConnect: (host: SavedDevice) => void;
  onRequest: (host: SavedDevice) => void;
  onResume: () => void;
}

const field = "min-w-0 rounded-lg border border-zinc-700 bg-zinc-950 px-3 py-2 text-sm text-zinc-100";
const action = "rounded-lg border border-zinc-700 px-3 py-2 text-xs font-medium text-zinc-200 hover:bg-zinc-800 disabled:opacity-40";

function normalizeEndpoint(endpoint: string) {
  try {
    return new URL(endpoint).href;
  } catch {
    return endpoint;
  }
}

export function SavedConnectionsPanel({
  saved, endpoint, peerRole, canHost, canConnect, paused, friendAccess, onUpdate, onSelect, onConnect, onRequest, onResume,
}: Props) {
  const [networkName, setNetworkName] = useState("");
  const [newEndpoint, setNewEndpoint] = useState("");
  const [peerName, setPeerName] = useState("");
  const [working, setWorking] = useState(false);
  const [error, setError] = useState("");
  const selected = normalizeEndpoint(endpoint);
  const approvedEndpoints = [...new Set(saved.devices
    .filter((device) => device.role === "client" && device.automatic)
    .map((device) => device.endpoint))];
  const [hostingEndpoint, setHostingEndpoint] = useState("");
  const approvedEndpoint = approvedEndpoints.find((value) => value === hostingEndpoint)
    ?? approvedEndpoints.find((value) => value === selected)
    ?? approvedEndpoints[0];
  const autoConfigured = !!saved.autoHostEndpoint || saved.devices.some((device) => device.automatic && device.role === "host");

  async function update(change: SavedConnectionUpdate) {
    setWorking(true);
    setError("");
    try {
      await onUpdate(change);
      if (change.kind === "remember_peer") setPeerName("");
      if (change.kind === "save_network") {
        setNetworkName("");
        setNewEndpoint("");
      }
    } catch (cause) {
      setError(String(cause));
    } finally {
      setWorking(false);
    }
  }

  return (
    <div className="flex flex-col gap-6 text-sm">
      <section className="order-2 space-y-3">
        <h3 className="font-semibold text-zinc-100">Saved networks</h3>
        <div className="grid gap-2 sm:grid-cols-[1fr_1.5fr_auto]">
          <input className={field} aria-label="Network name" placeholder="Network name" maxLength={80}
            value={networkName} onChange={(event) => setNetworkName(event.target.value)} />
          <input className={field} aria-label="Signaling endpoint to save" placeholder={endpoint}
            value={newEndpoint} onChange={(event) => setNewEndpoint(event.target.value)} />
          <button className={action} type="button" disabled={working || !networkName.trim()}
            onClick={() => void update({ kind: "save_network", name: networkName, endpoint: newEndpoint.trim() || endpoint })}>Save</button>
        </div>
        {saved.networks.map((network) => (
          <div key={network.endpoint} className="flex flex-wrap items-center gap-2 rounded-lg border border-zinc-800 p-3">
            <div className="min-w-0 flex-1">
              <div className="font-medium text-zinc-200">{network.name}</div>
              <div className="break-all text-xs text-zinc-400">{network.endpoint}</div>
            </div>
            <button className={action} type="button" disabled={!canConnect || network.endpoint === selected}
              onClick={() => onSelect(network.endpoint)}>{network.endpoint === selected ? "Selected" : "Use"}</button>
            <button className={action} type="button" disabled={working} aria-label={`Remove ${network.name}`}
              onClick={() => void update({ kind: "remove_network", endpoint: network.endpoint })}>Remove</button>
          </div>
        ))}
      </section>

      <section className="space-y-3">
        <h3 className="font-semibold text-zinc-100">Friends</h3>
        {peerRole && (
          <div className="flex flex-wrap gap-2">
            <input className={`${field} flex-1`} aria-label="Friend name" placeholder="Friend’s name"
              maxLength={80} value={peerName} onChange={(event) => setPeerName(event.target.value)} />
            <button className={action} type="button" disabled={working || !peerName.trim()}
              onClick={() => void update({ kind: "remember_peer", name: peerName })}>Add current {peerRole} as friend</button>
          </div>
        )}
        {!peerRole && saved.devices.length === 0 && (
          <p className="text-xs text-zinc-400">Connect through an invite first, then add your friend here.</p>
        )}
        {saved.devices.map((device) => (
          <div key={`${device.role}:${device.fingerprint}`} className="space-y-2 rounded-lg border border-zinc-800 p-3">
            <div className="flex flex-wrap items-center gap-2">
              <div className="min-w-0 flex-1">
                <div className="font-medium text-zinc-200">{device.name} <span className="text-xs text-zinc-400">· {device.role}</span></div>
                <div className="break-all text-xs text-zinc-400">{device.endpoint}</div>
              </div>
              {device.role === "host" && friendAccess[device.fingerprint] !== false && (
                <button className={action} type="button" disabled={!canConnect} onClick={() => onConnect(device)}>Connect</button>
              )}
              {device.role === "host" && friendAccess[device.fingerprint] !== true && (
                <button className={action} type="button" disabled={!canConnect} onClick={() => onRequest(device)}>Request to join</button>
              )}
              <button className={action} type="button" disabled={working} aria-label={`Remove ${device.name}`}
                onClick={() => void update({ kind: "remove_device", role: device.role, fingerprint: device.fingerprint })}>Remove</button>
            </div>
            <label className="flex items-center gap-2 text-xs text-zinc-300">
              <input type="checkbox" checked={device.automatic} disabled={working}
                onChange={(event) => void update({ kind: "set_device_automatic", role: device.role, fingerprint: device.fingerprint, enabled: event.target.checked })} />
              {device.role === "client" ? "Allow this friend to reconnect" : "Connect automatically when this app is open"}
            </label>
          </div>
        ))}
        {canHost && (approvedEndpoint || saved.autoHostEndpoint) && (
          <div className="flex flex-wrap items-center gap-2">
            {!saved.autoHostEndpoint && approvedEndpoints.length > 1 && (
              <select className={field} aria-label="Approved hosting endpoint" value={approvedEndpoint}
                onChange={(event) => setHostingEndpoint(event.target.value)}>
                {approvedEndpoints.map((value) => <option key={value} value={value}>{value}</option>)}
              </select>
            )}
            <button className={action} type="button" disabled={working}
              onClick={() => void update({ kind: "set_auto_host", endpoint: saved.autoHostEndpoint ? null : approvedEndpoint ?? null })}>
              {saved.autoHostEndpoint ? "Turn off friends-only hosting" : "Enable friends-only hosting"}
            </button>
            {saved.autoHostEndpoint && <span className="break-all text-xs text-zinc-400">{saved.autoHostEndpoint}</span>}
          </div>
        )}
        {saved.devices.length > 0 && (
          <p className="text-xs text-zinc-400">For reconnects over the internet, pair on a permanent WSS relay. The host must allow your device and leave VmaX running in friends-only hosting mode.</p>
        )}
        {paused && autoConfigured && canConnect && (
          <button className={action} type="button" onClick={onResume}>Resume automatic connections</button>
        )}
      </section>
      {error && <p role="alert" className="order-3 text-xs text-red-300">{error}</p>}
    </div>
  );
}
