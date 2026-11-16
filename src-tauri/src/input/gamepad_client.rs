#[cfg(any(target_os = "windows", target_os = "linux"))]
use crate::haptics::Rumble;
use crate::input::now_unix_us;
use crate::input::touch_gamepad::TouchGamepad;
use crate::protocol::{InputEvent, InputPacket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior, interval};
#[cfg(target_os = "windows")]
use windows::Win32::Foundation::ERROR_SUCCESS;
#[cfg(target_os = "windows")]
use windows::Win32::UI::Input::XboxController::{
    XINPUT_GAMEPAD, XINPUT_STATE, XINPUT_VIBRATION, XInputGetState, XInputSetState, XUSER_MAX_COUNT,
};
#[cfg(target_os = "windows")]
use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

const POLL_INTERVAL: Duration = Duration::from_millis(8);
#[cfg(target_os = "windows")]
const DISCOVERY_INTERVAL: Duration = Duration::from_millis(250);
const REPORT_INTERVAL: Duration = Duration::from_millis(100);
const NEUTRAL: InputEvent = InputEvent::GamepadState {
    index: 0,
    buttons: 0,
    left_trigger: 0,
    right_trigger: 0,
    left_x: 0,
    left_y: 0,
    right_x: 0,
    right_y: 0,
};

pub struct ClientGamepad {
    task: JoinHandle<()>,
    #[cfg(target_os = "linux")]
    rumble_tx: tokio::sync::watch::Sender<(Rumble, Option<std::time::Instant>)>,
    #[cfg(target_os = "windows")]
    selected: Arc<std::sync::atomic::AtomicU32>,
    #[cfg(target_os = "windows")]
    rumbling_slot: Option<u32>,
}

impl ClientGamepad {
    pub fn start(
        window_handle: Option<isize>,
        paused: Arc<AtomicBool>,
        touch: Arc<TouchGamepad>,
        seq: Arc<AtomicU64>,
        input_tx: mpsc::Sender<InputPacket>,
    ) -> Self {
        #[cfg(target_os = "windows")]
        let selected = Arc::new(std::sync::atomic::AtomicU32::new(u32::MAX));
        #[cfg(target_os = "windows")]
        let selected_for_poll = selected.clone();
        #[cfg(target_os = "linux")]
        let (rumble_tx, rumble_rx) = tokio::sync::watch::channel((Rumble::default(), None));
        #[cfg(target_os = "linux")]
        let gamepad = LinuxWorker::start(rumble_rx);
        let task = tokio::spawn(async move {
            let mut poll = interval(POLL_INTERVAL);
            poll.set_missed_tick_behavior(MissedTickBehavior::Skip);
            #[cfg(target_os = "windows")]
            let mut native = NativeGamepad {
                slot: None,
                next_discovery: Instant::now(),
            };
            let mut last_report = None;

            loop {
                tokio::select! {
                    _ = input_tx.closed() => break,
                    _ = poll.tick() => {}
                    _ = touch.changed() => {}
                }
                let now = Instant::now();
                let reading = if paused.load(Ordering::Acquire) || !is_focused(window_handle) {
                    None
                } else {
                    let reading = touch.reading();
                    #[cfg(target_os = "windows")]
                    let reading = reading.or_else(|| native.read(now));
                    #[cfg(target_os = "linux")]
                    let reading = reading.or_else(|| *gamepad.reading.borrow());
                    #[cfg(target_os = "android")]
                    let reading = reading.or_else(crate::haptics::android_gamepad);
                    reading
                };

                #[cfg(target_os = "windows")]
                selected_for_poll.store(
                    if reading.is_some() {
                        native.slot.unwrap_or(u32::MAX)
                    } else {
                        u32::MAX
                    },
                    Ordering::Release,
                );

                if let Some(report) = report_due(reading, last_report, now) {
                    if send_report(&input_tx, &seq, report) {
                        last_report = Some((report, now));
                    }
                }
            }
        });
        Self {
            task,
            #[cfg(target_os = "linux")]
            rumble_tx,
            #[cfg(target_os = "windows")]
            selected,
            #[cfg(target_os = "windows")]
            rumbling_slot: None,
        }
    }

