use crate::haptics::Rumble;
use crate::protocol::{InputEvent, InputPacket};
use std::sync::{
    Arc,
    atomic::{AtomicU16, Ordering},
};
use std::time::{Duration, Instant};
use vigem_client::{Client, TargetId, XButtons, XGamepad, Xbox360Wired};
use windows::Win32::Foundation::ERROR_NO_MORE_ITEMS;

const REPORT_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Default)]
pub struct HostGamepad {
    target: Option<Xbox360Wired<Client>>,
    retry_after: Option<Instant>,
    last_seq: Option<u64>,
    last_report_at: Option<Instant>,
    report: XGamepad,
    motors: Arc<AtomicU16>,
}

impl HostGamepad {
    pub fn reconnect(&mut self) {
        let previous_seq = self.last_seq;
        *self = Self::default();
        self.last_seq = previous_seq;
    }

    pub fn windows_port(&mut self) -> Option<u8> {
        self.target
            .as_mut()?
            .get_user_index()
            .ok()
            .filter(|&index| index < 4)
            .map(|index| index as u8 + 1)
    }

    pub async fn receive(&mut self, packet: &InputPacket) -> Result<(), String> {
        let InputEvent::GamepadState {
            index: 0,
            buttons,
            left_trigger,
            right_trigger,
            left_x,
            left_y,
            right_x,
            right_y,
        } = packet.event
        else {
            return Ok(());
        };

        // Raw UDP can deliver older reports after a newer release.
        if self.retry_after.is_some_and(|time| Instant::now() < time)
            || self.last_seq.is_some_and(|seq| packet.seq <= seq)
        {
            return Ok(());
        }
        if self.target.is_none() {
            let result = tokio::task::spawn_blocking(|| {
                let client = Client::connect()?;
                let mut target = Xbox360Wired::new(client, TargetId::XBOX360_WIRED);
                target.plugin()?;
                target.wait_ready()?;
                Ok::<_, vigem_client::Error>(target)
            })
            .await
            .map_err(|err| err.to_string())
            .and_then(|result| result.map_err(|err| err.to_string()));

            match result {
                Ok(target) => {
                    let mut target = target;
                    self.motors = Arc::new(AtomicU16::new(0));
                    if let Ok(notification) = target.request_notification() {
                        let motors = Arc::clone(&self.motors);
                        notification.spawn_thread(move |_, data| {
                            motors.store(
                                u16::from_le_bytes([data.large_motor, data.small_motor]),
                                Ordering::Release,
                            );
                        });
                    }
                    self.target = Some(target);
                    self.report = XGamepad::default();
                    self.retry_after = None;
                }
                Err(err) => {
                    self.retry_after = Some(Instant::now() + Duration::from_secs(5));
                    return Err(format!(
                        "Xbox controller driver unavailable. Install or repair ViGEmBus on the host; VmaX retries automatically ({err})."
                    ));
                }
            }
        }

        self.last_seq = Some(packet.seq);
        self.last_report_at = Some(Instant::now());
        self.update(XGamepad {
            // Bit 11 is reserved in the Xbox 360 report.
            buttons: XButtons(buttons & 0xf7ff),
            left_trigger,
            right_trigger,
            thumb_lx: left_x,
            thumb_ly: left_y,
            thumb_rx: right_x,
            thumb_ry: right_y,
        })
    }

    pub fn rumble(&self) -> Rumble {
        if !self
            .last_report_at
            .is_some_and(|time| time.elapsed() < REPORT_TIMEOUT)
            || self.target.is_none()
        {
            return Rumble::default();
        }
        let [large, small] = self.motors.load(Ordering::Acquire).to_le_bytes();
        Rumble {
            large,
            small,
            pad: 0,
        }
    }

    pub fn expire(&mut self) -> Result<(), String> {
        if self
            .last_report_at
            .is_some_and(|time| time.elapsed() >= REPORT_TIMEOUT)
        {
            self.update(XGamepad::default())?;
            if self.report == XGamepad::default() {
                self.last_report_at = None;
            }
        }
        Ok(())
    }

    pub fn neutralize(&mut self) -> Result<(), String> {
        self.update(XGamepad::default())?;
        self.last_report_at =
            (self.report != XGamepad::default()).then(|| Instant::now() - REPORT_TIMEOUT);
        Ok(())
    }

    fn update(&mut self, report: XGamepad) -> Result<(), String> {
        if report != self.report
            && let Some(target) = self.target.as_mut()
        {
            match target.update(&report) {
                Ok(()) => self.report = report,
                // ViGEmBus has no pending XUSB read yet. Keep the target and retry
                // with the next input snapshot, or on the next release timeout tick.
                Err(vigem_client::Error::TargetNotReady) => {}
                Err(vigem_client::Error::WinError(code)) if code == ERROR_NO_MORE_ITEMS.0 => {}
                Err(err) => {
                    self.target = None;
                    self.retry_after = Some(Instant::now() + Duration::from_secs(5));
                    return Err(format!("ViGEmBus controller update failed ({err})."));
                }
            }
        }
        Ok(())
    }
}

impl Drop for HostGamepad {
    fn drop(&mut self) {
        if let Some(target) = self.target.as_mut() {
            let _ = target.update(&XGamepad::default());
            let _ = target.unplug();
        }
    }
}
