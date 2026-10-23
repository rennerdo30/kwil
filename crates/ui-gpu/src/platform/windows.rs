use super::super::UserEvent;
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
};
use windows_sys::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, POINTL, WPARAM},
        Graphics::Dwm::{DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute},
        System::{
            Com::{DVASPECT_CONTENT, FORMATETC, STGMEDIUM, TYMED_HGLOBAL},
            Ole::{
                DROPEFFECT_COPY, DROPEFFECT_NONE, OleInitialize, OleUninitialize, RegisterDragDrop, ReleaseStgMedium,
                RevokeDragDrop,
            },
            RemoteDesktop::{
                NOTIFY_FOR_THIS_SESSION, WTSRegisterSessionNotification, WTSUnRegisterSessionNotification,
            },
        },
        UI::{
            Shell::{DefSubclassProc, DragQueryFileW, RemoveWindowSubclass, SetWindowSubclass},
            WindowsAndMessaging::{
                WM_WTSSESSION_CHANGE, WTS_CONSOLE_CONNECT, WTS_REMOTE_CONNECT, WTS_REMOTE_DISCONNECT,
            },
        },
    },
    core::GUID,
};
type SendEvent = Box<dyn Fn(UserEvent) + Send + Sync>;
const SUBCLASS_ID: usize = 0x4b57494c;

pub(crate) struct Native {
    hwnd: HWND,
    state: Arc<State>,
    ole: bool,
    target: *mut Target,
    session: bool,
    subclass: bool,
}
struct State {
    send: SendEvent,
}
impl Native {
    pub fn register(hwnd: isize, send: impl Fn(UserEvent) + Send + Sync + 'static) -> Self {
        let hwnd = hwnd as HWND;
        let state = Arc::new(State { send: Box::new(send) });
        let ptr = Arc::as_ptr(&state) as usize;
        let subclass = unsafe { SetWindowSubclass(hwnd, Some(window_proc), SUBCLASS_ID, ptr) != 0 };
        let session = subclass && unsafe { WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION) != 0 };
        let ole = unsafe { OleInitialize(std::ptr::null()) >= 0 };
        let mut target = std::ptr::null_mut();
        if ole {
            target =
                Box::into_raw(Box::new(Target { vtable: &DROP_VTABLE, refs: AtomicU32::new(1), state: state.clone() }));
            if unsafe { RegisterDragDrop(hwnd, target.cast()) } < 0 {
                unsafe {
                    release(target);
                }
                target = std::ptr::null_mut();
            }
        }
        if !session {
            (state.send)(UserEvent::Notice(
                "Session notifications unavailable; remote override remains available".into(),
            ));
        }
        if target.is_null() {
            (state.send)(UserEvent::Notice("File drop registration unavailable".into()));
        }
        #[cfg(feature = "frame-harness")]
        println!(
            "kwil-native subclass={subclass} session_notifications={session} ole={ole} drop_target={}",
            !target.is_null()
        );
        Self { hwnd, state, ole, target, session, subclass }
    }
    pub fn theme(&self, dark: bool, remote: bool) {
        decorate_window(self.hwnd as isize, dark, remote, false);
    }
}

/// Keep OS-drawn Windows 11 caption buttons, Snap and hit-testing. Unsupported
/// attributes are ignored by Windows 10; remote mode retains opaque surfaces.
pub(crate) fn decorate_window(hwnd: isize, dark: bool, remote: bool, dialog: bool) {
    let dark = i32::from(dark);
    let backdrop = if remote {
        1i32
    } else if dialog {
        3
    } else {
        2
    };
    let round = 2i32; // DWMWCP_ROUND
    let enabled = 2i32; // DWMNCRP_ENABLED
    let theme = super::super::theme_ui::chrome();
    let colorref = |role| {
        let rgb = super::super::theme_ui::rgb(theme, role);
        ((rgb & 0xff) << 16) | (rgb & 0xff00) | ((rgb >> 16) & 0xff)
    };
    let caption = colorref(kwil_core_commands::theme::UiRole::PanelBackground);
    let text = colorref(kwil_core_commands::theme::UiRole::PanelForeground);
    let border = colorref(kwil_core_commands::theme::UiRole::Border);
    unsafe {
        let hwnd = hwnd as HWND;
        DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE as u32, (&dark as *const i32).cast(), 4);
        DwmSetWindowAttribute(hwnd, DWMWA_SYSTEMBACKDROP_TYPE as u32, (&backdrop as *const i32).cast(), 4);
        DwmSetWindowAttribute(hwnd, 33, (&round as *const i32).cast(), 4);
        DwmSetWindowAttribute(hwnd, 2, (&enabled as *const i32).cast(), 4);
        DwmSetWindowAttribute(hwnd, 35, (&caption as *const u32).cast(), 4);
        DwmSetWindowAttribute(hwnd, 36, (&text as *const u32).cast(), 4);
        DwmSetWindowAttribute(hwnd, 34, (&border as *const u32).cast(), 4);
    }
}
impl Drop for Native {
    fn drop(&mut self) {
        unsafe {
            if self.session {
                WTSUnRegisterSessionNotification(self.hwnd);
            }
            if self.subclass {
                RemoveWindowSubclass(self.hwnd, Some(window_proc), SUBCLASS_ID);
            }
            if !self.target.is_null() {
                RevokeDragDrop(self.hwnd);
                release(self.target);
            }
            if self.ole {
                OleUninitialize();
            }
            // State remains alive until both the subclass and COM registration are gone.
            let _ = &self.state;
        }
    }
}
unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM, _id: usize, data: usize) -> LRESULT {
    if msg == WM_WTSSESSION_CHANGE {
        let remote = match w as u32 {
            WTS_REMOTE_CONNECT | WTS_REMOTE_DISCONNECT => true,
            WTS_CONSOLE_CONNECT => false,
            _ => super::super::remote::detect(),
        };
        // SAFETY: Native owns this stable Box and removes the subclass before drop.
        unsafe {
            ((&*(data as *const State)).send)(UserEvent::SessionChanged(remote));
        }
    }
    unsafe { DefSubclassProc(hwnd, msg, w, l) }
}

