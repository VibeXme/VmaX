use super::FrontendInputPayload;
use std::{cell::RefCell, collections::HashSet};
use tokio::sync::mpsc;
use windows::{
    Win32::{
        Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM},
        System::LibraryLoader::GetModuleHandleW,
        UI::WindowsAndMessaging::{
            CallNextHookEx, GetForegroundWindow, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT, LLKHF_EXTENDED,
            LLKHF_INJECTED, LLKHF_UP, SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL,
        },
    },
    core::PCWSTR,
};

pub enum CaptureEvent {
    Key(FrontendInputPayload),
    Released,
}

struct Capture {
    id: u32,
    hook: HHOOK,
    hwnd: HWND,
    shortcut: (u32, u8),
    held: HashSet<u32>,
    sender: mpsc::UnboundedSender<CaptureEvent>,
}

thread_local! {
    static CAPTURE: RefCell<Option<Capture>> = const { RefCell::new(None) };
}

fn modifier(scan: u32) -> u8 {
    match scan {
        0x2a | 0x36 => 1,
        0x1d | 0xe01d => 2,
        0x38 | 0xe038 => 4,
        0xe05b | 0xe05c => 8,
        _ => 0,
    }
}

impl Capture {
    fn key(&mut self, code: u32, pressed: bool) -> bool {
        let modifiers = self
            .held
            .iter()
            .fold(0, |mask, &scan| mask | modifier(scan));
        if pressed && (code, modifiers) == self.shortcut {
            return false;
        }
        if pressed {
            self.held.insert(code);
        } else if !self.held.remove(&code) {
            return true;
        }
        let _ = self
            .sender
            .send(CaptureEvent::Key(FrontendInputPayload::Key {
                scan_code: code,
                pressed,
                modifiers: 0,
            }));
        true
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = unsafe { UnhookWindowsHookEx(self.hook) };
        for scan_code in self.held.drain() {
            let _ = self
                .sender
                .send(CaptureEvent::Key(FrontendInputPayload::Key {
                    scan_code,
                    pressed: false,
                    modifiers: 0,
                }));
        }
        let _ = self.sender.send(CaptureEvent::Released);
    }
}

pub fn stop(id: u32) {
    CAPTURE.with_borrow_mut(|capture| {
        if capture.as_ref().is_some_and(|active| active.id == id) {
            capture.take();
        }
    });
}

pub fn start(
    id: u32,
    hwnd: HWND,
    shortcut: (u32, u8),
    sender: mpsc::UnboundedSender<CaptureEvent>,
) -> Result<(), String> {
    let (scan, mask) = shortcut;
    if !matches!(scan, 1..=0x7f | 0xe001..=0xe07f)
        || modifier(scan) != 0
        || mask & !7 != 0
        || mask.count_ones() < 2
    {
        return Err("Use two modifiers and a regular key to release capture".into());
    }
    CAPTURE.with_borrow_mut(|capture| {
        capture.take();
    });
    let module = unsafe { GetModuleHandleW(PCWSTR::null()) }.map_err(|error| error.to_string())?;
    let hook = unsafe {
        SetWindowsHookExW(
            WH_KEYBOARD_LL,
            Some(keyboard_event),
            Some(HINSTANCE(module.0)),
            0,
        )
    }
    .map_err(|error| error.to_string())?;
    CAPTURE.with_borrow_mut(|capture| {
        *capture = Some(Capture {
            id,
            hook,
            hwnd,
            shortcut,
            held: HashSet::new(),
            sender,
        });
    });
    Ok(())
}

unsafe extern "system" fn keyboard_event(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && lparam.0 != 0 {
        let event = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        if !event.flags.contains(LLKHF_INJECTED) && event.scanCode != 0 {
            let swallowed = CAPTURE.with_borrow_mut(|slot| {
                let Some(capture) = slot.as_mut() else {
                    return false;
                };
                if unsafe { GetForegroundWindow() } != capture.hwnd || capture.sender.is_closed() {
                    slot.take();
                    return false;
                }
                let prefix = if event.flags.contains(LLKHF_EXTENDED) {
                    0xe000
                } else {
                    0
                };
                if !capture.key(
                    prefix | (event.scanCode & 0xff),
                    !event.flags.contains(LLKHF_UP),
                ) {
                    slot.take();
                }
                true
            });
            if swallowed {
                return LRESULT(1);
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}
