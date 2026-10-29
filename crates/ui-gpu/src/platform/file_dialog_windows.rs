//! Modern Shell common-item dialogs, using the existing windows-sys dependency.
//! ABI provenance: Windows SDK 10.0.26100.0 um/ShObjIdl_core.h:
//! IUnknown -> IModalWindow -> IFileDialog -> IFileOpenDialog/IFileSaveDialog.
//! All COM pointers stay on this STA. UI receives only owned paths and tickets.
use crate::file_dialog::{Event, Outcome, Port, Request};
use kwil_view_model::FilePromptKind;
use std::{
    cell::Cell,
    ffi::{OsString, c_void},
    io,
    os::windows::ffi::{OsStrExt, OsStringExt},
    ptr::{null, null_mut},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicIsize, Ordering},
    },
    thread::{self, JoinHandle},
};
use windows_sys::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, WPARAM},
        System::Com::{
            CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance, CoInitializeEx,
            CoTaskMemFree, CoUninitialize,
        },
        UI::{
            Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
            WindowsAndMessaging::{
                CreateWindowExW, DestroyWindow, DispatchMessageW, GetMessageW, HWND_MESSAGE, IsWindow, MSG,
                PostMessageW, TranslateMessage, WM_APP,
            },
        },
    },
    core::{GUID, HRESULT},
};

const WAKE: u32 = WM_APP + 0x56;
const SUBCLASS: usize = 0x4b573536;
const CANCELLED: HRESULT = 0x800704c7u32 as i32;
const MAX_PATH_UNITS: usize = 32768;
const CLSID_OPEN: GUID = GUID::from_u128(0xdc1c5a9c_e88a_4dde_a5a1_60f82a20aef7);
const CLSID_SAVE: GUID = GUID::from_u128(0xc0b4e2f3_ba21_4773_8dba_335ec946eb8b);
const IID_OPEN: GUID = GUID::from_u128(0xd57c7288_d4ad_4768_be02_9d969532d960);
const IID_SAVE: GUID = GUID::from_u128(0x84bccd23_5fde_4cdb_aea4_af64b83d78ab);

// Flags copied from SDK FILEOPENDIALOGOPTIONS. Never permit multi-select,
// virtual namespace results, validation bypass or Shell test-file creation.
const FORCE_FILESYSTEM: u32 = 0x40;
const PATH_MUST_EXIST: u32 = 0x800;
const FILE_MUST_EXIST: u32 = 0x1000;
const OVERWRITE_PROMPT: u32 = 0x2;
const NO_CHANGE_DIR: u32 = 0x8;
const NO_TEST_FILE_CREATE: u32 = 0x10000;
const DONT_ADD_TO_RECENT: u32 = 0x02000000;
const SIGDN_FILESYSPATH: i32 = 0x80058000u32 as i32;

type Release = unsafe extern "system" fn(*mut c_void) -> u32;
#[repr(C)]
struct UnknownVtable {
    query: usize,
    add_ref: usize,
    release: Release,
}
#[repr(C)]
struct DialogVtable {
    unknown: UnknownVtable,                                        // slots 0..=2
    show: unsafe extern "system" fn(*mut c_void, HWND) -> HRESULT, // 3
    unused4_8: [usize; 5],
    set_options: unsafe extern "system" fn(*mut c_void, u32) -> HRESULT, // 9
    get_options: unsafe extern "system" fn(*mut c_void, *mut u32) -> HRESULT, // 10
    unused11_14: [usize; 4],
    set_file_name: unsafe extern "system" fn(*mut c_void, *const u16) -> HRESULT, // 15
    unused16_19: [usize; 4],
    get_result: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT, // 20
    unused21_22: [usize; 2],
    close: unsafe extern "system" fn(*mut c_void, HRESULT) -> HRESULT, // 23
    unused24_26: [usize; 3],
}
#[repr(C)]
struct ShellItemVtable {
    unknown: UnknownVtable,
    unused3_4: [usize; 2],
    get_display_name: unsafe extern "system" fn(*mut c_void, i32, *mut *mut u16) -> HRESULT,
    unused6_7: [usize; 2],
}

