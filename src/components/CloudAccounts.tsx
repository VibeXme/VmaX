import { createPortal } from "react-dom";
import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

type Device = { device_id: string; owner_id: string; device_name: string; can_host: boolean; last_seen: string; platform: string; status: string };
type Friend = { id: string; requester_id: string; addressee_id: string; status: string; auto_approve_join: boolean; requester_auto_approve: boolean };
type Profile = { id: string; username: string; tag: string };
type Snapshot = { user_id: string; device_id: string; available: boolean; profiles: Profile[]; devices: Device[]; friendships: Friend[] };
type Incoming = { id: string; name: string; friend: boolean };

export function CloudAccounts({ section, canHost, hostParams, join, active, container, onAccountNeeded, onOpenComputers, onOpenFriends }: {
  section: "computers" | "friends" | "accounts" | null;
  canHost: boolean;
  hostParams: () => unknown;
  join: (invite: string) => Promise<void>;
  active: boolean;
  container: HTMLDivElement | null;
  onAccountNeeded: () => void;
  onOpenComputers: () => void;
  onOpenFriends: () => void;
}) {
  const [data, setData] = useState<Snapshot | null>(null);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [username, setUsername] = useState("");
  const [tag, setTag] = useState("");
  const [handle, setHandle] = useState("");
  const [incoming, setIncoming] = useState<Incoming[]>([]);
  const [copied, setCopied] = useState(false);
  const [autostart, setAutostart] = useState(false);
  const [project, setProject] = useState<string | null | undefined>(undefined);
  const [projectUrl, setProjectUrl] = useState("");
  const [projectKey, setProjectKey] = useState("");
  const [configure, setConfigure] = useState(false);
  const [editProfile, setEditProfile] = useState(false);
  const [loaded, setLoaded] = useState(false);
  const [loadError, setLoadError] = useState("");
  const [notice, setNotice] = useState("");
  const [queuedLink, setQueuedLink] = useState<string | null>(null);

  async function refresh() {
    try {
      setData(await invoke<Snapshot>("cloud_snapshot"));
      setLoadError("");
    } catch (error) {
      const message = String(error);
      if (message === "Sign in to VmaX first") {
        setData(null);
        setLoadError("");
      } else {
        setLoadError(message);
      }
    } finally {
      setLoaded(true);
    }
  }
  async function checkProject() {
    const url = await invoke<string | null>("cloud_project_url");
    setProject(url);
    if (url) await refresh();
  }
  async function run(action: () => Promise<unknown>) {
    setBusy(true);
    setError("");
    setNotice("");
    try { await action(); } catch (error) { setError(String(error)); }
    finally { setBusy(false); }
  }
  async function connect(target: string, invite?: string) {
    const route = await invoke<string>("cloud_connect", { target, invite });
    await join(route);
  }

  useEffect(() => {
    if (!("__TAURI_INTERNALS__" in window)) return;
    let current = true;
    void checkProject().catch((error) => setError(String(error)));
    void invoke<boolean>("autostart_enabled").then(setAutostart).catch(() => {});
    const poll = window.setInterval(() => void checkProject().catch(() => {}), 12_000);
    const subscriptions = [
      listen("vmax://cloud-updated", () => { if (current) void refresh(); }),
      listen<Incoming>("vmax://join-request", ({ payload }) => {
        if (current) setIncoming((all) => [...all.filter((item) => item.id !== payload.id), payload]);
      }),
      listen<string>("vmax://cloud-error", ({ payload }) => { if (current) setError(payload); }),
    ];
    async function loadLink() {
      const link = await invoke<string | null>("take_deep_link");
      if (current && link) setQueuedLink(link);
    }
    subscriptions.push(listen("vmax://deep-link-ready", () => void loadLink()));
    void loadLink().catch(() => {});
    return () => {
      current = false;
      clearInterval(poll);
      for (const subscription of subscriptions) void subscription.then((unlisten) => unlisten()).catch(() => {});
    };
  }, []);

  useEffect(() => {
    if (!queuedLink || active || busy) return;
    if (queuedLink.startsWith("vmax://join/cloud/") && !data?.profiles.some((p) => p.id === data.user_id)) { onAccountNeeded(); return; }
    const link = queuedLink;
    setQueuedLink(null);
    void run(() => link.startsWith("vmax://join/cloud/") ? connect("", link) : join(link));
  }, [queuedLink, active, busy, data]);

  const online = (device: Device) => Date.now() - Date.parse(device.last_seen) < 60_000;
  const profile = data?.profiles.find((p) => p.id === data.user_id);
  const devices = (owner: string) => (data?.devices ?? []).filter((d) => d.owner_id === owner);
  function rows(owner: string, label: string) {
    return devices(owner).map((device) => (
      <div className="flex flex-wrap items-center justify-between gap-2 rounded-lg bg-zinc-900 p-3 text-zinc-100" key={device.device_id}>
        <div><div>{device.device_name}</div><small className={online(device) ? "text-green-400" : "text-zinc-400"}>
          {online(device) ? device.status.replace("_", " ") : "Offline"} · {device.platform}
        </small></div>
        {device.device_id !== data?.device_id && device.can_host && online(device) &&
          <button className="vmax-primary" disabled={busy || active} onClick={() => void run(() => connect(device.device_id))}>{label}</button>}
      </div>
    ));
  }
  return <>
    {incoming.map((request) => (
      <div className="fixed left-4 top-16 z-[120] max-w-sm space-y-2 rounded-xl border border-zinc-700 bg-zinc-950 p-4 text-zinc-100" key={request.id} role="alert">
        <p>{request.name} wants to connect</p>
        <div className="flex flex-wrap gap-2">
          {(["accept", ...(request.friend ? ["always_allow"] : []), "decline"]).map((action) =>
            <button key={action} className="vmax-secondary" disabled={busy} onClick={() => void run(async () => {
              await invoke("cloud_decide", { id: request.id, action });
              setIncoming((all) => all.filter((item) => item.id !== request.id));
            })}>{action === "always_allow" ? "Always Allow" : action === "accept" ? "Accept" : "Decline"}</button>)}
        </div>
        {error && <p>{error}</p>}
      </div>
    ))}
    {section && container && createPortal(<section className="vmax-account-panel space-y-3 rounded-xl border border-zinc-800 bg-zinc-950 p-4 text-zinc-100">
      <h2 className="text-lg font-semibold">{section === "accounts" ? "Accounts" : section === "computers" ? "Your Computers" : "Friends on your account"}</h2>
      {queuedLink && !data && <p>Sign in to open the invite.</p>}
      {error && <p role="alert" className="text-rose-300">{error}</p>}
      {loadError && <p role="alert" className="text-rose-300">{loadError}</p>}
      {notice && <p role="status">{notice}</p>}
      {project === undefined ? <p role="status">Checking account setup…</p> :
        (project === null || configure) ? <form className="grid gap-2" onSubmit={(event) => {
          event.preventDefault();
          void run(async () => {
            await invoke("cloud_configure", { url: projectUrl, publishableKey: projectKey });
            setProjectKey("");
            setConfigure(false);
            await checkProject();
          });
        }}>
          <p>Connect VmaX to the Supabase project used by your computers.</p>
          <input aria-label="Supabase project URL" type="url" placeholder="https://your-project.supabase.co" value={projectUrl} onChange={(event) => setProjectUrl(event.target.value)} required />
          <input aria-label="Supabase publishable key" placeholder="sb_publishable_…" value={projectKey} onChange={(event) => setProjectKey(event.target.value)} required />
          <div className="flex flex-wrap gap-2">
            <button className="vmax-primary" disabled={busy}>Save project</button>
            {project && <button className="vmax-secondary" type="button" disabled={busy} onClick={() => setConfigure(false)}>Cancel</button>}
          </div>
        </form> : !loaded ? <p role="status">Loading account…</p> : !data ? <form className="grid gap-2" onSubmit={(event) => {
        event.preventDefault();
        void run(async () => {
          await invoke("cloud_sign_in", { email, password, register: false });
          setPassword("");
          await refresh();
        });
      }}>
        <input aria-label="Email" placeholder="Email address" type="email" autoComplete="username" value={email} onChange={(e) => setEmail(e.target.value)} required />
        <input aria-label="Password" placeholder="Password" type="password" autoComplete="current-password" value={password} onChange={(e) => setPassword(e.target.value)} required />
        <div className="flex flex-wrap gap-2">
          <button className="vmax-primary" disabled={busy}>Sign in</button>
          <button className="vmax-secondary" type="button" disabled={busy || !email || !password} onClick={() => void run(async () => {
            const signedIn = await invoke<boolean>("cloud_sign_in", { email, password, register: true });
            setPassword("");
            if (signedIn) await refresh(); else setNotice("Check your email to confirm your account, then sign in.");
          })}>Create account</button>
          <button className="vmax-secondary" type="button" disabled={busy} onClick={() => {
            setProjectUrl(project);
            setConfigure(true);
          }}>Change project</button>
        </div>
      </form> : !profile || editProfile ? <form className="flex flex-wrap gap-2" onSubmit={(event) => {
        event.preventDefault();
        void run(async () => { await invoke("cloud_profile", { username, tag }); setEditProfile(false); await refresh(); });
      }}>
        <input aria-label="Username" placeholder="Username" value={username} onChange={(e) => setUsername(e.target.value)} minLength={3} maxLength={24} required />
        <input aria-label="Tag" placeholder="4-digit tag" value={tag} onChange={(e) => setTag(e.target.value)} pattern="[0-9]{4}" inputMode="numeric" required />
        <button className="vmax-primary" disabled={busy}>{profile ? "Save profile" : "Set up profile"}</button>
        {profile && <button className="vmax-secondary" type="button" disabled={busy} onClick={() => setEditProfile(false)}>Cancel</button>}
        {!profile && <button className="vmax-secondary" type="button" disabled={busy} onClick={() => void run(async () => {
          await invoke("cloud_sign_out"); setData(null); setIncoming([]);
        })}>Sign out</button>}
      </form> : <>
        <div className="flex items-center justify-between gap-2">
          <span>{profile.username}#{profile.tag}</span>
          <button className="vmax-secondary" disabled={busy} onClick={() => void run(async () => {
            await invoke("cloud_sign_out"); setData(null); setIncoming([]); await refresh();
          })}>Sign out</button>
        </div>
        {section === "accounts" ? <>
          <div className="flex flex-wrap gap-2">
            <button className="vmax-secondary" disabled={busy} onClick={() => {
              setUsername(profile.username);
              setTag(profile.tag);
              setEditProfile(true);
            }}>Edit profile</button>
            <button className="vmax-secondary" disabled={busy} onClick={() => void run(refresh)}>Refresh account</button>
            <button className="vmax-secondary" onClick={onOpenComputers}>Your computers</button>
            <button className="vmax-secondary" onClick={onOpenFriends}>Friends</button>
          </div>
        </> : section === "computers" ? <>
          {rows(data.user_id, "Instant Self-Connect")}
          {canHost && <div className="flex flex-wrap gap-2">
            <button className="vmax-primary" disabled={busy} onClick={() => void run(async () => {
              await invoke("cloud_host_available", { params: data.available ? null : hostParams() });
              await refresh();
            })}>{data.available ? "Disable background hosting" : "Enable background hosting"}</button>
            <button className="vmax-secondary" disabled={busy || !data.available} onClick={() => void run(async () => {
              const link = await invoke<string>("create_invite_token");
              await navigator.clipboard.writeText(link);
              setCopied(true);
            })}>{copied ? "Link copied" : "Copy Invite Link"}</button>
          </div>}
        </> : <>
          <form className="flex flex-wrap gap-2" onSubmit={(event) => {
            event.preventDefault();
            void run(async () => {
              await invoke("cloud_friend", { action: "add", value: handle }); setHandle(""); await refresh();
            });
          }}>
            <input aria-label="Friend handle" placeholder="Username#Tag" value={handle} onChange={(e) => setHandle(e.target.value)} required />
            <button className="vmax-primary" disabled={busy}>Add friend</button>
          </form>
          {data.friendships.filter((friend) => friend.status !== "blocked").map((friend) => {
            const requester = friend.requester_id === data.user_id;
            const peer = requester ? friend.addressee_id : friend.requester_id;
            const identity = data.profiles.find((p) => p.id === peer);
            function update(action: string, enabled?: boolean) {
              void run(async () => { await invoke("cloud_friend", { action, value: friend.id, enabled }); await refresh(); });
            }
            return <div className="space-y-2 rounded-lg bg-zinc-900 p-3" key={friend.id}>
              <div>{identity ? `${identity.username}#${identity.tag}` : "Friend"} · {friend.status}</div>
              {friend.status === "accepted" ? <>
                <label><input type="checkbox" checked={requester ? friend.requester_auto_approve : friend.auto_approve_join}
                  disabled={busy} onChange={(event) => update("auto_approve", event.target.checked)} /> Auto-Approve</label>
                {rows(peer, "Join")}
                <button className="vmax-secondary" disabled={busy} onClick={() => update("decline")}>Remove friend</button>
              </> : <div className="flex gap-2">
                {!requester && <button className="vmax-primary" disabled={busy} onClick={() => update("accept")}>Accept</button>}
                <button className="vmax-secondary" disabled={busy} onClick={() => update("decline")}>{requester ? "Cancel" : "Decline"}</button>
              </div>}
            </div>;
          })}
        </>}
      </>}
      {canHost && section === "computers" && <label className="block"><input type="checkbox" checked={autostart} disabled={busy}
        onChange={(event) => {
          const enabled = event.target.checked;
          void run(async () => { await invoke("set_autostart", { enabled }); setAutostart(enabled); });
        }} /> Start minimized to tray on boot</label>}
    </section>, container)}
  </>;
}
