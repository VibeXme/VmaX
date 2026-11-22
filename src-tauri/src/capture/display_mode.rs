use windows::Win32::Graphics::Gdi::{
    CDS_TEST, CDS_TYPE, ChangeDisplaySettingsExW, DEVMODEW, DISP_CHANGE_SUCCESSFUL, DM_BITSPERPEL,
    DM_DISPLAYFLAGS, DM_DISPLAYFREQUENCY, DM_PELSHEIGHT, DM_PELSWIDTH, ENUM_CURRENT_SETTINGS,
    ENUM_DISPLAY_SETTINGS_MODE, EnumDisplaySettingsW,
};
use windows_core::PCWSTR;

pub(crate) struct DisplayMode {
    device: Vec<u16>,
    original: DEVMODEW,
    applied: DEVMODEW,
}

impl DisplayMode {
    pub(crate) fn apply(device: &str, target: (u32, u32, u32)) -> Result<Option<Self>, String> {
        let device = device.encode_utf16().chain([0]).collect::<Vec<_>>();
        let name = PCWSTR(device.as_ptr());
        let original = read_mode(name, ENUM_CURRENT_SETTINGS).ok_or("Monitor is unavailable")?;
        if dimensions(&original) == target {
            return Ok(None);
        }
        let mut mode = (0..)
            .map(ENUM_DISPLAY_SETTINGS_MODE)
            .map_while(|index| read_mode(name, index))
            .find(|mode| {
                dimensions(mode) == target
                    && mode.dmBitsPerPel == original.dmBitsPerPel
                    && unsafe {
                        mode.Anonymous2.dmDisplayFlags == original.Anonymous2.dmDisplayFlags
                            && mode.Anonymous1.Anonymous2.dmDisplayOrientation
                                == original.Anonymous1.Anonymous2.dmDisplayOrientation
                    }
            })
            .ok_or("Monitor does not advertise the requested resolution and refresh rate")?;
        // Leave monitor placement and orientation under Windows' existing configuration.
        mode.dmFields =
            DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY | DM_BITSPERPEL | DM_DISPLAYFLAGS;
        if unsafe { ChangeDisplaySettingsExW(name, Some(&mode), None, CDS_TEST, None) }
            != DISP_CHANGE_SUCCESSFUL
        {
            return Err("Windows rejected the requested display mode".into());
        }
        if unsafe { ChangeDisplaySettingsExW(name, Some(&mode), None, CDS_TYPE(0), None) }
            != DISP_CHANGE_SUCCESSFUL
        {
            return Err("Windows could not apply the requested display mode".into());
        }
        let applied = read_mode(name, ENUM_CURRENT_SETTINGS).unwrap_or(mode);
        Ok(Some(Self {
            device,
            original,
            applied,
        }))
    }
}

impl Drop for DisplayMode {
    fn drop(&mut self) {
        let name = PCWSTR(self.device.as_ptr());
        // Preserve changes the user made to the display during the stream.
        if read_mode(name, ENUM_CURRENT_SETTINGS)
            .is_some_and(|mode| same_mode(&mode, &self.applied))
            && unsafe {
                ChangeDisplaySettingsExW(name, Some(&self.original), None, CDS_TYPE(0), None)
            } != DISP_CHANGE_SUCCESSFUL
        {
            tracing::warn!("Windows could not restore the previous display mode");
        }
    }
}

fn read_mode(name: PCWSTR, index: ENUM_DISPLAY_SETTINGS_MODE) -> Option<DEVMODEW> {
    let mut mode = DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    unsafe { EnumDisplaySettingsW(name, index, &mut mode) }
        .as_bool()
        .then_some(mode)
}

fn dimensions(mode: &DEVMODEW) -> (u32, u32, u32) {
    (mode.dmPelsWidth, mode.dmPelsHeight, mode.dmDisplayFrequency)
}

fn same_mode(a: &DEVMODEW, b: &DEVMODEW) -> bool {
    dimensions(a) == dimensions(b)
        && a.dmBitsPerPel == b.dmBitsPerPel
        && unsafe {
            let a_display = a.Anonymous1.Anonymous2;
            let b_display = b.Anonymous1.Anonymous2;
            a.Anonymous2.dmDisplayFlags == b.Anonymous2.dmDisplayFlags
                && a_display.dmDisplayOrientation == b_display.dmDisplayOrientation
                && a_display.dmPosition.x == b_display.dmPosition.x
                && a_display.dmPosition.y == b_display.dmPosition.y
        }
}
