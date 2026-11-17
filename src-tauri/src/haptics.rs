use bytes::Bytes;
use serde::Serialize;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

pub const REFRESH: Duration = Duration::from_millis(60);
pub const LEASE: Duration = Duration::from_millis(180);

/// Channel 0x06 follows the existing control and congestion feedback channels.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Rumble {
    pub large: u8,
    pub small: u8,
    pub pad: u8,
}

impl Rumble {
    pub fn encode(self) -> Bytes {
        Bytes::copy_from_slice(&[
            crate::protocol::ChannelId::Haptics as u8,
            self.large,
            self.small,
            self.pad,
        ])
    }

    pub fn decode(data: &[u8]) -> Option<Self> {
        match data {
            [channel, large, small, 0] if *channel == crate::protocol::ChannelId::Haptics as u8 => {
                Some(Self {
                    large: *large,
                    small: *small,
                    pad: 0,
                })
            }
            _ => None,
        }
    }

    pub fn scaled(self, intensity: u8) -> Self {
        let scale = |motor| ((u16::from(motor) * u16::from(intensity.min(100)) + 50) / 100) as u8;
        Self {
            large: scale(self.large),
            small: scale(self.small),
            ..self
        }
    }
}

#[derive(Clone, Copy, Serialize)]
pub struct HapticsSettings {
    pub enabled: bool,
    pub intensity: u8,
}

// One atomic snapshot keeps enabled and intensity consistent.
pub struct HapticsConfig(AtomicU8);

impl Default for HapticsConfig {
    fn default() -> Self {
        Self(AtomicU8::new(0x80 | 100))
    }
}

impl HapticsConfig {
    pub fn get(&self) -> HapticsSettings {
        let bits = self.0.load(Ordering::Acquire);
        HapticsSettings {
            enabled: bits & 0x80 != 0,
            intensity: bits & 0x7f,
        }
    }

    pub fn set(&self, enabled: bool, intensity: u8) {
        self.0.store(
            (if enabled { 0x80 } else { 0 }) | intensity.min(100),
            Ordering::Release,
        );
    }
}

#[cfg(target_os = "android")]
mod android {
    use super::{LEASE, Rumble};
    use jni::objects::{GlobalRef, JObject, JValue};
    use jni::{JNIEnv, JavaVM};
    use std::sync::{Arc, Mutex};

    struct Activity {
        vm: JavaVM,
        instance: GlobalRef,
    }
    static ACTIVITY: Mutex<Option<Arc<Activity>>> = Mutex::new(None);
    static GAMEPAD: Mutex<Option<crate::protocol::InputEvent>> = Mutex::new(None);

    pub fn gamepad() -> Option<crate::protocol::InputEvent> {
        GAMEPAD.lock().ok().and_then(|state| *state)
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_com_vmax_app_MainActivity_nativeControllerState(
        _env: JNIEnv<'_>,
        _activity: JObject<'_>,
        buttons: i32,
        lt: i32,
        rt: i32,
        lx: i32,
        ly: i32,
        rx: i32,
        ry: i32,
    ) {
        if let Ok(mut state) = GAMEPAD.lock() {
            *state = (buttons >= 0).then_some(crate::protocol::InputEvent::GamepadState {
                index: 0,
                buttons: buttons as u16,
                left_trigger: lt as u8,
                right_trigger: rt as u8,
                left_x: lx as i16,
                left_y: ly as i16,
                right_x: rx as i16,
                right_y: ry as i16,
            });
        }
    }

    static HEVC: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    static MAIN10: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    static REFRESH_HZ: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);

    pub fn hevc() -> bool {
        HEVC.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn main10() -> bool {
        MAIN10.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn refresh_hz() -> u16 {
        REFRESH_HZ.load(std::sync::atomic::Ordering::Relaxed)
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_com_vmax_app_MainActivity_nativeRegisterDeviceCapabilities(
        env: JNIEnv<'_>,
        activity: JObject<'_>,
        enabled: jni::sys::jboolean,
        refresh_hz: jni::sys::jint,
        hevc: jni::sys::jboolean,
        main10: jni::sys::jboolean,
    ) {
        let Ok(mut current) = ACTIVITY.lock() else {
            return;
        };
        if enabled == 0 {
            if current.as_ref().is_some_and(|active| {
                env.is_same_object(&active.instance, &activity)
                    .unwrap_or(false)
            }) {
                *current = None;
            }
        } else if let Ok(vm) = env.get_java_vm()
            && let Ok(instance) = env.new_global_ref(activity)
        {
            *current = Some(Arc::new(Activity { vm, instance }));
            HEVC.store(hevc != 0, std::sync::atomic::Ordering::Relaxed);
            MAIN10.store(main10 != 0, std::sync::atomic::Ordering::Relaxed);
            REFRESH_HZ.store(
                refresh_hz.clamp(0, 240) as u16,
                std::sync::atomic::Ordering::Relaxed,
            );
        }
    }

    pub fn deliver(rumble: Rumble, touch: bool) {
        let Some(activity) = ACTIVITY.lock().ok().and_then(|current| current.clone()) else {
            return;
        };
        let Ok(mut env) = activity.vm.attach_current_thread() else {
            return;
        };
        if env
            .call_method(
                &activity.instance,
                "playControllerRumble",
                "(IIIZ)V",
                &[
                    JValue::Int(i32::from(rumble.large)),
                    JValue::Int(i32::from(rumble.small)),
                    JValue::Int(LEASE.as_millis() as i32),
                    JValue::Bool(u8::from(touch)),
                ],
            )
            .is_err()
        {
            let _ = env.exception_clear();
        }
    }
}

#[cfg(target_os = "android")]
pub use android::{
    deliver as deliver_android, gamepad as android_gamepad, hevc as android_hevc,
    main10 as android_main10, refresh_hz as android_refresh_hz,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rumble_has_its_own_channel_and_rejects_unsupported_pads() {
        let rumble = Rumble {
            large: 255,
            small: 128,
            pad: 0,
        };
        assert_eq!(Rumble::decode(&rumble.encode()), Some(rumble));
        assert_eq!(
            rumble.scaled(50),
            Rumble {
                large: 128,
                small: 64,
                pad: 0
            }
        );
        assert_eq!(Rumble::decode(&[4, 255, 128, 0]), None);
        assert_eq!(Rumble::decode(&[6, 255, 128, 1]), None);
        assert_eq!(rumble.scaled(0), Rumble::default());
    }
}