// Applies to both x64 and ARM64; there are no packed structs or Rust bool ABI.
const _: () = {
    use std::mem::{align_of, offset_of, size_of};
    let pointer = size_of::<usize>();
    assert!(size_of::<UnknownVtable>() == 3 * pointer);
    assert!(size_of::<DialogVtable>() == 27 * pointer);
    assert!(align_of::<DialogVtable>() == align_of::<usize>());
    assert!(offset_of!(DialogVtable, show) == 3 * pointer);
    assert!(offset_of!(DialogVtable, set_options) == 9 * pointer);
    assert!(offset_of!(DialogVtable, get_options) == 10 * pointer);
    assert!(offset_of!(DialogVtable, set_file_name) == 15 * pointer);
    assert!(offset_of!(DialogVtable, get_result) == 20 * pointer);
    assert!(offset_of!(DialogVtable, close) == 23 * pointer);
    assert!(size_of::<ShellItemVtable>() == 8 * pointer);
    assert!(offset_of!(ShellItemVtable, get_display_name) == 5 * pointer);
};

struct ComPtr(*mut c_void);
impl ComPtr {
    unsafe fn dialog(&self) -> &DialogVtable {
        unsafe { &**(self.0 as *const *const DialogVtable) }
    }
    unsafe fn item(&self) -> &ShellItemVtable {
        unsafe { &**(self.0 as *const *const ShellItemVtable) }
    }
}
impl Drop for ComPtr {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let table = &**(self.0 as *const *const UnknownVtable);
                (table.release)(self.0);
            }
        }
    }
}
struct TaskString(*mut u16);
impl Drop for TaskString {
    fn drop(&mut self) {
        unsafe {
            CoTaskMemFree(self.0.cast());
        }
    }
}
struct Apartment;
impl Apartment {
    fn enter() -> Result<Self, String> {
        check("CoInitializeEx(STA)", unsafe {
            CoInitializeEx(null(), (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32)
        })?;
        Ok(Self)
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}

#[derive(Default)]
struct Shared {
    request: Mutex<Option<Request>>,
    hwnd: AtomicIsize,
    occupied: AtomicBool,
    cancel: AtomicBool,
    shutdown: AtomicBool,
}
struct Pump {
    shared: Arc<Shared>,
    active: Cell<*mut c_void>,
    closing: Cell<bool>,
}

pub(crate) struct Worker {
    shared: Arc<Shared>,
    // Normal UI shutdown waits asynchronously for Stopped before dropping owner.
    // Drop never joins: a COM extension must not block the UI thread's teardown.
    _thread: JoinHandle<()>,
}
impl Worker {
    pub fn spawn(complete: impl Fn(Event) + Send + 'static) -> io::Result<Self> {
        let shared = Arc::new(Shared::default());
        let worker_shared = shared.clone();
        let handle = thread::Builder::new().name("kwil-file-dialog-sta".into()).spawn(move || {
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(worker_shared.clone(), &complete)))
                    .unwrap_or_else(|_| Err("STA worker panicked".into()));
            worker_shared.hwnd.store(0, Ordering::Release);
            worker_shared.shutdown.store(true, Ordering::Release);
            complete(Event::Stopped(result));
        })?;
        Ok(Self { shared, _thread: handle })
    }
    fn wake(&self) -> bool {
        let hwnd = self.shared.hwnd.load(Ordering::Acquire) as HWND;
        !hwnd.is_null() && unsafe { PostMessageW(hwnd, WAKE, 0, 0) != 0 }
    }
}
impl Port for Worker {
    fn submit(&mut self, request: Request) -> Result<(), (Request, String)> {
        if self.shared.shutdown.load(Ordering::Acquire) || self.shared.hwnd.load(Ordering::Acquire) == 0 {
            return Err((request, "STA chooser is not ready or has stopped".into()));
        }
        if self.shared.occupied.swap(true, Ordering::AcqRel) {
            return Err((request, "another native chooser owns the slot".into()));
        }
        let Ok(mut slot) = self.shared.request.lock() else {
            self.shared.occupied.store(false, Ordering::Release);
            return Err((request, "STA request mailbox poisoned".into()));
        };
        self.shared.cancel.store(false, Ordering::Release);
        *slot = Some(request);
        // Post while holding the short mailbox lock. On failure no worker can
        // take the request before it has been returned to the caller.
        if !self.wake() {
            let request = slot.take().unwrap();
            self.shared.occupied.store(false, Ordering::Release);
            return Err((request, format!("STA wake failed: {}", io::Error::last_os_error())));
        }
        Ok(())
    }
    fn cancel(&mut self) {
        if !self.shared.cancel.swap(true, Ordering::AcqRel) {
            self.wake();
        }
    }
    fn shutdown(&mut self) {
        if !self.shared.shutdown.swap(true, Ordering::AcqRel) {
            self.shared.cancel.store(true, Ordering::Release);
            self.wake();
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// Subclass belongs to the STA, so nested modal message dispatch can call Close
// on the SAME apartment as Show. No COM pointer is published in Shared.
unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM, _id: usize, data: usize) -> LRESULT {
    if msg == WAKE {
        let pump = unsafe { &*(data as *const Pump) };
        let dialog = pump.active.get();
        if !dialog.is_null()
            && (pump.shared.cancel.load(Ordering::Acquire) || pump.shared.shutdown.load(Ordering::Acquire))
            && !pump.closing.replace(true)
        {
            let vtable = unsafe { &**(dialog as *const *const DialogVtable) };
            unsafe {
                (vtable.close)(dialog, CANCELLED);
            }
            pump.closing.set(false);
        }
        return 0;
    }
    unsafe { DefSubclassProc(hwnd, msg, w, l) }
}

struct MessageWindow(HWND);
impl Drop for MessageWindow {
    fn drop(&mut self) {
        unsafe {
            RemoveWindowSubclass(self.0, Some(window_proc), SUBCLASS);
            DestroyWindow(self.0);
        }
    }
}

fn run(shared: Arc<Shared>, complete: &impl Fn(Event)) -> Result<(), String> {
    let _apartment = Apartment::enter()?;
    let pump = Box::new(Pump { shared: shared.clone(), active: Cell::new(null_mut()), closing: Cell::new(false) });
    // A system STATIC message-only window is a pump endpoint, never a dialog,
    // visible window, focus target or replacement for the Shell chooser.
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            windows_sys::w!("STATIC"),
            null(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            null_mut(),
            null_mut(),
            null(),
        )
    };
    if hwnd.is_null() {
        return Err(format!("STA message window: {}", io::Error::last_os_error()));
    }
    let window = MessageWindow(hwnd);
    if unsafe { SetWindowSubclass(hwnd, Some(window_proc), SUBCLASS, (&*pump as *const Pump) as usize) } == 0 {
        return Err(format!("STA message subclass: {}", io::Error::last_os_error()));
    }
    shared.hwnd.store(hwnd as isize, Ordering::Release);
    complete(Event::Ready);
    while !shared.shutdown.load(Ordering::Acquire) {
        let mut message: MSG = unsafe { std::mem::zeroed() };
        let got = unsafe { GetMessageW(&mut message, null_mut(), 0, 0) };
        if got == -1 {
            return Err(format!("STA GetMessage: {}", io::Error::last_os_error()));
        }
        if got == 0 {
            break;
        }
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        let request = shared.request.lock().map_err(|_| "STA request mailbox poisoned")?.take();
        if let Some(request) = request {
            let outcome = if shared.cancel.load(Ordering::Acquire) || shared.shutdown.load(Ordering::Acquire) {
                Outcome::Cancelled
            } else {
                choose(&request, &pump)
            };
            shared.occupied.store(false, Ordering::Release);
            complete(Event::Completed { ticket: *request.ticket, outcome });
        }
    }
    // Shutdown may precede message dispatch, including startup. Settle the slot.
    if let Some(request) = shared.request.lock().map_err(|_| "STA request mailbox poisoned")?.take() {
        complete(Event::Completed { ticket: *request.ticket, outcome: Outcome::Cancelled });
    }
    shared.hwnd.store(0, Ordering::Release);
    drop(window); // remove subclass before Pump, COM apartment and owner lifetime end
    Ok(())
}

fn check(stage: &str, hr: HRESULT) -> Result<(), String> {
    if hr < 0 { Err(format!("{stage}: HRESULT 0x{:08x}", hr as u32)) } else { Ok(()) }
}
fn show_accepted(hr: HRESULT) -> Result<bool, String> {
    if hr == CANCELLED {
        return Ok(false);
    }
    check("IFileDialog::Show", hr)?;
    Ok(true)
}

fn path_from_native_units(units: &[u16]) -> Result<std::path::PathBuf, String> {
    if units.is_empty() || units.len() >= MAX_PATH_UNITS || units.contains(&0) {
        return Err("dialog returned an empty, invalid or overlong path".into());
    }
    let path = std::path::PathBuf::from(OsString::from_wide(units));
    if !path.is_absolute() || path.file_name().is_none() {
        return Err("dialog did not accept an absolute file destination".into());
    }
    Ok(path)
}
fn options(kind: FilePromptKind, previous: u32) -> u32 {
    let unsafe_options = 0x80 | 0x100 | 0x200 | 0x20 | 0x2000; // virtual, no-validation, multi, folders, create prompt
    let base = (previous & !unsafe_options)
        | FORCE_FILESYSTEM
        | PATH_MUST_EXIST
        | NO_CHANGE_DIR
        | NO_TEST_FILE_CREATE
        | DONT_ADD_TO_RECENT;
    match kind {
        FilePromptKind::Open => base | FILE_MUST_EXIST,
        FilePromptKind::SaveAs => (base & !FILE_MUST_EXIST) | OVERWRITE_PROMPT,
        _ => base,
    }
}

fn choose(request: &Request, pump: &Pump) -> Outcome {
    match choose_inner(request, pump) {
        Ok(outcome) => outcome,
        Err(error) => Outcome::Failed(error),
    }
}
fn choose_inner(request: &Request, pump: &Pump) -> Result<Outcome, String> {
    let owner = request.owner as HWND;
    if owner.is_null() || unsafe { IsWindow(owner) } == 0 {
        return Err("native owner window is unavailable".into());
    }
    let (class, iid) = match request.ticket.kind() {
        FilePromptKind::Open => (&CLSID_OPEN, &IID_OPEN),
        FilePromptKind::SaveAs => (&CLSID_SAVE, &IID_SAVE),
        _ => return Err("this prompt is not a native chooser".into()),
    };
    let mut raw = null_mut();
    let hr = unsafe { CoCreateInstance(class, null_mut(), CLSCTX_INPROC_SERVER, iid, &mut raw) };
    let dialog = ComPtr(raw); // release even if a broken provider returns a pointer on failure
    check("CoCreateInstance(common-item dialog)", hr)?;
    if dialog.0.is_null() {
        return Err("CoCreateInstance returned a null dialog".into());
    }
    let vtable = unsafe { dialog.dialog() };
    let mut flags = 0;
    check("IFileDialog::GetOptions", unsafe { (vtable.get_options)(dialog.0, &mut flags) })?;
    check("IFileDialog::SetOptions", unsafe { (vtable.set_options)(dialog.0, options(request.ticket.kind(), flags)) })?;
    if let Some(name) = &request.suggested_name {
        let mut wide: Vec<u16> = name.encode_wide().take(MAX_PATH_UNITS).collect();
        if wide.len() == MAX_PATH_UNITS || wide.contains(&0) {
            return Err("suggested file name is invalid or too long".into());
        }
        wide.push(0);
        check("IFileDialog::SetFileName", unsafe { (vtable.set_file_name)(dialog.0, wide.as_ptr()) })?;
    }
    if pump.shared.cancel.load(Ordering::Acquire) || pump.shared.shutdown.load(Ordering::Acquire) {
        return Ok(Outcome::Cancelled);
    }
    pump.active.set(dialog.0);
    let hr = unsafe { (vtable.show)(dialog.0, owner) };
    pump.active.set(null_mut());
    collect_after_show(&dialog, pump, hr)
}
fn collect_after_show(dialog: &ComPtr, pump: &Pump, hr: HRESULT) -> Result<Outcome, String> {
    if pump.shared.cancel.load(Ordering::Acquire) || pump.shared.shutdown.load(Ordering::Acquire) {
        return Ok(Outcome::Cancelled);
    }
    if !show_accepted(hr)? {
        return Ok(Outcome::Cancelled);
    }
    let vtable = unsafe { dialog.dialog() };
    // Only successful Show retrieves a result. Shell handles overwrite consent.
    let mut raw_item = null_mut();
    let hr = unsafe { (vtable.get_result)(dialog.0, &mut raw_item) };
    let item = ComPtr(raw_item);
    check("IFileDialog::GetResult", hr)?;
    if item.0.is_null() {
        return Err("dialog returned no Shell item".into());
    }
    let mut text = null_mut();
    let hr = unsafe { (item.item().get_display_name)(item.0, SIGDN_FILESYSPATH, &mut text) };
    let text = TaskString(text);
    check("IShellItem::GetDisplayName(SIGDN_FILESYSPATH)", hr)?;
    if text.0.is_null() {
        return Err("dialog returned no filesystem path".into());
    }
    let mut len = 0;
    // SDK guarantees an allocated, terminated LPWSTR; cap our copy/scan to the
    // Windows extended path limit. No String, canonicalize, stat or cwd lookup.
    while len < MAX_PATH_UNITS && unsafe { *text.0.add(len) } != 0 {
        len += 1;
    }
    if len == 0 || len == MAX_PATH_UNITS {
        return Err("dialog returned an empty or overlong path".into());
    }
    let path = path_from_native_units(unsafe { std::slice::from_raw_parts(text.0, len) })?;
    Ok(Outcome::Accepted(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn successful_result_collects_native_path_and_releases_both_interfaces() {
        #[repr(C)]
        struct Item {
            table: *const ShellItemVtable,
            names: u32,
            releases: u32,
        }
        #[repr(C)]
        struct Dialog {
            table: *const DialogVtable,
            item: *mut Item,
            results: u32,
            releases: u32,
        }
        const PATH: &[u16] = &[67, 58, 92, 0x65e5, 0x672c, 0xd800, 46, 116, 120, 116, 0];
        unsafe extern "system" fn item_release(raw: *mut c_void) -> u32 {
            unsafe {
                (*(raw as *mut Item)).releases += 1;
            }
            0
        }
        unsafe extern "system" fn dialog_release(raw: *mut c_void) -> u32 {
            unsafe {
                (*(raw as *mut Dialog)).releases += 1;
            }
            0
        }
        unsafe extern "system" fn result(raw: *mut c_void, out: *mut *mut c_void) -> HRESULT {
            unsafe {
                let dialog = &mut *(raw as *mut Dialog);
                dialog.results += 1;
                *out = dialog.item.cast();
            }
            0
        }
        unsafe extern "system" fn name(raw: *mut c_void, kind: i32, out: *mut *mut u16) -> HRESULT {
            if kind != SIGDN_FILESYSPATH {
                return 0x80070057u32 as i32;
            }
            let memory =
                unsafe { windows_sys::Win32::System::Com::CoTaskMemAlloc(std::mem::size_of_val(PATH)) }.cast::<u16>();
            if memory.is_null() {
                return 0x8007000eu32 as i32;
            }
            unsafe {
                (*(raw as *mut Item)).names += 1;
                std::ptr::copy_nonoverlapping(PATH.as_ptr(), memory, PATH.len());
                *out = memory;
            }
            0
        }
        unsafe extern "system" fn show(_: *mut c_void, _: HWND) -> HRESULT {
            -1
        }
        unsafe extern "system" fn flags(_: *mut c_void, _: u32) -> HRESULT {
            -1
        }
        unsafe extern "system" fn get_flags(_: *mut c_void, _: *mut u32) -> HRESULT {
            -1
        }
        unsafe extern "system" fn set_name(_: *mut c_void, _: *const u16) -> HRESULT {
            -1
        }
        unsafe extern "system" fn close(_: *mut c_void, _: HRESULT) -> HRESULT {
            -1
        }
        let item_table = ShellItemVtable {
            unknown: UnknownVtable { query: 0, add_ref: 0, release: item_release },
            unused3_4: [0; 2],
            get_display_name: name,
            unused6_7: [0; 2],
        };
        let dialog_table = DialogVtable {
            unknown: UnknownVtable { query: 0, add_ref: 0, release: dialog_release },
            show,
            unused4_8: [0; 5],
            set_options: flags,
            get_options: get_flags,
            unused11_14: [0; 4],
            set_file_name: set_name,
            unused16_19: [0; 4],
            get_result: result,
            unused21_22: [0; 2],
            close,
            unused24_26: [0; 3],
        };
        let mut item = Item { table: &item_table, names: 0, releases: 0 };
        let mut dialog = Dialog { table: &dialog_table, item: &mut item, results: 0, releases: 0 };
        let pointer = ComPtr((&mut dialog as *mut Dialog).cast());
        let pump =
            Pump { shared: Arc::new(Shared::default()), active: Cell::new(null_mut()), closing: Cell::new(false) };
        let outcome = collect_after_show(&pointer, &pump, 0).unwrap();
        let Outcome::Accepted(path) = outcome else { panic!("successful result must own its exact path") };
        assert_eq!(path.as_os_str().encode_wide().collect::<Vec<_>>(), &PATH[..PATH.len() - 1]);
        assert_eq!((dialog.results, item.names, item.releases), (1, 1, 1));
        drop(pointer);
        assert_eq!(dialog.releases, 1);
    }
    #[test]
    fn save_requires_shell_overwrite_and_disables_shell_test_writes() {
        let flags = options(FilePromptKind::SaveAs, 0);
        assert_ne!(flags & OVERWRITE_PROMPT, 0);
        assert_ne!(flags & NO_TEST_FILE_CREATE, 0);
        assert_ne!(flags & FORCE_FILESYSTEM, 0);
        assert_eq!(flags & (0x80 | 0x100 | 0x200 | 0x20 | 0x2000 | FILE_MUST_EXIST), 0);
        assert_eq!(
            options(FilePromptKind::SaveAs, u32::MAX) & (0x80 | 0x100 | 0x200 | 0x20 | 0x2000 | FILE_MUST_EXIST),
            0
        );
    }
    #[test]
    fn native_cancel_is_distinct_from_access_denied_and_open_requires_existing_file() {
        assert!(!show_accepted(CANCELLED).unwrap());
        assert!(show_accepted(0).unwrap());
        assert!(show_accepted(0x80070005u32 as i32).unwrap_err().contains("80070005"));
        assert_ne!(options(FilePromptKind::Open, 0) & FILE_MUST_EXIST, 0);
    }
    #[test]
    fn native_utf16_round_trips_unpaired_surrogate_without_replacement() {
        let units = [b'C' as u16, 58, 92, 0x65e5, 0x672c, 0xd800, 46, 116, 120, 116];
        let path = path_from_native_units(&units).unwrap();
        assert!(path.is_absolute());
        assert_eq!(path.as_os_str().encode_wide().collect::<Vec<_>>(), units);
    }
    #[test]
    fn mocked_abi_calls_select_correct_vtable_slots_and_release_once() {
        #[repr(C)]
        struct Mock {
            table: *const DialogVtable,
            calls: u32,
            releases: u32,
        }
        unsafe extern "system" fn release(p: *mut c_void) -> u32 {
            unsafe {
                (*(p as *mut Mock)).releases += 1;
            }
            0
        }
        unsafe extern "system" fn show(p: *mut c_void, owner: HWND) -> HRESULT {
            assert_eq!(owner as isize, 56);
            unsafe {
                (*(p as *mut Mock)).calls |= 1;
            }
            CANCELLED
        }
        unsafe extern "system" fn set(p: *mut c_void, flags: u32) -> HRESULT {
            assert_ne!(flags & OVERWRITE_PROMPT, 0);
            unsafe {
                (*(p as *mut Mock)).calls |= 2;
            }
            0
        }
        unsafe extern "system" fn get(p: *mut c_void, flags: *mut u32) -> HRESULT {
            unsafe {
                (*(p as *mut Mock)).calls |= 4;
                *flags = 0;
            }
            0
        }
        unsafe extern "system" fn name(_: *mut c_void, _: *const u16) -> HRESULT {
            0
        }
        unsafe extern "system" fn result(p: *mut c_void, _: *mut *mut c_void) -> HRESULT {
            unsafe {
                (*(p as *mut Mock)).calls |= 16;
            }
            -1
        }
        unsafe extern "system" fn close(p: *mut c_void, hr: HRESULT) -> HRESULT {
            assert_eq!(hr, CANCELLED);
            unsafe {
                (*(p as *mut Mock)).calls |= 8;
            }
            0
        }
        let table = DialogVtable {
            unknown: UnknownVtable { query: 0, add_ref: 0, release },
            show,
            unused4_8: [0; 5],
            set_options: set,
            get_options: get,
            unused11_14: [0; 4],
            set_file_name: name,
            unused16_19: [0; 4],
            get_result: result,
            unused21_22: [0; 2],
            close,
            unused24_26: [0; 3],
        };
        let mut mock = Mock { table: &table, calls: 0, releases: 0 };
        let pointer = ComPtr((&mut mock as *mut Mock).cast());
        unsafe {
            let table = pointer.dialog();
            let mut flags = 0;
            (table.get_options)(pointer.0, &mut flags);
            (table.set_options)(pointer.0, options(FilePromptKind::SaveAs, flags));
            assert_eq!((table.show)(pointer.0, 56isize as HWND), CANCELLED);
            let pump =
                Pump { shared: Arc::new(Shared::default()), active: Cell::new(null_mut()), closing: Cell::new(false) };
            assert!(matches!(collect_after_show(&pointer, &pump, CANCELLED), Ok(Outcome::Cancelled)));
            (table.close)(pointer.0, CANCELLED);
        }
        drop(pointer);
        assert_eq!(mock.calls, 15);
        assert_eq!(mock.releases, 1);
    }
}
