use crate::protocol::ControlMessage;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use windows::Win32::Foundation::POINT;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation8, IUIAutomation2, IUIAutomationTextEditPattern, IUIAutomationValuePattern,
    UIA_EditControlTypeId, UIA_TextEditPatternId, UIA_ValuePatternId,
};

pub fn watch_remote_taps(
    control: mpsc::Sender<ControlMessage>,
) -> watch::Sender<Option<(u32, POINT)>> {
    let (sender, mut taps) = watch::channel::<Option<(u32, POINT)>>(None);
    tauri::async_runtime::spawn(async move {
        while taps.changed().await.is_ok() {
            // The window receiving the click needs time to apply keyboard focus.
            tokio::time::sleep(Duration::from_millis(70)).await;
            let Some((tap_id, point)) = *taps.borrow_and_update() else {
                continue;
            };
            let editable = tokio::task::spawn_blocking(move || focused_edit_at(point))
                .await
                .unwrap_or(false);
            if matches!(taps.has_changed(), Ok(false)) {
                let _ = control.try_send(ControlMessage::TextFocus { tap_id, editable });
            }
        }
    });
    sender
}

fn focused_edit_at(point: POINT) -> bool {
    unsafe {
        if CoInitializeEx(None, COINIT_MULTITHREADED).is_err() {
            return false;
        }
        let editable = query_focused_edit(point).unwrap_or(false);
        CoUninitialize();
        editable
    }
}

unsafe fn query_focused_edit(point: POINT) -> windows::core::Result<bool> {
    unsafe {
        let automation: IUIAutomation2 =
            CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER)?;
        let _ = automation.SetConnectionTimeout(300);
        let _ = automation.SetTransactionTimeout(300);
        let element = automation.GetFocusedElement()?;
        if !element.CurrentHasKeyboardFocus()?.as_bool() || !element.CurrentIsEnabled()?.as_bool() {
            return Ok(false);
        }
        let bounds = element.CurrentBoundingRectangle()?;
        if point.x < bounds.left
            || point.x >= bounds.right
            || point.y < bounds.top
            || point.y >= bounds.bottom
        {
            return Ok(false);
        }
        if let Ok(value) =
            element.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
        {
            return Ok(!value.CurrentIsReadOnly()?.as_bool());
        }
        Ok(element.CurrentControlType()? == UIA_EditControlTypeId
            || element
                .GetCurrentPatternAs::<IUIAutomationTextEditPattern>(UIA_TextEditPatternId)
                .is_ok())
    }
}
