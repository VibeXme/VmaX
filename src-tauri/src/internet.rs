//! App-owned temporary internet endpoint. Media still uses VmaX's encrypted
//! QUIC protocol; Cloudflare supplies reachability when a public relay is absent.
#[cfg(target_os = "windows")]
mod desktop {
    use crate::signaling::LocalSignalingServer;
    use std::{path::Path, process::Stdio, time::Duration};
    use tokio::{
        io::{AsyncBufReadExt, BufReader},
        process::{Child, Command},
    };

    pub struct InternetTunnel {
        child: Child,
        relay: LocalSignalingServer,
        reader: tokio::task::JoinHandle<()>,
        pub url: String,
    }

    impl InternetTunnel {
        pub fn alive(&mut self) -> bool {
            matches!(self.child.try_wait(), Ok(None))
        }

        pub fn origin_url(&self) -> &str {
            &self.relay.ws_url
        }

        pub async fn reachable(&self) -> bool {
            matches!(
                tokio::time::timeout(
                    Duration::from_secs(4),
                    crate::signaling::list_relay_rooms(&self.url)
                )
                .await,
                Ok(Ok(_))
            )
        }

        pub async fn start(directory: &Path) -> Result<Self, String> {
            tokio::fs::create_dir_all(directory)
                .await
                .map_err(|e| e.to_string())?;
            tokio::fs::write(
                directory.join("cloudflared-LICENSE"),
                include_bytes!("../resources/cloudflared-LICENSE"),
            )
            .await
            .map_err(|e| e.to_string())?;
            let helper = directory.join("cloudflared-2026.10.7.exe");
            let embedded = include_bytes!("../resources/cloudflared.exe");
            let existing = tokio::fs::read(&helper).await.unwrap_or_default();
            if existing.as_slice() != embedded {
                tokio::fs::write(&helper, embedded)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            match Self::start_attempt(&helper, "auto").await {
                Ok(tunnel) => Ok(tunnel),
                Err(first_error) => {
                    tracing::warn!(%first_error, "Retrying public tunnel over HTTP/2");
                    Self::start_attempt(&helper, "http2").await
                }
            }
        }

        async fn start_attempt(helper: &Path, protocol: &str) -> Result<Self, String> {
            let relay = LocalSignalingServer::start("127.0.0.1:0")
                .await
                .map_err(|e| e.to_string())?;
            let mut command = Command::new(helper);
            command
                .args([
                    "tunnel",
                    "--protocol",
                    protocol,
                    "--no-autoupdate",
                    "--url",
                    &format!("http://{}", relay.local_addr),
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .creation_flags(0x08000000);
            let mut child = command
                .spawn()
                .map_err(|e| format!("Internet helper could not start: {e}"))?;
            let stderr = child
                .stderr
                .take()
                .ok_or("Internet helper has no status output")?;
            let (tx, rx) = tokio::sync::oneshot::channel();
            let reader = tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                let mut tx = Some(tx);
                let mut url = None;
                let mut registered = false;
                while let Ok(Some(line)) = lines.next_line().await {
                    registered |= line.contains("Registered tunnel connection");
                    if url.is_none() {
                        url = super::endpoint_from_log(&line);
                    }
                    if registered
                        && let Some(url) = &url
                        && let Some(tx) = tx.take()
                    {
                        let _ = tx.send(url.clone());
                    }
                }
            });
            let result = tokio::time::timeout(Duration::from_secs(45), rx).await;
            let url = match result {
                Ok(Ok(url)) => url,
                _ => {
                    reader.abort();
                    return Err("Internet setup failed. Check internet access or configure a public VmaX relay.".into());
                }
            };
            // Keep draining helper output so its pipe cannot fill and stall it.
            let tunnel = Self {
                child,
                relay,
                reader,
                url,
            };
            // Give DNS time to publish the new name before the first lookup.
            // A premature lookup can cache a negative result on Windows.
            tokio::time::sleep(Duration::from_secs(10)).await;
            // A printed URL is not readiness: DNS and the edge route may still
            // be propagating. Verify TLS and WebSocket reachability first.
            let mut last_error = String::new();
            tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    match crate::signaling::list_relay_rooms(&tunnel.url).await {
                        Ok(_) => return,
                        Err(error) => {
                            last_error = error.to_string();
                            tokio::time::sleep(Duration::from_secs(2)).await;
                        }
                    }
                }
            }).await.map_err(|_| format!("Internet address is not reachable yet. Retry setup or use a public relay. {last_error}"))?;
            Ok(tunnel)
        }
    }

    impl Drop for InternetTunnel {
        fn drop(&mut self) {
            let _ = self.child.start_kill();
            self.reader.abort();
        }
    }
}

#[cfg(target_os = "windows")]
pub use desktop::InternetTunnel;

/// Prefer a consistently fast configured relay before creating a temporary tunnel.
#[cfg(target_os = "windows")]
pub async fn preferred_public_relay(
    preferred: Option<&str>,
    additional: &[String],
) -> Option<String> {
    let configured = std::env::var("VMAX_PUBLIC_RELAY_URLS")
        .ok()
        .or_else(|| option_env!("VMAX_PUBLIC_RELAY_URLS").map(str::to_owned))
        .unwrap_or_default();
    let mut checks = tokio::task::JoinSet::new();
    let mut seen = std::collections::HashSet::new();
    for endpoint in preferred
        .into_iter()
        .chain(additional.iter().map(String::as_str))
        .chain(configured.split(','))
        .map(str::trim)
    {
        if let Ok(url) = crate::signaling::validate_signaling_url(endpoint)
            && url.scheme() == "wss"
            && seen.insert(url.to_string())
        {
            let endpoint = url.to_string();
            checks.spawn(async move {
                let mut timings = Vec::with_capacity(3);
                for _ in 0..3 {
                    let started = std::time::Instant::now();
                    let result = tokio::time::timeout(
                        std::time::Duration::from_millis(900),
                        crate::signaling::list_relay_rooms(&endpoint),
                    )
                    .await;
                    if matches!(result, Ok(Ok(_))) {
                        timings.push(started.elapsed());
                    }
                }
                timings.sort_unstable();
                (timings.len() >= 2).then(|| (timings[timings.len() / 2], endpoint))
            });
            if seen.len() == 8 {
                break;
            }
        }
    }
    let mut best = None;
    while let Some(result) = checks.join_next().await {
        if let Ok(Some(candidate)) = result
            && best.as_ref().is_none_or(|current| candidate < *current)
        {
            best = Some(candidate);
        }
    }
    best.map(|(_, endpoint)| endpoint)
}

fn endpoint_from_log(line: &str) -> Option<String> {
    line.split_whitespace().find_map(|word| {
        let parsed = url::Url::parse(word).ok()?;
        let host = parsed.host_str()?;
        (parsed.scheme() == "https"
            && host.ends_with(".trycloudflare.com")
            && parsed.username().is_empty()
            && parsed.password().is_none()
            && parsed.port().is_none()
            && parsed.path() == "/"
            && parsed.query().is_none()
            && parsed.fragment().is_none())
        .then(|| format!("wss://{host}"))
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_accepts_tunnel_origin() {
        assert_eq!(
            super::endpoint_from_log("INF | https://test.trycloudflare.com |"),
            Some("wss://test.trycloudflare.com".into())
        );
        for invalid in [
            "https://trycloudflare.com",
            "https://a.trycloudflare.com.evil.test",
            "http://a.trycloudflare.com",
            "https://a.trycloudflare.com/path",
        ] {
            assert_eq!(super::endpoint_from_log(invalid), None);
        }
    }
}
