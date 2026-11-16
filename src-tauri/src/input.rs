use crate::protocol::{InputEvent, InputPacket, TouchAction};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(target_os = "windows")]
pub mod gamepad;
pub mod gamepad_client;
#[cfg(target_os = "linux")]
mod gamepad_linux;
#[cfg(target_os = "windows")]
pub mod keyboard_capture;
#[cfg(target_os = "windows")]
pub mod text_focus;
pub mod touch_gamepad;

/// Typed IPC input payload sent from Client frontend (`StreamViewer`) to Rust backend.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FrontendInputPayload {
    MouseMoveRel {
        dx: i32,
        dy: i32,
    },
    MouseMoveAbs {
        #[serde(rename = "xNorm")]
        x_norm: f32,
        #[serde(rename = "yNorm")]
        y_norm: f32,
    },
    TouchPointer {
        #[serde(rename = "xNorm")]
        x_norm: f32,
        #[serde(rename = "yNorm")]
        y_norm: f32,
        action: TouchAction,
        #[serde(rename = "tapId")]
        tap_id: u32,
    },
    MouseButton {
        button: u8,
        pressed: bool,
    },
    MouseWheel {
        #[serde(rename = "deltaX")]
        delta_x: i16,
        #[serde(rename = "deltaY")]
        delta_y: i16,
    },
    Key {
        #[serde(rename = "scanCode")]
        scan_code: u32,
        pressed: bool,
        modifiers: u8,
    },
}

impl From<FrontendInputPayload> for InputEvent {
    fn from(value: FrontendInputPayload) -> Self {
        match value {
            FrontendInputPayload::MouseMoveRel { dx, dy } => Self::MouseMoveRel { dx, dy },
            FrontendInputPayload::MouseMoveAbs { x_norm, y_norm } => {
                Self::MouseMoveAbs { x_norm, y_norm }
            }
            FrontendInputPayload::TouchPointer {
                x_norm,
                y_norm,
                action,
                tap_id,
            } => Self::TouchPointer {
                x_norm,
                y_norm,
                action,
                tap_id,
            },
            FrontendInputPayload::MouseButton { button, pressed } => {
                Self::MouseButton { button, pressed }
            }
            FrontendInputPayload::MouseWheel { delta_x, delta_y } => {
                Self::MouseWheel { delta_x, delta_y }
            }
            FrontendInputPayload::Key {
                scan_code,
                pressed,
                modifiers,
            } => Self::Key {
                scan_code,
                pressed,
                modifiers,
            },
        }
    }
}

pub fn now_unix_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}

pub fn build_input_packet(seq: u64, payload: FrontendInputPayload) -> InputPacket {
    InputPacket {
        seq,
        client_ts_us: now_unix_us(),
        event: payload.into(),
    }
}

#[cfg(target_os = "windows")]
#[derive(Default)]
pub struct HeldRemoteInputs {
    keys: std::collections::HashSet<u32>,
    buttons: std::collections::HashSet<u8>,
    last_position_seq: Option<u64>,
}

#[cfg(target_os = "windows")]
pub fn desktop_point(
    x_norm: f32,
    y_norm: f32,
    bounds: Option<windows::Win32::Foundation::RECT>,
) -> windows::Win32::Foundation::POINT {
    use windows::Win32::Foundation::POINT;
    let bounds = bounds.unwrap_or_else(win32::virtual_desktop);
    let scale = |value: f32, start: i32, end: i32| {
        start + (value.clamp(0.0, 1.0) * (end - start - 1).max(0) as f32).round() as i32
    };
    POINT {
        x: scale(x_norm, bounds.left, bounds.right),
        y: scale(y_norm, bounds.top, bounds.bottom),
    }
}

#[cfg(target_os = "windows")]
impl HeldRemoteInputs {
    pub fn release_keyboard(&mut self) {
        for scan_code in self.keys.drain() {
            win32::inject_win32_input(
                &InputEvent::Key {
                    scan_code,
                    pressed: false,
                    modifiers: 0,
                },
                None,
            );
        }
    }

    pub fn release_mouse(&mut self) {
        for button in self.buttons.drain() {
            win32::inject_win32_input(
                &InputEvent::MouseButton {
                    button,
                    pressed: false,
                },
                None,
            );
        }
    }

