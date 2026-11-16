use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::Mutex;

#[derive(Clone, Serialize, Deserialize)]
struct Session {
    #[serde(skip)]
    project: String,
    access_token: String,
    refresh_token: String,
    expires_at: u64,
    user: User,
}
#[derive(Serialize, Deserialize)]
struct SavedAccount {
    project: String,
    session: Session,
    host: Option<crate::commands::StartSessionParams>,
}

#[cfg(windows)]
fn protect(bytes: &[u8], encrypt: bool) -> Result<Vec<u8>, String> {
    use windows::Win32::{
        Foundation::{HLOCAL, LocalFree},
        Security::Cryptography::{
            CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
        },
    };
    let input = CRYPT_INTEGER_BLOB {
        cbData: bytes
            .len()
            .try_into()
            .map_err(|_| "Account state is too large")?,
        pbData: bytes.as_ptr().cast_mut(),
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    unsafe {
        if encrypt {
            CryptProtectData(
                &input,
                windows_core::PCWSTR::null(),
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        } else {
            CryptUnprotectData(
                &input,
                None,
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        }
        .map_err(|e| e.to_string())?;
        let result = if output.cbData == 0 {
            Vec::new()
        } else {
            std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec()
        };
        let _ = LocalFree(Some(HLOCAL(output.pbData.cast())));
        Ok(result)
    }
}

#[cfg(windows)]
async fn save(app: &AppHandle, cloud: &Cloud, session: &Session) -> Result<(), String> {
    let host = app.state::<PresenceState>().host.lock().await.clone();
    let encoded = serde_json::to_vec(&SavedAccount {
        project: cloud.url.to_string(),
        session: session.clone(),
        host,
    })
    .map_err(|e| e.to_string())?;
    let protected = protect(&encoded, true)?;
    crate::connections::write_json(
        &crate::commands::app_data_file(app, "cloud-account.json")?,
        &hex::encode(protected),
    )
    .map_err(|e| e.to_string())
}
#[cfg(not(windows))]
async fn save(_app: &AppHandle, _cloud: &Cloud, _session: &Session) -> Result<(), String> {
    Ok(())
}

#[cfg(windows)]
async fn restore(app: &AppHandle) {
    let Ok(cloud) = Cloud::configured(app) else {
        return;
    };
    let Ok(path) = crate::commands::app_data_file(app, "cloud-account.json") else {
        return;
    };
    let Ok(Some(encoded)) = crate::connections::read_json::<String>(&path) else {
        return;
    };
    let Ok(bytes) = hex::decode(encoded) else {
        return;
    };
    let Ok(raw) = protect(&bytes, false) else {
        return;
    };
    let Ok(mut saved) = serde_json::from_slice::<SavedAccount>(&raw) else {
        return;
    };
    if saved.project == cloud.url.as_str() {
        let state = app.state::<PresenceState>();
        let mut session = state.session.lock().await;
        if session.is_none() {
            *state.host.lock().await = saved.host;
            saved.session.project = saved.project;
            *session = Some(saved.session);
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct User {
    id: String,
}

#[derive(Clone)]
pub struct Cloud {
    http: reqwest::Client,
    url: url::Url,
    key: String,
}
impl Cloud {
    fn configured(app: &AppHandle) -> Result<Self, String> {
        let local: Option<CloudSettings> =
            crate::connections::read_json(&crate::commands::app_data_file(app, "supabase.json")?)
                .map_err(|e| e.to_string())?;
        let (url, key) = match local {
            Some(settings) => (settings.url, settings.publishable_key),
            None => (
                option_env!("VMAX_SUPABASE_URL")
                    .unwrap_or_default()
                    .to_owned(),
                option_env!("VMAX_SUPABASE_PUBLISHABLE_KEY")
                    .unwrap_or_default()
                    .to_owned(),
            ),
        };
        Self::new(&url, key)
    }

    fn new(project_url: &str, key: String) -> Result<Self, String> {
        let url = url::Url::parse(project_url)
            .map_err(|_| "Configure a Supabase project for account features")?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.path() != "/"
            || !key.starts_with("sb_publishable_")
        {
            return Err("Supabase needs an HTTPS project URL and a publishable key".into());
        }
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|e| e.to_string())?,
            url,
            key,
        })
    }

    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
        prefer: Option<&str>,
    ) -> Result<Value, String> {
        let url = self
            .url
            .join(path)
            .map_err(|_| "Invalid cloud request URL")?;
        let mut req = self.http.request(method, url).header("apikey", &self.key);
        if let Some(token) = token {
            req = req.bearer_auth(token);
        }
        if let Some(body) = body {
            req = req.json(&body);
        }
        if let Some(prefer) = prefer {
            req = req.header("Prefer", prefer);
        }
        let response = req.send().await.map_err(|e| {
            format!(
                "Supabase request failed ({})",
                if e.is_timeout() { "timeout" } else { "network" }
            )
        })?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("Supabase returned HTTP {}", status.as_u16()));
        }
        if status == reqwest::StatusCode::NO_CONTENT || response.content_length() == Some(0) {
            return Ok(Value::Null);
        }
        response
            .json()
            .await
            .map_err(|_| "Invalid Supabase response".into())
    }
    async fn get(&self, path: &str, session: &Session) -> Result<Value, String> {
        self.call(
            reqwest::Method::GET,
            path,
            Some(&session.access_token),
            None,
            None,
        )
        .await
    }
    async fn post(
        &self,
        path: &str,
        session: &Session,
        data: Value,
        prefer: &str,
    ) -> Result<Value, String> {
        self.call(
            reqwest::Method::POST,
            path,
            Some(&session.access_token),
            Some(data),
            Some(prefer),
        )
        .await
    }
    async fn patch(&self, path: &str, session: &Session, data: Value) -> Result<Value, String> {
        self.call(
            reqwest::Method::PATCH,
            path,
            Some(&session.access_token),
            Some(data),
            Some("return=representation"),
        )
        .await
    }
}

