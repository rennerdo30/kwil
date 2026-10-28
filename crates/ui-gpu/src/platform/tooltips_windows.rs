//! Standard Windows tooltips for the SVG command buttons; text owners outlive TOOLINFO.
use super::super::{Rect, menu};
use std::collections::HashMap;
use windows_sys::Win32::{
    Foundation::HWND,
    UI::{Controls::*, WindowsAndMessaging::*},
};
struct Tip {
    _text: Vec<u16>,
}
#[derive(Default)]
pub(super) struct Tips {
    window: HWND,
    source: Vec<(&'static str, Rect, String)>,
    tips: Vec<Tip>,
}
impl Tips {
    pub fn update(&mut self, owner: HWND, tools: &[menu::Tool], labels: &HashMap<&'static str, String>) {
        let source: Vec<_> = tools
            .iter()
            .map(|t| {
                let name = match t.command {
                    "file.open" => "Open file",
                    "file.save" => "Save",
                    "edit.undo" => "Undo",
                    "edit.redo" => "Redo",
                    "search.find" => "Find",
                    "navigation.goToLine" => "Go to line",
                    "view.settings" => "Settings",
                    _ => t.command,
                };
                let text = labels.get(t.command).map_or_else(|| name.to_owned(), |keys| format!("{name} ({keys})"));
                (t.command, t.rect, text)
            })
            .collect();
        if source == self.source {
            return;
        }
        if self.window.is_null() {
            let init = INITCOMMONCONTROLSEX {
                dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
                dwICC: ICC_WIN95_CLASSES,
            };
            unsafe {
                InitCommonControlsEx(&init);
                self.window = CreateWindowExW(
                    WS_EX_TOPMOST | WS_EX_NOACTIVATE,
                    TOOLTIPS_CLASSW,
                    std::ptr::null(),
                    WS_POPUP | TTS_ALWAYSTIP | TTS_NOPREFIX,
                    0,
                    0,
                    0,
                    0,
                    owner,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                );
            }
            if self.window.is_null() {
                return;
            }
            unsafe {
                SendMessageW(self.window, TTM_SETMAXTIPWIDTH, 0, 480);
            }
        }
        for index in 0..self.tips.len() {
            let info = TTTOOLINFOW {
                cbSize: std::mem::size_of::<TTTOOLINFOW>() as u32,
                hwnd: owner,
                uId: index + 1,
                ..Default::default()
            };
            unsafe {
                SendMessageW(self.window, TTM_DELTOOLW, 0, (&info as *const TTTOOLINFOW) as isize);
            }
        }
        self.tips.clear();
        self.source = source;
        for (index, (_, rect, text)) in self.source.iter().enumerate() {
            let mut text: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
            let info = TTTOOLINFOW {
                cbSize: std::mem::size_of::<TTTOOLINFOW>() as u32,
                uFlags: TTF_SUBCLASS | TTF_TRANSPARENT,
                hwnd: owner,
                uId: index + 1,
                rect: windows_sys::Win32::Foundation::RECT {
                    left: rect.x as i32,
                    top: rect.y as i32,
                    right: (rect.x + rect.w) as i32,
                    bottom: (rect.y + rect.h) as i32,
                },
                lpszText: text.as_mut_ptr(),
                ..Default::default()
            };
            unsafe {
                SendMessageW(self.window, TTM_ADDTOOLW, 0, (&info as *const TTTOOLINFOW) as isize);
            }
            self.tips.push(Tip { _text: text });
        }
    }
}
impl Drop for Tips {
    fn drop(&mut self) {
        if !self.window.is_null() {
            unsafe {
                DestroyWindow(self.window);
            }
        }
    }
}
