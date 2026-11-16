use crate::{
    haptics::{LEASE, Rumble},
    protocol::InputEvent,
};
use gilrs::ff::{BaseEffect, BaseEffectType, Effect, EffectBuilder, Repeat, Replay, Ticks};
use gilrs::{Axis, Button, GamepadId, Gilrs};
use std::time::Instant;

pub(super) struct LinuxGamepad {
    input: Gilrs,
    selected: Option<GamepadId>,
    effects: Option<[Effect; 2]>,
    last_rumble: (Rumble, Option<Instant>),
}

impl LinuxGamepad {
    pub(super) fn new() -> Option<Self> {
        Some(Self {
            input: Gilrs::new().ok()?,
            selected: None,
            effects: None,
            last_rumble: (Rumble::default(), None),
        })
    }

    pub(super) fn read(&mut self) -> Option<InputEvent> {
        while self.input.next_event().is_some() {}
        let selected = self
            .selected
            .filter(|&id| self.input.connected_gamepad(id).is_some())
            .or_else(|| self.input.gamepads().next().map(|(id, _)| id));
        if self.selected != selected {
            self.stop();
            self.effects = None;
            self.selected = selected;
        }
        let pad = self.input.connected_gamepad(selected?)?;
        let buttons = [
            (Button::DPadUp, 0x0001),
            (Button::DPadDown, 0x0002),
            (Button::DPadLeft, 0x0004),
            (Button::DPadRight, 0x0008),
            (Button::Start, 0x0010),
            (Button::Select, 0x0020),
            (Button::LeftThumb, 0x0040),
            (Button::RightThumb, 0x0080),
            (Button::LeftTrigger, 0x0100),
            (Button::RightTrigger, 0x0200),
            (Button::Mode, 0x0400),
            (Button::South, 0x1000),
            (Button::East, 0x2000),
            (Button::West, 0x4000),
            (Button::North, 0x8000),
        ]
        .into_iter()
        .filter(|(button, _)| pad.is_pressed(*button))
        .fold(0, |mask, (_, bit)| mask | bit);
        let axis = |axis| {
            let value = pad.value(axis).clamp(-1.0, 1.0);
            (value * if value < 0.0 { 32768.0 } else { 32767.0 }).round() as i16
        };
        let trigger = |button| {
            pad.button_data(button).map_or(0, |data| {
                (data.value().clamp(0.0, 1.0) * 255.0).round() as u8
            })
        };
        Some(InputEvent::GamepadState {
            index: 0,
            buttons,
            left_trigger: trigger(Button::LeftTrigger2),
            right_trigger: trigger(Button::RightTrigger2),
            left_x: axis(Axis::LeftStickX),
            left_y: axis(Axis::LeftStickY),
            right_x: axis(Axis::RightStickX),
            right_y: axis(Axis::RightStickY),
        })
    }

    pub(super) fn rumble(&mut self, rumble: Rumble, received: Option<Instant>) {
        let Some(received) = received.filter(|time| time.elapsed() < LEASE) else {
            self.stop();
            return;
        };
        if rumble == Rumble::default() {
            self.stop();
            return;
        }
        if self.last_rumble == (rumble, Some(received)) {
            return;
        }
        let Some(id) = self.selected.filter(|&id| {
            self.input
                .connected_gamepad(id)
                .is_some_and(|pad| pad.is_ff_supported())
        }) else {
            return;
        };
        if self.effects.is_none() {
            let duration = Ticks::from_ms(LEASE.as_millis() as u32);
            let mut build = |kind| {
                EffectBuilder::new()
                    .add_effect(BaseEffect {
                        kind,
                        scheduling: Replay {
                            play_for: duration,
                            ..Default::default()
                        },
                        ..Default::default()
                    })
                    .repeat(Repeat::For(duration))
                    .gamepads(&[id])
                    .finish(&mut self.input)
            };
            self.effects = build(BaseEffectType::Strong {
                magnitude: u16::MAX,
            })
            .ok()
            .zip(
                build(BaseEffectType::Weak {
                    magnitude: u16::MAX,
                })
                .ok(),
            )
            .map(|(strong, weak)| [strong, weak]);
        }
        if let Some(effects) = &self.effects {
            for (effect, strength) in effects.iter().zip([rumble.large, rumble.small]) {
                if strength == 0 {
                    let _ = effect.stop();
                } else {
                    let _ = effect.set_gain(f32::from(strength) / 255.0);
                    let _ = effect.play();
                }
            }
        }
        self.last_rumble = (rumble, Some(received));
    }

    fn stop(&mut self) {
        if self.last_rumble.1.take().is_some()
            && let Some(effects) = &self.effects
        {
            for effect in effects {
                let _ = effect.stop();
            }
        }
    }
}

impl Drop for LinuxGamepad {
    fn drop(&mut self) {
        self.stop();
    }
}