// Minimal IDropTarget ABI: avoids another COM wrapper/proc-macro dependency.
// OLE invokes this apartment-bound object on the owning UI thread.
#[repr(C)]
struct Target {
    vtable: *const DropVtable,
    refs: AtomicU32,
    state: Arc<State>,
}
#[repr(C)]
struct DropVtable {
    query: unsafe extern "system" fn(*mut Target, *const GUID, *mut *mut std::ffi::c_void) -> i32,
    add: unsafe extern "system" fn(*mut Target) -> u32,
    release: unsafe extern "system" fn(*mut Target) -> u32,
    enter: unsafe extern "system" fn(*mut Target, *mut std::ffi::c_void, u32, POINTL, *mut u32) -> i32,
    over: unsafe extern "system" fn(*mut Target, u32, POINTL, *mut u32) -> i32,
    leave: unsafe extern "system" fn(*mut Target) -> i32,
    drop: unsafe extern "system" fn(*mut Target, *mut std::ffi::c_void, u32, POINTL, *mut u32) -> i32,
}
static DROP_VTABLE: DropVtable = DropVtable { query, add, release, enter, over, leave, drop: drop_files };
unsafe extern "system" fn query(t: *mut Target, iid: *const GUID, out: *mut *mut std::ffi::c_void) -> i32 {
    if out.is_null() || iid.is_null() {
        return 0x80004003u32 as i32;
    }
    unsafe {
        *out = std::ptr::null_mut();
    }
    let iid = unsafe { &*iid };
    if (iid.data1 == 0 || iid.data1 == 0x122)
        && iid.data2 == 0
        && iid.data3 == 0
        && iid.data4 == [0xc0, 0, 0, 0, 0, 0, 0, 0x46]
    {
        unsafe {
            *out = t.cast();
            add(t);
        }
        0
    } else {
        0x80004002u32 as i32
    }
}
unsafe extern "system" fn add(t: *mut Target) -> u32 {
    unsafe { (*t).refs.fetch_add(1, Ordering::Relaxed) + 1 }
}
unsafe extern "system" fn release(t: *mut Target) -> u32 {
    let n = unsafe { (*t).refs.fetch_sub(1, Ordering::Release) - 1 };
    if n == 0 {
        std::sync::atomic::fence(Ordering::Acquire);
        unsafe {
            drop(Box::from_raw(t));
        }
    }
    n
}
unsafe extern "system" fn enter(
    _t: *mut Target,
    _data: *mut std::ffi::c_void,
    _keys: u32,
    _point: POINTL,
    effect: *mut u32,
) -> i32 {
    unsafe {
        *effect &= DROPEFFECT_COPY;
    }
    0
}
unsafe extern "system" fn over(_t: *mut Target, _keys: u32, _point: POINTL, effect: *mut u32) -> i32 {
    unsafe {
        *effect &= DROPEFFECT_COPY;
    }
    0
}
unsafe extern "system" fn leave(_t: *mut Target) -> i32 {
    0
}
unsafe extern "system" fn drop_files(
    t: *mut Target,
    data: *mut std::ffi::c_void,
    _keys: u32,
    _point: POINTL,
    effect: *mut u32,
) -> i32 {
    unsafe {
        *effect = DROPEFFECT_NONE;
    }
    if data.is_null() {
        return 0;
    }
    // IDataObject's first method after IUnknown is GetData (slot 3).
    type GetData = unsafe extern "system" fn(*mut std::ffi::c_void, *const FORMATETC, *mut STGMEDIUM) -> i32;
    let get: GetData = unsafe { std::mem::transmute(*(*(data as *const *const *const std::ffi::c_void)).add(3)) };
    let format = FORMATETC {
        cfFormat: 15,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT,
        lindex: -1,
        tymed: TYMED_HGLOBAL as u32,
    };
    let mut medium: STGMEDIUM = unsafe { std::mem::zeroed() };
    if unsafe { get(data, &format, &mut medium) } < 0 {
        return 0;
    }
    if medium.tymed == TYMED_HGLOBAL as u32 {
        let handle = unsafe { medium.u.hGlobal };
        let count = unsafe { DragQueryFileW(handle, 0xffffffff, std::ptr::null_mut(), 0) }.min(1024);
        for i in 0..count {
            let len = unsafe { DragQueryFileW(handle, i, std::ptr::null_mut(), 0) };
            if len > 32768 {
                continue;
            }
            let mut path = vec![0u16; len as usize + 1];
            unsafe {
                DragQueryFileW(handle, i, path.as_mut_ptr(), len + 1);
            }
            use std::os::windows::ffi::OsStringExt;
            let path = PathBuf::from(std::ffi::OsString::from_wide(&path[..len as usize]));
            unsafe {
                let target = &*t;
                (target.state.send)(UserEvent::DroppedFile(path));
            }
        }
        unsafe {
            *effect = DROPEFFECT_COPY;
        }
    }
    unsafe {
        ReleaseStgMedium(&mut medium);
    }
    0
}