#[derive(Deserialize, Serialize)]
struct CloudSettings {
    url: String,
    publishable_key: String,
}

#[derive(Default)]
pub struct PresenceState {
    session: Mutex<Option<Session>>,
    host: Mutex<Option<crate::commands::StartSessionParams>>,
    operation: Mutex<()>,
    deep_link: std::sync::Mutex<Option<String>>,
    changed: tokio::sync::Notify,
}

pub(crate) fn device(app: &AppHandle) -> Result<crate::identity::DeviceIdentity, String> {
    crate::identity::DeviceIdentity::load_or_create(&crate::commands::app_data_file(
        app,
        crate::identity::IDENTITY_FILE,
    )?)
}

pub(crate) fn queue_link(app: &AppHandle, link: &str) {
    if link.len() > 2048 || !link.starts_with("vmax://join/") {
        return;
    }
    let state = app.state::<PresenceState>();
    if let Ok(mut pending) = state.deep_link.lock() {
        *pending = Some(link.to_owned());
    }
    let _ = app.emit("vmax://deep-link-ready", ());
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.set_focus();
    }
}

#[tauri::command]
pub fn take_deep_link(state: tauri::State<'_, PresenceState>) -> Option<String> {
    state.deep_link.lock().ok()?.take()
}

#[tauri::command]
pub fn identity_summary(app: AppHandle) -> Result<crate::identity::IdentitySummary, String> {
    Ok(device(&app)?.summary(cfg!(windows)))
}

async fn session(app: &AppHandle, cloud: &Cloud) -> Result<Session, String> {
    let state = app.state::<PresenceState>();
    let mut current = state.session.lock().await;
    let active = current.as_ref().ok_or("Sign in to VmaX first")?;
    if active.project != cloud.url.as_str() {
        return Err("The local account is signed in to a different project".into());
    }
    if active.expires_at > crate::identity::now_ms() / 1000 + 45 {
        return Ok(active.clone());
    }
    let refreshed = cloud
        .call(
            reqwest::Method::POST,
            "/auth/v1/token?grant_type=refresh_token",
            None,
            Some(json!({"refresh_token":active.refresh_token})),
            None,
        )
        .await?;
    let mut refreshed: Session =
        serde_json::from_value(refreshed).map_err(|_| "Invalid authentication response")?;
    refreshed.project = cloud.url.to_string();
    *current = Some(refreshed.clone());
    save(app, cloud, &refreshed).await?;
    Ok(refreshed)
}