    #[cfg(target_os = "linux")]
    pub fn rumble(&mut self, packet: Rumble) {
        self.rumble_tx
            .send_replace((packet, Some(std::time::Instant::now())));
    }

    #[cfg(target_os = "windows")]
    pub fn rumble(&mut self, packet: Rumble) {
        let selected = self.selected.load(Ordering::Acquire);
        let next = (selected < XUSER_MAX_COUNT && packet != Rumble::default()).then_some(selected);
        if let Some(previous) = self.rumbling_slot
            && Some(previous) != next
        {
            let _ = unsafe { XInputSetState(previous, &XINPUT_VIBRATION::default()) };
        }
        if let Some(index) = next {
            let _ = unsafe {
                XInputSetState(
                    index,
                    &XINPUT_VIBRATION {
                        wLeftMotorSpeed: u16::from(packet.large) * 257,
                        wRightMotorSpeed: u16::from(packet.small) * 257,
                    },
                )
            };
        }
        self.rumbling_slot = next;
    }
}

#[cfg(target_os = "linux")]
struct LinuxWorker {
    reading: tokio::sync::watch::Receiver<Option<InputEvent>>,
}

#[cfg(target_os = "linux")]
impl LinuxWorker {
    fn start(rumble: tokio::sync::watch::Receiver<(Rumble, Option<std::time::Instant>)>) -> Self {
        let (tx, reading) = tokio::sync::watch::channel(None);
        let _ = std::thread::Builder::new()
            .name("vmax-linux-gamepad".into())
            .spawn(move || {
                let Some(mut gamepad) = super::gamepad_linux::LinuxGamepad::new() else {
                    return;
                };
                while !tx.is_closed() {
                    tx.send_if_modified(|current| {
                        let report = gamepad.read();
                        if *current == report {
                            return false;
                        }
                        *current = report;
                        true
                    });
                    let (packet, timestamp) = *rumble.borrow();
                    gamepad.rumble(packet, timestamp);
                    std::thread::sleep(POLL_INTERVAL);
                }
            });
        Self { reading }
    }
}

fn report_due(
    reading: Option<InputEvent>,
    previous: Option<(InputEvent, Instant)>,
    now: Instant,
) -> Option<InputEvent> {
    // After focus loss/disconnect, repeat release snapshots in case the first
    // datagram is lost. Never create a controller for a client that has none.
    let report = reading.or_else(|| previous.map(|_| NEUTRAL))?;
    let cadence = if reading.is_some() && report != NEUTRAL { POLL_INTERVAL } else { REPORT_INTERVAL };
    if previous.is_some_and(|(old, sent)| old == report && now - sent < cadence) {
        None
    } else {
        Some(report)
    }
}

impl Drop for ClientGamepad {
    fn drop(&mut self) {
        self.task.abort();
        #[cfg(any(target_os = "windows", target_os = "linux"))]
        self.rumble(Rumble::default());
    }
}

#[cfg(target_os = "windows")]
fn is_focused(window_handle: Option<isize>) -> bool {
    window_handle.is_some_and(|handle| unsafe { GetForegroundWindow() }.0 as isize == handle)
}

#[cfg(not(target_os = "windows"))]
fn is_focused(_window_handle: Option<isize>) -> bool {
    // Touch reports stop when the WebView is hidden and expire if it is suspended.
    true
}

#[cfg(target_os = "windows")]
struct NativeGamepad {
    slot: Option<u32>,
    next_discovery: Instant,
}

#[cfg(target_os = "windows")]
impl NativeGamepad {
    fn read(&mut self, now: Instant) -> Option<InputEvent> {
        if let Some(index) = self.slot {
            let reading = read_gamepad(index);
            if reading.is_none() {
                self.slot = None;
            }
            return reading.map(report_event);
        }
        if now < self.next_discovery {
            return None;
        }

        self.next_discovery = now + DISCOVERY_INTERVAL;
        (0..XUSER_MAX_COUNT).find_map(|index| {
            let reading = read_gamepad(index)?;
            self.slot = Some(index);
            Some(report_event(reading))
        })
    }
}