    pub fn inject(
        &mut self,
        packet: &InputPacket,
        bounds: Option<windows::Win32::Foundation::RECT>,
    ) -> bool {
        let event = &packet.event;
        if matches!(
            event,
            InputEvent::MouseMoveAbs { .. } | InputEvent::TouchPointer { .. }
        ) {
            if matches!(
                event,
                InputEvent::MouseMoveAbs { .. }
                    | InputEvent::TouchPointer {
                        action: TouchAction::Drag,
                        ..
                    }
            ) && self
                .last_position_seq
                .is_some_and(|last| packet.seq <= last)
            {
                return true;
            }
            self.last_position_seq = Some(
                self.last_position_seq
                    .map_or(packet.seq, |last| last.max(packet.seq)),
            );
        }
        match *event {
            InputEvent::Key {
                scan_code, pressed, ..
            } if scan_code != 0 => {
                if pressed {
                    self.keys.insert(scan_code);
                } else {
                    self.keys.remove(&scan_code);
                }
            }
            InputEvent::MouseButton { button, pressed } if button <= 4 => {
                if pressed {
                    self.buttons.insert(button);
                } else {
                    self.buttons.remove(&button);
                }
            }
            InputEvent::TouchPointer {
                action: TouchAction::Down,
                ..
            } => {
                self.buttons.insert(0);
            }
            InputEvent::TouchPointer {
                action: TouchAction::Up,
                ..
            } => {
                if !self.buttons.remove(&0) {
                    return true;
                }
            }
            InputEvent::TouchPointer {
                action: TouchAction::Drag,
                ..
            } if !self.buttons.contains(&0) => return true,
            _ => {}
        }
        win32::inject_win32_input(event, bounds)
    }
}

#[cfg(target_os = "windows")]
impl Drop for HeldRemoteInputs {
    fn drop(&mut self) {
        self.release_keyboard();
        self.release_mouse();
    }
}

