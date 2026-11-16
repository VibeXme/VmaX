use crate::protocol::InputEvent;
use serde::Deserialize;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::Notify;

const TOUCH_REPORT_TIMEOUT: Duration = Duration::from_millis(750);

#[derive(Deserialize)]
pub struct TouchGamepadInput {
    buttons: Vec<TouchButton>,
    left: [f32; 2],
    right: [f32; 2],
}

#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum TouchButton {
    A,
    B,
    X,
    Y,
    Lb,
    Rb,
    Lt,
    Rt,
    Up,
    Down,
    Left,
    Right,
    View,
    Menu,
    L3,
    R3,
}

#[derive(Default)]
pub struct TouchGamepad {
    report: Mutex<Option<(Instant, InputEvent)>>,
    changed: Notify,
}

impl TouchGamepad {
    pub fn set(&self, input: Option<TouchGamepadInput>) {
        if let Ok(mut report) = self.report.lock() {
            *report = input.map(|input| (Instant::now(), input.into()));
            self.changed.notify_one();
        }
    }

    pub async fn changed(&self) {
        self.changed.notified().await;
    }

    pub fn reading(&self) -> Option<InputEvent> {
        self.report
            .lock()
            .ok()
            .and_then(|report| *report)
            .filter(|(time, _)| time.elapsed() < TOUCH_REPORT_TIMEOUT)
            .map(|(_, event)| event)
    }
}

impl From<TouchGamepadInput> for InputEvent {
    fn from(input: TouchGamepadInput) -> Self {
        let buttons = input.buttons.iter().fold(0, |flags, button| {
            flags
                | match button {
                    TouchButton::Up => 0x0001,
                    TouchButton::Down => 0x0002,
                    TouchButton::Left => 0x0004,
                    TouchButton::Right => 0x0008,
                    TouchButton::Menu => 0x0010,
                    TouchButton::View => 0x0020,
                    TouchButton::L3 => 0x0040,
                    TouchButton::R3 => 0x0080,
                    TouchButton::Lb => 0x0100,
                    TouchButton::Rb => 0x0200,
                    TouchButton::A => 0x1000,
                    TouchButton::B => 0x2000,
                    TouchButton::X => 0x4000,
                    TouchButton::Y => 0x8000,
                    TouchButton::Lt | TouchButton::Rt => 0,
                }
        });

        Self::GamepadState {
            index: 0,
            buttons,
            left_trigger: if input.buttons.contains(&TouchButton::Lt) {
                255
            } else {
                0
            },
            right_trigger: if input.buttons.contains(&TouchButton::Rt) {
                255
            } else {
                0
            },
            left_x: stick_axis(input.left[0]),
            left_y: stick_axis(-input.left[1]),
            right_x: stick_axis(input.right[0]),
            right_y: stick_axis(-input.right[1]),
        }
    }
}

fn stick_axis(axis: f32) -> i16 {
    let axis = axis.clamp(-1.0, 1.0);
    (axis * if axis < 0.0 { 32768.0 } else { 32767.0 }).round() as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touch_report_maps_combined_controls_and_expires() {
        let pad = TouchGamepad::default();
        let input = serde_json::from_value(serde_json::json!({
            "buttons": ["a", "lb", "rt", "left", "l3", "r3"],
            "left": [-1, 1],
            "right": [0.5, -1],
        }))
        .unwrap();
        pad.set(Some(input));
        assert_eq!(
            pad.reading(),
            Some(InputEvent::GamepadState {
                index: 0,
                buttons: 0x11c4,
                left_trigger: 0,
                right_trigger: 255,
                left_x: i16::MIN,
                left_y: i16::MIN,
                right_x: 16384,
                right_y: i16::MAX,
            }),
        );
        if let Some((time, _)) = pad.report.lock().unwrap().as_mut() {
            *time -= TOUCH_REPORT_TIMEOUT;
        }
        assert_eq!(pad.reading(), None);
        pad.set(None);
        assert_eq!(pad.reading(), None);
    }
}