#[cfg(target_os = "windows")]
fn read_gamepad(index: u32) -> Option<XINPUT_GAMEPAD> {
    let mut state = XINPUT_STATE::default();
    // XInput writes to this initialized state and accepts slots below XUSER_MAX_COUNT.
    let result = unsafe { XInputGetState(index, &mut state) };
    (result == ERROR_SUCCESS.0).then_some(state.Gamepad)
}

fn send_report(input_tx: &mpsc::Sender<InputPacket>, seq: &AtomicU64, event: InputEvent) -> bool {
    input_tx
        .try_send(InputPacket {
            seq: seq.fetch_add(1, Ordering::Relaxed),
            client_ts_us: now_unix_us(),
            event,
        })
        .is_ok()
}

#[cfg(target_os = "windows")]
fn report_event(gamepad: XINPUT_GAMEPAD) -> InputEvent {
    InputEvent::GamepadState {
        index: 0,
        buttons: gamepad.wButtons.0,
        left_trigger: gamepad.bLeftTrigger,
        right_trigger: gamepad.bRightTrigger,
        left_x: gamepad.sThumbLX,
        left_y: gamepad.sThumbLY,
        right_x: gamepad.sThumbRX,
        right_y: gamepad.sThumbRY,
    }
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::*;
    use windows::Win32::UI::Input::XboxController::{XINPUT_GAMEPAD_A, XINPUT_GAMEPAD_DPAD_RIGHT};

    #[test]
    fn lost_release_is_repeated_without_poll_rate_flooding() {
        let now = Instant::now();
        let pressed = report_event(XINPUT_GAMEPAD {
            wButtons: XINPUT_GAMEPAD_A,
            ..Default::default()
        });
        assert_eq!(report_due(None, None, now), None);
        assert_eq!(report_due(None, Some((pressed, now)), now), Some(NEUTRAL));
        assert_eq!(
            report_due(None, Some((NEUTRAL, now)), now + POLL_INTERVAL),
            None
        );
        assert_eq!(
            report_due(None, Some((NEUTRAL, now)), now + REPORT_INTERVAL),
            Some(NEUTRAL)
        );
        assert_eq!(
            report_due(Some(pressed), Some((NEUTRAL, now)), now),
            Some(pressed)
        );
    }

    #[test]
    fn native_gamepad_and_release_keep_wire_values_and_order() {
        let (tx, mut rx) = mpsc::channel(2);
        let seq = AtomicU64::new(42);
        assert!(send_report(
            &tx,
            &seq,
            report_event(XINPUT_GAMEPAD {
                wButtons: XINPUT_GAMEPAD_A | XINPUT_GAMEPAD_DPAD_RIGHT,
                bLeftTrigger: 128,
                bRightTrigger: 255,
                sThumbLX: i16::MIN,
                sThumbLY: i16::MAX,
                sThumbRX: 1234,
                sThumbRY: -5678,
            }),
        ));
        assert!(send_report(&tx, &seq, NEUTRAL));

        let active = rx.try_recv().unwrap();
        assert_eq!(active.seq, 42);
        assert_eq!(
            active.event,
            InputEvent::GamepadState {
                index: 0,
                buttons: 0x1008,
                left_trigger: 128,
                right_trigger: 255,
                left_x: i16::MIN,
                left_y: i16::MAX,
                right_x: 1234,
                right_y: -5678,
            },
        );
        let release = rx.try_recv().unwrap();
        assert_eq!(release.seq, 43);
        assert_eq!(
            release.event,
            InputEvent::GamepadState {
                index: 0,
                buttons: 0,
                left_trigger: 0,
                right_trigger: 0,
                left_x: 0,
                left_y: 0,
                right_x: 0,
                right_y: 0,
            },
        );
    }
}