#[cfg(target_os = "windows")]
mod win32 {
    use super::desktop_point;
    use crate::protocol::{InputEvent, TouchAction};
    use std::mem::size_of;
    use windows::Win32::Foundation::RECT;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT,
        KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE,
        MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
        MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
        MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL,
        MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, SendInput, VIRTUAL_KEY,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
    };

    const XBUTTON1: u32 = 0x0001;
    const XBUTTON2: u32 = 0x0002;
    const ABSOLUTE_MOVE: MOUSE_EVENT_FLAGS =
        MOUSE_EVENT_FLAGS(MOUSEEVENTF_MOVE.0 | MOUSEEVENTF_ABSOLUTE.0 | MOUSEEVENTF_VIRTUALDESK.0);

    pub(super) fn virtual_desktop() -> RECT {
        unsafe {
            let left = GetSystemMetrics(SM_XVIRTUALSCREEN);
            let top = GetSystemMetrics(SM_YVIRTUALSCREEN);
            RECT {
                left,
                top,
                right: left + GetSystemMetrics(SM_CXVIRTUALSCREEN),
                bottom: top + GetSystemMetrics(SM_CYVIRTUALSCREEN),
            }
        }
    }

    fn absolute_move(x_norm: f32, y_norm: f32, bounds: Option<RECT>) -> INPUT {
        let desktop = virtual_desktop();
        let point = desktop_point(x_norm, y_norm, bounds.or(Some(desktop)));
        let normalize = |offset: i32, size: i32| {
            ((f64::from(offset) + 0.5) * 65536.0 / f64::from(size.max(1))).clamp(0.0, 65535.0)
                as i32
        };
        mouse_input(
            normalize(point.x - desktop.left, desktop.right - desktop.left),
            normalize(point.y - desktop.top, desktop.bottom - desktop.top),
            0,
            ABSOLUTE_MOVE,
        )
    }

    fn mouse_input(dx: i32, dy: i32, mouse_data: u32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    mouseData: mouse_data,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    fn submit(inputs: &[INPUT]) -> bool {
        unsafe { SendInput(inputs, size_of::<INPUT>() as i32) as usize == inputs.len() }
    }

    fn send_mouse(dx: i32, dy: i32, mouse_data: u32, flags: MOUSE_EVENT_FLAGS) -> bool {
        submit(&[mouse_input(dx, dy, mouse_data, flags)])
    }

    fn key_input(scan_code: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0),
                    wScan: scan_code,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    pub(super) fn inject_win32_input(event: &InputEvent, bounds: Option<RECT>) -> bool {
        match *event {
            InputEvent::MouseMoveRel { dx, dy } => send_mouse(dx, dy, 0, MOUSEEVENTF_MOVE),
            InputEvent::MouseMoveAbs { x_norm, y_norm } => {
                submit(&[absolute_move(x_norm, y_norm, bounds)])
            }
            InputEvent::TouchPointer {
                x_norm,
                y_norm,
                action,
                ..
            } => {
                let (first, release) = match action {
                    TouchAction::Tap => (MOUSEEVENTF_LEFTDOWN, Some(MOUSEEVENTF_LEFTUP)),
                    TouchAction::Down => (MOUSEEVENTF_LEFTDOWN, None),
                    TouchAction::Up => (MOUSEEVENTF_LEFTUP, None),
                    TouchAction::Context => (MOUSEEVENTF_RIGHTDOWN, Some(MOUSEEVENTF_RIGHTUP)),
                    TouchAction::Drag => (MOUSE_EVENT_FLAGS::default(), None),
                };
                let input = [
                    absolute_move(x_norm, y_norm, bounds),
                    mouse_input(0, 0, 0, first),
                    mouse_input(0, 0, 0, release.unwrap_or_default()),
                ];
                let len = if release.is_some() {
                    3
                } else if action == TouchAction::Drag {
                    1
                } else {
                    2
                };
                submit(&input[..len])
            }
            InputEvent::Unicode { codepoint } => {
                let Some(character) = char::from_u32(codepoint) else {
                    return true;
                };
                let mut utf16 = [0; 2];
                let mut keys = [INPUT::default(); 4];
                let units = character.encode_utf16(&mut utf16);
                for (index, &mut unit) in units.iter_mut().enumerate() {
                    keys[index * 2] = key_input(unit, KEYEVENTF_UNICODE);
                    keys[index * 2 + 1] = key_input(unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP);
                }
                submit(&keys[..units.len() * 2])
            }
            InputEvent::MouseButton { button, pressed } => {
                let (flags, data) = match (button, pressed) {
                    (0, true) => (MOUSEEVENTF_LEFTDOWN, 0),
                    (0, false) => (MOUSEEVENTF_LEFTUP, 0),
                    (1, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
                    (1, false) => (MOUSEEVENTF_MIDDLEUP, 0),
                    (2, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
                    (2, false) => (MOUSEEVENTF_RIGHTUP, 0),
                    (3, true) => (MOUSEEVENTF_XDOWN, XBUTTON1),
                    (3, false) => (MOUSEEVENTF_XUP, XBUTTON1),
                    (4, true) => (MOUSEEVENTF_XDOWN, XBUTTON2),
                    (4, false) => (MOUSEEVENTF_XUP, XBUTTON2),
                    _ => return true,
                };
                send_mouse(0, 0, data, flags)
            }
            InputEvent::MouseWheel { delta_x, delta_y } => {
                let vertical =
                    delta_y == 0 || send_mouse(0, 0, (-delta_y as i32) as u32, MOUSEEVENTF_WHEEL);
                let horizontal =
                    delta_x == 0 || send_mouse(0, 0, (delta_x as i32) as u32, MOUSEEVENTF_HWHEEL);
                vertical && horizontal
            }
            InputEvent::Key {
                scan_code,
                pressed,
                modifiers: _,
            } => {
                if scan_code == 0 {
                    return true;
                }
                // Bit 8 (0xE0xx) signals an extended AT/PS2 scan code (e.g., arrows, RAlt, RCtrl, Insert/Delete).
                let is_extended = (scan_code & 0xFF00) == 0xE000;
                let hw_scan = (scan_code & 0xFF) as u16;
                let mut flags: KEYBD_EVENT_FLAGS = KEYEVENTF_SCANCODE;
                if is_extended {
                    flags |= KEYEVENTF_EXTENDEDKEY;
                }
                if !pressed {
                    flags |= KEYEVENTF_KEYUP;
                }
                submit(&[key_input(hw_scan, flags)])
            }
            InputEvent::GamepadState { .. } => true,
        }
    }
}