#[tauri::command]
pub fn cloud_project_url(app: AppHandle) -> Option<String> {
    Cloud::configured(&app).ok().map(|cloud| cloud.url.to_string())
}

#[tauri::command]
pub async fn cloud_configure(app: AppHandle, url: String, publishable_key: String) -> Result<(), String> {
    let runtime = app.state::<PresenceState>();
    let _operation = runtime.operation.lock().await;
    if runtime.session.lock().await.is_some() {
        return Err("Sign out before changing the account project".into());
    }
    let cloud = Cloud::new(url.trim(), publishable_key.trim().to_owned())?;
    cloud.call(reqwest::Method::GET, "/auth/v1/settings", None, None, None).await?;
    crate::connections::write_json(
        &crate::commands::app_data_file(&app, "supabase.json")?,
        &CloudSettings { url: cloud.url.to_string(), publishable_key: cloud.key },
    ).map_err(|e| e.to_string())?;
    runtime.changed.notify_one();
    Ok(())
}

#[tauri::command]
pub async fn cloud_sign_in(
    app: AppHandle,
    email: String,
    password: String,
    register: bool,
) -> Result<bool, String> {
    let runtime = app.state::<PresenceState>();
    let _operation = runtime.operation.lock().await;
    let cloud = Cloud::configured(&app)?;
    let response = cloud
        .call(
            reqwest::Method::POST,
            if register {
                "/auth/v1/signup"
            } else {
                "/auth/v1/token?grant_type=password"
            },
            None,
            Some(json!({"email":email,"password":password})),
            None,
        )
        .await?;
    if register
        && response
            .get("access_token")
            .and_then(Value::as_str)
            .is_none()
    {
        return Ok(false);
    }
    let mut active: Session =
        serde_json::from_value(response).map_err(|_| "Invalid authentication response")?;
    if runtime
        .session
        .lock()
        .await
        .as_ref()
        .is_some_and(|current| current.user.id != active.user.id)
    {
        return Err("Sign out before switching accounts".into());
    }
    active.project = cloud.url.to_string();
    save(&app, &cloud, &active).await?;
    *runtime.session.lock().await = Some(active);
    runtime.changed.notify_one();
    Ok(true)
}

