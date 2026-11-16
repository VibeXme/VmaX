use crate::network::SessionRole;
use crate::signaling::validate_signaling_url;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{fs, io, io::Write, path::Path};

const MAX_SAVED: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedNetwork {
    pub name: String,
    pub endpoint: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedDevice {
    pub name: String,
    pub role: SessionRole,
    pub fingerprint: String,
    pub endpoint: String,
    pub automatic: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_key: Option<String>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SavedConnections {
    pub networks: Vec<SavedNetwork>,
    pub devices: Vec<SavedDevice>,
    pub auto_host_endpoint: Option<String>,
}

impl SavedConnections {
    pub fn view(mut self) -> Self {
        for device in &mut self.devices {
            device.resume_key = None;
        }
        self
    }

    pub fn remember(&mut self, device: SavedDevice) -> Result<(), String> {
        if let Some(existing) = self
            .devices
            .iter_mut()
            .find(|entry| entry.role == device.role && entry.fingerprint == device.fingerprint)
        {
            let automatic = existing.automatic && existing.endpoint == device.endpoint;
            *existing = SavedDevice {
                automatic,
                ..device
            };
        } else if self.devices.len() < MAX_SAVED {
            self.devices.push(device);
        } else {
            return Err("Remove a saved device before adding another.".into());
        }
        Ok(())
    }

    pub fn save_network(&mut self, name: String, endpoint: String) -> Result<(), String> {
        let endpoint = normalize_endpoint(&endpoint)?;
        let name = normalize_name(&name)?;
        if let Some(profile) = self
            .networks
            .iter_mut()
            .find(|entry| entry.endpoint == endpoint)
        {
            profile.name = name;
        } else if self.networks.len() < MAX_SAVED {
            self.networks.push(SavedNetwork { name, endpoint });
        } else {
            return Err("Remove a saved network before adding another.".into());
        }
        Ok(())
    }

    pub fn allowed_clients(&self, endpoint: &str) -> Vec<String> {
        self.devices
            .iter()
            .filter(|peer| {
                peer.role == SessionRole::Client && peer.automatic && peer.endpoint == endpoint
            })
            .map(|peer| peer.fingerprint.clone())
            .collect()
    }

    pub fn set_device_automatic(
        &mut self,
        role: SessionRole,
        fingerprint: &str,
        enabled: bool,
    ) -> Result<(), String> {
        let index = self
            .devices
            .iter()
            .position(|peer| peer.role == role && peer.fingerprint == fingerprint)
            .ok_or_else(|| "Saved device no longer exists.".to_string())?;
        if enabled && role == SessionRole::Host && self.devices[index].resume_key.is_none() {
            return Err("Reconnect with a fresh invite to save this host again.".into());
        }
        if enabled && role == SessionRole::Host {
            for peer in self
                .devices
                .iter_mut()
                .filter(|peer| peer.role == SessionRole::Host)
            {
                peer.automatic = false;
            }
            self.auto_host_endpoint = None;
        }
        self.devices[index].automatic = enabled;
        Ok(())
    }
}

pub fn normalize_endpoint(endpoint: &str) -> Result<String, String> {
    validate_signaling_url(endpoint.trim())
        .map(|url| url.to_string())
        .map_err(|err| err.to_string())
}

pub fn normalize_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 80 || name.chars().any(char::is_control) {
        return Err("Enter a name from 1 to 80 characters.".into());
    }
    Ok(name.to_string())
}

pub fn read_json<T: DeserializeOwned>(path: &Path) -> io::Result<Option<T>> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(io::Error::other),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub fn write_json(path: &Path, value: &impl Serialize) -> io::Result<()> {
    write_json_atomic(path, value)
}

pub fn create_json(path: &Path, value: &impl Serialize) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("missing data directory"))?;
    fs::create_dir_all(parent)?;
    let mut lock_path = path.as_os_str().to_owned();
    lock_path.push(".lock");
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;
    // Android app storage permits renaming but can deny hard links.
    // Keep the existence check and atomic rename under one lock.
    lock.lock()?;
    if path.try_exists()? {
        return Err(io::Error::from(io::ErrorKind::AlreadyExists));
    }
    write_json_atomic(path, value)
}

fn write_json_atomic(path: &Path, value: &impl Serialize) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("missing data directory"))?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".vmax-{:016x}.tmp", rand::random::<u64>()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    let result = (|| {
        serde_json::to_writer(&mut file, value).map_err(io::Error::other)?;
        file.flush()?;
        file.sync_all()
    })();
    drop(file);
    let result = result.and_then(|()| fs::rename(&temporary, path));
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn competing_identity_creators_keep_the_first_complete_value() {
        let directory =
            std::env::temp_dir().join(format!("vmax-identity-test-{:016x}", rand::random::<u64>()));
        let path = directory.join("identity.json");
        let (first, second) = std::thread::scope(|scope| {
            let first = scope.spawn(|| create_json(&path, &1_u8));
            let second = scope.spawn(|| create_json(&path, &2_u8));
            (first.join().unwrap(), second.join().unwrap())
        });
        let persisted = read_json::<u8>(&path).unwrap();
        let (winner, loser) = if first.is_ok() {
            (1, second)
        } else {
            (2, first)
        };
        assert_eq!(persisted, Some(winner));
        assert_eq!(loser.unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        fs::remove_file(path.with_extension("json.lock")).unwrap();
        fs::remove_file(path).unwrap();
        fs::remove_dir(directory).unwrap();
    }
}
