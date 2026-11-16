use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceType {
    Desktop,
    Mobile,
    Handheld,
}

/// Optional limits in the reliable ClientCapabilities message. Zero means
/// unknown. max_decode_mbps is measured in megabits per second.
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientDisplay {
    pub device_type: DeviceType,
    pub screen_width: u16,
    pub screen_height: u16,
    pub max_refresh_hz: u16,
    pub max_decode_mbps: u16,
    pub supports_haptics: bool,
    #[serde(default)]
    pub hevc_main10: bool,
    #[serde(default)]
    pub hevc_444: bool,
}

impl ClientDisplay {
    /// Applies only the limits that cannot be worked around: the client's
    /// display refresh rate caps presentation. The stream size is *not*
    /// clamped to the client's screen, because the resolution the host selects
    /// is the resolution the client receives and displays (scaled by the
    /// client's own renderer when the panel is smaller).
    pub fn constrain(&self, width: u32, height: u32, fps: u16) -> (u32, u32, u16) {
        let _ = (self.screen_width, self.screen_height, self.device_type);
        let fps = if self.max_refresh_hz >= 30 {
            fps.min(self.max_refresh_hz)
        } else {
            fps
        };
        (width, height, fps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_resolution_is_kept_and_only_refresh_caps_the_cadence() {
        let phone = ClientDisplay {
            device_type: DeviceType::Mobile,
            screen_width: 1080,
            screen_height: 2400,
            max_refresh_hz: 90,
            max_decode_mbps: 0,
            supports_haptics: true,
            hevc_main10: false,
            hevc_444: false,
        };
        // A 4K host selection stays 4K: the client scales it to its own panel.
        assert_eq!(phone.constrain(3840, 2160, 120), (3840, 2160, 90));
        let mut legacy = serde_json::to_value(phone).unwrap();
        assert_eq!(legacy.get("hevc444"), Some(&serde_json::Value::Bool(false)));
        legacy.as_object_mut().unwrap().remove("hevc444");
        let legacy: ClientDisplay = serde_json::from_value(legacy).unwrap();
        assert!(!legacy.hevc_444);
        assert_eq!(phone.constrain(1280, 720, 60), (1280, 720, 60));
    }
}