#[tauri::command]
pub async fn cloud_sign_out(app: AppHandle) -> Result<(), String> {
    let runtime = app.state::<PresenceState>();
    let _operation = runtime.operation.lock().await;
    *runtime.host.lock().await = None;
    #[cfg(windows)]
    {
        let path = crate::commands::app_data_file(&app, "cloud-account.json")?;
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    if let Some(active) = runtime.session.lock().await.take()
        && let Ok(cloud) = Cloud::configured(&app)
    {
        if let Ok(identity) = device(&app) {
            let _ = cloud
                .patch(
                    &format!("/rest/v1/devices?device_id=eq.{}", identity.device_id),
                    &active,
                    json!({"can_host":false, "last_seen":"1970-01-01T00:00:00Z"}),
                )
                .await;
        }
        let _ = cloud
            .call(
                reqwest::Method::POST,
                "/auth/v1/logout?scope=local",
                Some(&active.access_token),
                None,
                None,
            )
            .await;
    }
    Ok(())
}

async fn heartbeat(app: &AppHandle, cloud: &Cloud, active: &Session) -> Result<(), String> {
    let device = device(app)?;
    let can_host = app.state::<PresenceState>().host.lock().await.is_some();
    let fingerprint = crate::network::identity_fingerprint(crate::commands::app_data_file(
        app,
        "device-identity.json",
    )?)
    .map_err(|e| e.to_string())?;
    let telemetry =
        crate::commands::get_network_snapshot(app.state::<crate::commands::AppState>()).await?;
    let status = if telemetry.phase == crate::network::ConnectionPhase::Connected {
        "in_game"
    } else if can_host {
        "hosting"
    } else {
        "online"
    };
    cloud.post("/rest/v1/devices?on_conflict=device_id", active, json!({
        "owner_id":active.user.id, "device_id":device.device_id, "device_name":device.device_name,
        "platform":device.platform, "public_key":device.public_key, "tls_fingerprint":fingerprint,
        "can_host":can_host, "status":status, "last_seen":"now",
    }), "resolution=merge-duplicates,return=minimal").await?;
    Ok(())
}

#[tauri::command]
pub async fn cloud_profile(app: AppHandle, username: String, tag: String) -> Result<(), String> {
    let cloud = Cloud::configured(&app)?;
    let active = session(&app, &cloud).await?;
    cloud
        .post(
            "/rest/v1/profiles?on_conflict=id",
            &active,
            json!({"id":active.user.id,
        "username":username.trim(),"tag":tag.trim()}),
            "resolution=merge-duplicates,return=minimal",
        )
        .await?;
    heartbeat(&app, &cloud, &active).await
}

#[tauri::command]
pub async fn cloud_snapshot(app: AppHandle) -> Result<Value, String> {
    let cloud = Cloud::configured(&app)?;
    let active = session(&app, &cloud).await?;
    let (profiles, devices, friends) = tokio::try_join!(
        cloud.get("/rest/v1/profiles?select=id,username,tag", &active),
        cloud.get("/rest/v1/devices?select=device_id,owner_id,device_name,platform,can_host,status,last_seen", &active),
        cloud.get("/rest/v1/friendships?select=id,requester_id,addressee_id,status,auto_approve_join,requester_auto_approve", &active),
    )?;
    Ok(
        json!({"user_id":active.user.id,"device_id":device(&app)?.device_id,
        "available":app.state::<PresenceState>().host.lock().await.is_some(),
        "profiles":profiles,"devices":devices,"friendships":friends}),
    )
}

fn uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(i, c)| {
            if [8, 13, 18, 23].contains(&i) {
                c == b'-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
}

#[tauri::command]
pub async fn cloud_friend(
    app: AppHandle,
    action: String,
    value: String,
    enabled: Option<bool>,
) -> Result<(), String> {
    let cloud = Cloud::configured(&app)?;
    let active = session(&app, &cloud).await?;
    if action == "add" {
        let (name, tag) = value.trim().split_once('#').ok_or("Use Username#Tag")?;
        let result = cloud
            .post(
                "/rest/v1/rpc/find_vmax_profile",
                &active,
                json!({"p_username":name,"p_tag":tag}),
                "",
            )
            .await?;
        let friend = result
            .get(0)
            .and_then(|r| r["id"].as_str())
            .ok_or("No matching account")?;
        if friend == active.user.id {
            return Err("This is your own account".into());
        }
        cloud
            .post(
                "/rest/v1/friendships",
                &active,
                json!({"requester_id":active.user.id,"addressee_id":friend}),
                "return=minimal",
            )
            .await?;
        return Ok(());
    }
    if !uuid(&value) {
        return Err("Invalid friendship".into());
    }
    let path = format!("/rest/v1/friendships?id=eq.{value}");
    let data = match action.as_str() {
        "accept" => json!({"status":"accepted"}),
        "decline" => {
            cloud
                .call(
                    reqwest::Method::DELETE,
                    &path,
                    Some(&active.access_token),
                    None,
                    Some("return=minimal"),
                )
                .await?;
            return Ok(());
        }
        "block" => json!({"status":"blocked"}),
        "auto_approve" => {
            let row = cloud.get(&path, &active).await?;
            let flag = if row[0]["requester_id"].as_str() == Some(&active.user.id) {
                "requester_auto_approve"
            } else {
                "auto_approve_join"
            };
            json!({(flag):enabled.unwrap_or(false)})
        }
        _ => return Err("Unknown friend action".into()),
    };
    let result = cloud.patch(&path, &active, data).await?;
    if result.get(0).is_none() {
        return Err("Friendship update was denied".into());
    }
    Ok(())
}

#[tauri::command]
pub async fn cloud_host_available(
    app: AppHandle,
    params: Option<crate::commands::StartSessionParams>,
) -> Result<(), String> {
    let cloud = Cloud::configured(&app)?;
    let session = session(&app, &cloud).await?;
    if let Some(params) = &params
        && (!cfg!(windows)
            || crate::signaling::is_lan_signaling_url(&params.signaling_url)
            || !params.signaling_url.starts_with("wss://"))
    {
        return Err("Background hosting needs a Windows PC and a public wss:// VmaX relay".into());
    }
    let state = app.state::<PresenceState>();
    let _operation = state.operation.lock().await;
    if let Some(params) = &params {
        crate::connections::write_json(
            &crate::commands::app_data_file(&app, "cloud-host-settings.json")?,
            params,
        )
        .map_err(|e| e.to_string())?;
    }
    *state.host.lock().await = params;
    save(&app, &cloud, &session).await?;
    heartbeat(&app, &cloud, &session).await
}

#[tauri::command]
pub async fn create_invite_token(app: AppHandle) -> Result<String, String> {
    let cloud = Cloud::configured(&app)?;
    let session = session(&app, &cloud).await?;
    if app.state::<PresenceState>().host.lock().await.is_none() {
        return Err("Enable background hosting first".into());
    }
    let device = device(&app)?;
    heartbeat(&app, &cloud, &session).await?;
    let token = hex::encode(rand::random::<[u8; 32]>());
    cloud
        .post(
            "/rest/v1/session_invites",
            &session,
            json!({
                "token":token,"host_user_id":session.user.id,"host_device_id":device.device_id,
            }),
            "return=minimal",
        )
        .await?;
    Ok(crate::invite::link(&device.device_id, &token))
}

#[derive(Clone, Deserialize, Serialize)]
pub struct ConnectRequest {
    id: String,
    requester_id: String,
    source_device: String,
    host_user_id: String,
    target_device: String,
    source_public_key: String,
    source_tls_fingerprint: String,
    source_label: String,
    signature: String,
    expires_at_ms: u64,
}
fn proof(
    request_id: &str,
    owner_id: &str,
    source: &str,
    target: &str,
    fingerprint: &str,
    expiry: u64,
) -> String {
    format!(
        "vmax-connect-v1\n{request_id}\n{owner_id}\n{source}\n{target}\n{fingerprint}\n{expiry}"
    )
}

#[tauri::command]
pub async fn cloud_connect(
    app: AppHandle,
    target: String,
    invite: Option<String>,
) -> Result<String, String> {
    let cloud = Cloud::configured(&app)?;
    let session = session(&app, &cloud).await?;
    let snapshot =
        crate::commands::get_network_snapshot(app.state::<crate::commands::AppState>()).await?;
    if !matches!(
        snapshot.phase,
        crate::network::ConnectionPhase::Idle
            | crate::network::ConnectionPhase::Disconnected
            | crate::network::ConnectionPhase::Failed
    ) {
        return Err("Finish the current session before connecting".into());
    }
    let parsed = invite
        .as_deref()
        .map(|link| crate::invite::parse(link).ok_or("Invalid VmaX invite"))
        .transpose()?;
    let (target, token) = parsed.map_or((target.as_str(), None), |(device, token)| {
        (device, Some(token))
    });
    if !uuid(target) {
        return Err("Invalid target device".into());
    }
    let device = device(&app)?;
    heartbeat(&app, &cloud, &session).await?;
    let fingerprint = crate::network::identity_fingerprint(crate::commands::app_data_file(
        &app,
        "device-identity.json",
    )?)
    .map_err(|e| e.to_string())?;
    let request = crate::identity::new_uuid_v4();
    let expiry = crate::identity::now_ms() + 80_000;
    let sig = device.sign(
        proof(
            &request,
            &session.user.id,
            &device.device_id,
            target,
            &fingerprint,
            expiry,
        )
        .as_bytes(),
    )?;
    cloud
        .post(
            "/rest/v1/rpc/request_vmax_connect",
            &session,
            json!({"p_id":request,"p_source":device.device_id,
        "p_target":target,"p_expiry":expiry,"p_signature":sig,"p_token":token}),
            "",
        )
        .await?;
    let path = format!("/rest/v1/connect_requests?id=eq.{request}&select=status,route");
    tokio::time::timeout(Duration::from_secs(75), async {
        loop {
            let result = cloud.get(&path, &session).await?;
            match result[0]["status"].as_str() {
                Some("declined") => return Err("The host declined or is unavailable".into()),
                Some("approved") => {
                    let route = result[0]["route"]
                        .as_str()
                        .ok_or("Host returned no route")?;
                    let parsed = crate::signaling::parse_invite_input(route, "")
                        .map_err(|_| "Invalid host route")?;
                    if !parsed.signaling_url.starts_with("wss://")
                        || parsed.expected_host_fingerprint.is_none()
                    {
                        return Err(
                            "The host did not provide an authenticated internet route".into()
                        );
                    }
                    return Ok(route.to_owned());
                }
                _ => tokio::time::sleep(Duration::from_millis(500)).await,
            }
        }
    })
    .await
    .map_err(|_| "The host did not respond in time".to_string())?
}

async fn incoming(
    app: &AppHandle,
    cloud: &Cloud,
    active: &Session,
) -> Result<Vec<ConnectRequest>, String> {
    let device = device(app)?;
    let rows = cloud.get(&format!("/rest/v1/connect_requests?target_device=eq.{}&status=eq.pending&expires_at_ms=gt.{}&limit=32",
        device.device_id,crate::identity::now_ms()),active).await?;
    let rows: Vec<ConnectRequest> =
        serde_json::from_value(rows).map_err(|_| "Invalid connection request")?;
    Ok(rows
        .into_iter()
        .filter(|r| {
            r.host_user_id == active.user.id
                && crate::identity::verify(
                    &r.source_public_key,
                    proof(
                        &r.id,
                        &r.requester_id,
                        &r.source_device,
                        &r.target_device,
                        &r.source_tls_fingerprint,
                        r.expires_at_ms,
                    )
                    .as_bytes(),
                    &r.signature,
                )
        })
        .collect())
}

async fn friendship(cloud: &Cloud, active: &Session, peer: &str) -> Result<Option<Value>, String> {
    if !uuid(peer) {
        return Ok(None);
    }
    let rows = cloud.get(&format!("/rest/v1/friendships?or=(and(requester_id.eq.{peer},addressee_id.eq.{}),and(addressee_id.eq.{peer},requester_id.eq.{}))",
        active.user.id,active.user.id),active).await?;
    Ok(rows.get(0).cloned())
}

async fn decide(
    app: &AppHandle,
    cloud: &Cloud,
    active: &Session,
    request: &ConnectRequest,
    action: &str,
) -> Result<(), String> {
    let path = format!(
        "/rest/v1/connect_requests?id=eq.{}&status=eq.pending",
        request.id
    );
    let peer = friendship(cloud, active, &request.requester_id).await?;
    let blocked = peer.as_ref().is_some_and(|row| row["status"] == "blocked");
    if blocked || action == "decline" {
        cloud
            .patch(&path, active, json!({"status":"declined","route":null}))
            .await?;
        return Ok(());
    }
    if action == "always_allow" {
        let friend = peer
            .as_ref()
            .filter(|row| row["status"] == "accepted")
            .ok_or("Accept the friendship before enabling unattended access")?;
        let id = friend["id"].as_str().ok_or("Invalid friendship")?;
        cloud_friend(app.clone(), "auto_approve".into(), id.into(), Some(true)).await?;
    }
    let params = app
        .state::<PresenceState>()
        .host
        .lock()
        .await
        .clone()
        .ok_or("Background hosting is disabled")?;
    let state =
        crate::commands::get_network_snapshot(app.state::<crate::commands::AppState>()).await?;
    if !matches!(
        state.phase,
        crate::network::ConnectionPhase::Idle
            | crate::network::ConnectionPhase::Disconnected
            | crate::network::ConnectionPhase::Failed
    ) {
        return Err("Finish the active session before accepting another connection".into());
    }
    let route = crate::commands::host_approved_device(
        app.clone(),
        params,
        request.source_tls_fingerprint.clone(),
    )
    .await?;
    let expiration = request.expires_at_ms;
    let deadline_app = app.clone();
    let waiting_route = route.clone();
    tauri::async_runtime::spawn(async move {
        let remaining = expiration.saturating_sub(crate::identity::now_ms());
        tokio::time::sleep(Duration::from_millis(remaining)).await;
        crate::commands::expire_waiting_invite(&deadline_app, &waiting_route).await;
    });
    let changed = cloud
        .patch(&path, active, json!({"status":"approved","route":route}))
        .await;
    if changed.as_ref().ok().and_then(|v| v.get(0)).is_none() {
        let _ = crate::commands::stop_session(app.clone(), app.state()).await;
        return Err("The approval expired or could not be published".into());
    }
    Ok(())
}

#[tauri::command]
pub async fn cloud_decide(app: AppHandle, id: String, action: String) -> Result<(), String> {
    if !matches!(action.as_str(), "accept" | "always_allow" | "decline") {
        return Err("Invalid response".into());
    }
    let cloud = Cloud::configured(&app)?;
    let state = app.state::<PresenceState>();
    let _operation = state.operation.lock().await;
    let actor = session(&app, &cloud).await?;
    let row = incoming(&app, &cloud, &actor)
        .await?
        .into_iter()
        .find(|row| row.id == id)
        .ok_or("Request expired")?;
    decide(&app, &cloud, &actor, &row, &action).await
}

pub async fn tray_toggle(app: AppHandle) -> Result<(), String> {
    let current = app.state::<PresenceState>();
    if current.host.lock().await.is_some() {
        return cloud_host_available(app.clone(), None).await;
    }
    let Some(params) = crate::connections::read_json::<crate::commands::StartSessionParams>(
        &crate::commands::app_data_file(&app, "cloud-host-settings.json")?,
    )
    .map_err(|e| e.to_string())?
    else {
        return Err("Set up background hosting in Computers first".into());
    };
    cloud_host_available(app.clone(), Some(params)).await
}

pub fn start(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        #[cfg(windows)]
        restore(&app).await;
        let mut shown = std::collections::HashSet::new();
        let mut next_heartbeat = tokio::time::Instant::now();
        let mut realtime = None;
        let mut retry_realtime = tokio::time::Instant::now();
        loop {
            let runtime = app.state::<PresenceState>();
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(3)) => {},
                _ = runtime.changed.notified() => {},
            }
            let Ok(_operation) = runtime.operation.try_lock() else {
                continue;
            };
            let Ok(cloud) = Cloud::configured(&app) else {
                continue;
            };
            let Ok(actor) = session(&app, &cloud).await else {
                if let Some((_, task)) = realtime.take() {
                    tokio::task::JoinHandle::<()>::abort(&task);
                }
                shown.clear();
                next_heartbeat = tokio::time::Instant::now();
                continue;
            };
            if tokio::time::Instant::now() >= next_heartbeat {
                let _ = heartbeat(&app, &cloud, &actor).await;
                next_heartbeat = tokio::time::Instant::now() + Duration::from_secs(20);
            }
            if realtime.as_ref().is_some_and(
                |(token, task): &(String, tokio::task::JoinHandle<()>)| {
                    token != &actor.access_token || task.is_finished()
                },
            ) {
                if let Some((_, task)) = realtime.take() {
                    task.abort();
                }
            }
            if realtime.is_none()
                && tokio::time::Instant::now() >= retry_realtime
                && let Ok(identity) = device(&app)
            {
                realtime = Some((
                    actor.access_token.clone(),
                    tokio::spawn(realtime_updates(
                        app.clone(),
                        cloud.clone(),
                        actor.clone(),
                        identity.device_id,
                    )),
                ));
                retry_realtime = tokio::time::Instant::now() + Duration::from_secs(5);
            }
            if runtime.host.lock().await.is_none() {
                continue;
            }
            let Ok(rows) = incoming(&app, &cloud, &actor).await else {
                continue;
            };
            shown.retain(|id| rows.iter().any(|r| &r.id == id));
            for request in rows {
                let Ok(friend) = friendship(&cloud, &actor, &request.requester_id).await else {
                    continue;
                };
                let accepted = friend.as_ref().filter(|r| r["status"] == "accepted");
                let allowed = accepted.is_some_and(|r| {
                    let flag = if r["requester_id"].as_str() == Some(&actor.user.id) {
                        "requester_auto_approve"
                    } else {
                        "auto_approve_join"
                    };
                    r[flag].as_bool().unwrap_or(false)
                });
                if friend.as_ref().is_some_and(|r| r["status"] == "blocked") {
                    let _ = decide(&app, &cloud, &actor, &request, "decline").await;
                } else if crate::identity::decide_join(
                    Some(&actor.user.id),
                    Some(&request.requester_id),
                    allowed,
                )
                .starts_streaming()
                {
                    if decide(&app, &cloud, &actor, &request, "accept")
                        .await
                        .is_ok()
                    {
                        break;
                    }
                } else if shown.insert(request.id.clone()) {
                    let _ = app.emit("vmax://join-request",json!({"id":request.id,"name":request.source_label,"friend":accepted.is_some()}));
                    #[cfg(desktop)]
                    {
                        use tauri_plugin_notification::NotificationExt;
                        let _ = app
                            .notification()
                            .builder()
                            .title("VmaX join request")
                            .body(format!(
                                "{} wants to connect. Open VmaX to respond.",
                                request.source_label
                            ))
                            .show();
                    }
                }
            }
        }
    });
}

