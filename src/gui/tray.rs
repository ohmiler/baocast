//! The notification-area icon: shows LIVE and the time in its tooltip, and a
//! right-click menu for muting or ending the stream without the window.

use super::*;
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW, Shell_NotifyIconW,
};

fn data(hwnd: HWND, tip: &str) -> NOTIFYICONDATAW {
    let mut data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: WM_TRAY,
        hIcon: unsafe { LoadIconW(None, IDI_APPLICATION) }.unwrap_or_default(),
        ..Default::default()
    };
    for (slot, unit) in data.szTip.iter_mut().take(127).zip(tip.encode_utf16()) {
        *slot = unit;
    }
    data
}

pub(super) fn add(hwnd: HWND, tip: &str) {
    unsafe {
        let _ = Shell_NotifyIconW(NIM_ADD, &data(hwnd, tip));
    }
}

pub(super) fn update(hwnd: HWND, tip: &str) {
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &data(hwnd, tip));
    }
}

pub(super) fn remove(hwnd: HWND) {
    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &data(hwnd, ""));
    }
}

/// Shows a menu at the cursor and returns the chosen id (0 for none).
/// Items are (id, label, checked); an id of 0 is a separator.
pub(super) fn menu(hwnd: HWND, items: &[(u32, &str, bool)]) -> u32 {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return 0 };
        let mut last_was_separator = true;
        for &(id, label, ticked) in items {
            if id == 0 {
                if !last_was_separator {
                    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
                    last_was_separator = true;
                }
                continue;
            }
            let flags = if ticked { MF_STRING | MF_CHECKED } else { MF_STRING };
            let _ = AppendMenuW(menu, flags, id as usize, &HSTRING::from(label));
            last_was_separator = false;
        }
        // Without this the menu doesn't close when you click elsewhere.
        let _ = SetForegroundWindow(hwnd);
        let at = cursor();
        let chosen = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY, at.x, at.y, None, hwnd, None);
        let _ = DestroyMenu(menu);
        chosen.0 as u32
    }
}
