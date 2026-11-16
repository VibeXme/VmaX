import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";

export function HostSetup({ disabled }: { disabled: boolean }) {
  const [pending, setPending] = useState(false);
  const [status, setStatus] = useState("");
  async function run(command: string) {
    setPending(true);
    setStatus("Checking controller setup…");
    try {
      const result = await invoke<string>(command);
      setStatus(result);
    } catch (error) { setStatus(String(error)); }
    finally { setPending(false); }
  }
  return <div className="mt-3 space-y-2 text-xs text-zinc-400">
    <div className="flex flex-wrap gap-2">
      {[["controller_driver_status", "Check controller driver"], ["install_controller_driver", "Install controller driver"]].map(([command, label]) =>
        <button key={command} type="button" disabled={disabled || pending} onClick={() => void run(command)} className="rounded-lg border border-zinc-700 px-3 py-2 text-zinc-200 disabled:opacity-50">{label}</button>)}
    </div>
    <p role="status" aria-live="polite">{status}</p>
  </div>;
}
