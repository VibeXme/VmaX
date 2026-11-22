// Desktop Duplication and the encoder use separate threads and GPU surfaces.
use super::*;
use std::ops::{Deref, DerefMut};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU8, AtomicU16, Ordering},
};
use std::thread::{self, JoinHandle};
use windows::Win32::Graphics::Direct3D11::ID3D11Multithread;

#[derive(Default)]
struct Mailbox {
    ready: bool,
    texture_changed: bool,
    discontinuity: bool,
    static_screen: bool,
    pointer: Option<Arc<crate::cursor::PointerShape>>,
    position: (i32, i32),
    visible: bool,
    error: Option<String>,
    capture_count: u32,
    capture_time: Duration,
}

pub struct CaptureWorker {
    view: DxgiCapturer,
    // GPU commands copying into and out of this surface are ordered by the mailbox mutex.
    snapshot: ID3D11Texture2D,
    mailbox: Arc<Mutex<Mailbox>>,
    sharpness: Arc<AtomicU8>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Deref for CaptureWorker {
    type Target = DxgiCapturer;
    fn deref(&self) -> &DxgiCapturer {
        &self.view
    }
}
impl DerefMut for CaptureWorker {
    fn deref_mut(&mut self) -> &mut DxgiCapturer {
        &mut self.view
    }
}

impl CaptureWorker {
    pub fn new(
        w: u32,
        h: u32,
        display: Option<&str>,
        fps: Arc<AtomicU16>,
        connected: Arc<AtomicBool>,
        pointer: Arc<AtomicBool>,
    ) -> Result<Self, String> {
        let mut source = DxgiCapturer::new(w, h, display)?;
        let protection = source
            .context
            .cast::<ID3D11Multithread>()
            .map_err(|e| e.to_string())?;
        unsafe {
            let _ = protection.SetMultithreadProtected(true);
        }
        let blank = || {
            create_bgra_texture(
                &source.device,
                source.encode_width,
                source.encode_height,
                false,
            )
        };
        let snapshot = blank()?;
        let view = DxgiCapturer {
            device: source.device.clone(),
            context: source.context.clone(),
            work_tex: blank()?,
            rotation: source.rotation,
            desktop_bounds: source.desktop_bounds,
            native_width: source.native_width,
            native_height: source.native_height,
            encode_width: source.encode_width,
            encode_height: source.encode_height,
            adapter_vendor_id: source.adapter_vendor_id,
            adapter_luid: source.adapter_luid,
            vram_total_gb: source.vram_total_gb,
            adapter_name: source.adapter_name.clone(),
            acquired_frame: None,
            capture_discontinuity: false,
            duplication: None,
            output1: None,
            staging_tex: None,
            scaler: None,
            has_valid_frame: false,
            surface_size: (0, 0),
            static_screen: false,
            refinement_frames_left: 0,
            pointer_shape: None,
            pointer_position: (0, 0),
            pointer_visible: false,
            pointer_overlay: true,
        };
        let mut worker = Self {
            view,
            snapshot,
            mailbox: Default::default(),
            sharpness: Arc::new(AtomicU8::new(35)),
            stop: Default::default(),
            thread: None,
        };
        let target = worker.snapshot.clone();
        let mailbox = worker.mailbox.clone();
        let stop = worker.stop.clone();
        let sharpness = worker.sharpness.clone();
        worker.thread = Some(
            thread::Builder::new()
                .name("vmax-dxgi-capture".into())
                .spawn(move || {
                    let _timer = super::super::WindowsTimerPeriod::new();
                    let mut previous_sharpness = None;
                    let mut last_published: Option<Instant> = None;
                    while !stop.load(Ordering::Acquire) {
                        if !connected.load(Ordering::Relaxed) {
                            last_published = None;
                            thread::park_timeout(Duration::from_millis(25));
                            continue;
                        }
                        let level = sharpness.load(Ordering::Relaxed);
                        if previous_sharpness != Some(level) {
                            source.set_sharpness(level);
                            previous_sharpness = Some(level);
                        }
                        source.set_pointer_overlay(pointer.load(Ordering::Relaxed));
                        // DXGI wakes for a desktop present. Shorter waits keep
                        // the static-screen path responsive above 60 Hz.
                        let target_fps = fps.load(Ordering::Relaxed).max(1);
                        let wait_ms = (1000 / u32::from(target_fps)).clamp(1, 16);
                        let started = Instant::now();
                        let result = source.capture_frame(wait_ms, false);
                        let capture_time = started.elapsed();
                        let retry = matches!(&result, Ok(false)) && !source.has_valid_frame();
                        {
                            let mut slot = mailbox.lock().unwrap_or_else(|e| e.into_inner());
                            if source.take_capture_discontinuity() {
                                slot.discontinuity = true;
                                slot.ready = false;
                                slot.texture_changed = false;
                                last_published = None;
                            }
                            match result {
                                Ok(true) => {
                                    let now = Instant::now();
                                    if source.static_screen
                                        && last_published.is_some_and(|last| {
                                            now.duration_since(last)
                                                < Duration::from_millis(u64::from(wait_ms))
                                        })
                                    {
                                        continue;
                                    }
                                    last_published = Some(now);
                                    if !source.static_screen {
                                        slot.texture_changed = true;
                                        unsafe {
                                            source.context.CopyResource(&target, source.texture());
                                        }
                                    }
                                    slot.ready = true;
                                    slot.static_screen = source.static_screen;
                                    slot.pointer = source.pointer_shape.clone();
                                    slot.position = source.pointer_position;
                                    slot.visible = source.pointer_visible;
                                    slot.capture_count += 1;
                                    slot.capture_time += capture_time;
                                }
                                Ok(false) => {}
                                Err(error) => {
                                    slot.error = Some(error);
                                    break;
                                }
                            }
                        }
                        // Recreating a lost duplication can return immediately.
                        if retry {
                            thread::park_timeout(Duration::from_millis(5));
                        }
                    }
                })
                .map_err(|e| e.to_string())?,
        );
        Ok(worker)
    }

    pub fn set_sharpness(&self, level: u8) {
        self.sharpness.store(level, Ordering::Relaxed);
    }
    pub fn take_capture_sample(&self) -> (u32, Duration) {
        let mut slot = self.mailbox.lock().unwrap_or_else(|e| e.into_inner());
        (
            std::mem::take(&mut slot.capture_count),
            std::mem::take(&mut slot.capture_time),
        )
    }

    pub fn latest_frame(&mut self, force: bool) -> Result<bool, String> {
        let mut slot = self.mailbox.lock().map_err(|_| "Capture thread stopped")?;
        if let Some(error) = &slot.error {
            return Err(error.clone());
        }
        if std::mem::take(&mut slot.discontinuity) {
            self.view.has_valid_frame = false;
            self.view.capture_discontinuity = true;
        }
        if !std::mem::take(&mut slot.ready) {
            return Ok(self.view.has_valid_frame && (force || is_constant_fps()));
        }
        if std::mem::take(&mut slot.texture_changed) {
            unsafe {
                self.view
                    .context
                    .CopyResource(self.view.texture(), &self.snapshot);
            }
        }
        self.view.has_valid_frame = true;
        self.view.static_screen = slot.static_screen;
        self.view.pointer_shape = slot.pointer.clone();
        self.view.pointer_position = slot.position;
        self.view.pointer_visible = slot.visible;
        Ok(true)
    }
}

impl Drop for CaptureWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}