async fn realtime_updates(app: AppHandle, cloud: Cloud, actor: Session, device_id: String) {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    let Ok(mut url) = cloud.url.join("/realtime/v1/websocket") else {
        return;
    };
    let _ = url.set_scheme("wss");
    url.query_pairs_mut()
        .append_pair("apikey", &cloud.key)
        .append_pair("vsn", "1.0.0");
    let Ok(Ok((mut ws, _))) = tokio::time::timeout(
        Duration::from_secs(10),
        tokio_tungstenite::connect_async(url.as_str()),
    )
    .await
    else {
        return;
    };
    let topic = format!("realtime:signal:device:{device_id}");
    let join = json!({"topic":topic,"event":"phx_join","ref":"1","payload":{
        "access_token":actor.access_token,
        "config":{"private":true,"postgres_changes":[
            {"event":"*","schema":"public","table":"devices"},
            {"event":"*","schema":"public","table":"friendships"},
            {"event":"*","schema":"public","table":"connect_requests"}
        ]}
    }});
    if ws
        .send(Message::Text(join.to_string().into()))
        .await
        .is_err()
    {
        return;
    }
    let mut clock = tokio::time::interval(Duration::from_secs(25));
    let mut id = 2u64;
    loop {
        tokio::select! {
            inbound = ws.next() => match inbound {
                Some(Ok(Message::Text(text))) => {
                    if let Ok(value) = serde_json::from_str::<Value>(&text) {
                        if value["event"] == "postgres_changes" {
                            let _ = app.emit("vmax://cloud-updated", ());
                            if value["payload"]["data"]["table"] == "connect_requests" {
                                app.state::<PresenceState>().changed.notify_one();
                            }
                        }
                        if value["event"] == "phx_error" || (value["event"] == "phx_reply" && value["payload"]["status"] == "error") { break; }
                    }
                }
                Some(Ok(Message::Ping(data))) => { if ws.send(Message::Pong(data)).await.is_err() { break; } }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                _ => {}
            },
            _ = clock.tick() => {
                let heartbeat = json!({"topic":"phoenix","event":"heartbeat","payload":{},"ref":id.to_string()});
                if ws.send(Message::Text(heartbeat.to_string().into())).await.is_err() { break; }
                id = id.wrapping_add(1);
            }
        }
    }
}
