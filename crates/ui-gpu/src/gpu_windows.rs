//! DX12 damage presentation. Device creation and fence waits run off the UI.
//! The retained texture receives only damage and scroll copies; Present1 carries
//! the same rectangles to DWM. No timer, idle submission or GPU readback in release.
use super::Rect;
use std::{
    mem::ManuallyDrop,
    sync::{Arc, Mutex, mpsc},
    thread,
};
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, HWND, POINT, RECT},
        Graphics::{
            Direct3D::D3D_FEATURE_LEVEL_11_0,
            Direct3D12::*,
            Dxgi::{Common::*, *},
        },
        System::Threading::{CreateEventW, WaitForSingleObject},
    },
    core::Interface,
};
const MAX_PIXELS: usize = 16 * 1024 * 1024;
const FRAMES: usize = 3;
const WAIT_MS: u32 = 5000;
#[derive(Clone, Debug, PartialEq, Eq)]
struct ScrollCopy {
    source: Rect,
    destination: Rect,
    offset: (i32, i32),
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct CopyPlan {
    dirty: Vec<Rect>,
    scroll: Option<ScrollCopy>,
}
fn in_bounds(r: Rect, (w, h): (usize, usize)) -> bool {
    r.x <= w && r.y <= h && r.w <= w - r.x && r.h <= h - r.y
}
fn copy_plan(
    size: (usize, usize),
    full: bool,
    damage: &[Rect],
    scroll: i64,
    ch: usize,
    body: Rect,
) -> Result<CopyPlan, String> {
    let (w, h) = size;
    if w == 0
        || h == 0
        || w > i32::MAX as usize
        || h > i32::MAX as usize
        || w.checked_mul(h).is_none_or(|n| n > MAX_PIXELS)
    {
        return Err("DX12 viewport exceeds admission".into());
    }
    if full {
        return Ok(CopyPlan { dirty: vec![Rect { x: 0, y: 0, w, h }], scroll: None });
    }
    let mut dirty = Vec::with_capacity(damage.len().saturating_add(1));
    for &r in damage {
        if !in_bounds(r, size) {
            return Err("DX12 dirty rectangle outside viewport".into());
        }
        if r.w != 0 && r.h != 0 {
            dirty.push(r);
        }
    }
    let mut plan = CopyPlan { dirty, scroll: None };
    if scroll == 0 {
        return Ok(plan);
    }
    if ch == 0 || body.w == 0 || body.h == 0 || !in_bounds(body, size) {
        return Err("DX12 invalid scroll geometry".into());
    }
    let dy = usize::try_from(scroll.unsigned_abs()).ok().and_then(|n| n.checked_mul(ch));
    let Some(dy) = dy.filter(|dy| *dy < body.h) else {
        // No source pixels survive a large scroll. Repaint just the body.
        plan.dirty.push(body);
        return Ok(plan);
    };
    let (from, to, exposed) =
        if scroll > 0 { (body.y + dy, body.y, body.y + body.h - dy) } else { (body.y, body.y + dy, body.y) };
    plan.scroll = Some(ScrollCopy {
        source: Rect { x: body.x, y: from, w: body.w, h: body.h - dy },
        destination: Rect { x: body.x, y: to, w: body.w, h: body.h - dy },
        offset: (0, to as i32 - from as i32),
    });
    plan.dirty.push(Rect { x: body.x, y: exposed, w: body.w, h: dy });
    Ok(plan)
}
struct PendingPresent {
    plan: CopyPlan,
    buffer_index: usize,
    fence: u64,
    retried: bool,
}
impl PendingPresent {
    fn outcome(&mut self, result: windows::core::HRESULT) -> Result<bool, String> {
        if result == DXGI_ERROR_WAS_STILL_DRAWING {
            if self.retried {
                // A stale/readily-signalled latency handle must not create a
                // wake/retry busy loop. One readiness retry, then CPU fallback.
                return Err(
                    "DX12 Present1 still busy after GPU and frame-latency readiness; abandoning GPU presenter".into()
                );
            }
            self.retried = true;
            return Ok(false);
        }
        if result != windows::core::HRESULT(0) {
            return Err(format!("DX12 Present1 did not present: {result:?}"));
        }
        Ok(true)
    }
}
pub(super) enum Presented {
    Yes,
    Deferred,
    Failed(String),
}
struct Frame {
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    upload: Option<ID3D12Resource>,
    fence: u64,
}
struct Backend {
    device: ID3D12Device,
    queue: ID3D12CommandQueue,
    fence: ID3D12Fence,
    factory: IDXGIFactory2,
    swap: Option<IDXGISwapChain3>,
    buffers: Vec<ID3D12Resource>,
    backing: Option<ID3D12Resource>,
    scratch: Option<ID3D12Resource>,
    frames: Vec<Frame>,
    size: (usize, usize),
    serial: u64,
    full: bool,
    latency: Option<Arc<Event>>,
    pending: Option<PendingPresent>,
    presented: u64,
    // A failed queue Signal cannot retire accepted work using an older fence.
    unsignalled: bool,
}
enum Wait {
    Ready { value: u64, latency: Arc<Event>, _swap: IDXGISwapChain3 },
    Retire(Retirement),
}
// A disconnected/panicking worker must not drop an unread retirement message
// and thereby release allocations that the GPU still references.
struct Retirement(Option<Box<Backend>>);
impl Drop for Retirement {
    fn drop(&mut self) {
        if let Some(backend) = self.0.take() {
            eprintln!(
                "DX12 retirement unconfirmed; quarantining one bounded backend (3 frames, backing/scratch, swap chain and handles) until process exit"
            );
            std::mem::forget(backend);
        }
    }
}
pub(crate) struct Gpu {
    backend: Option<Box<Backend>>,
    wait: mpsc::Sender<Wait>,
    thread: Option<thread::JoinHandle<Result<(), String>>>,
    readiness: Arc<Mutex<Readiness>>,
}
// Immutable handles shared only for waits; the last owner closes the handle.
struct Event(HANDLE);
unsafe impl Send for Event {}
unsafe impl Sync for Event {}
impl Drop for Event {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
#[derive(Default, Debug)]
enum Readiness {
    #[default]
    Dormant,
    Waiting {
        notify: bool,
    },
    Ready,
    Failed(String),
}
impl Readiness {
    fn request(&mut self) -> Result<bool, String> {
        match self {
            Self::Ready => Ok(true),
            Self::Waiting { notify } => {
                *notify = true;
                Ok(false)
            }
            Self::Failed(error) => Err(error.clone()),
            Self::Dormant => Err("DX12 readiness was not armed".into()),
        }
    }
    fn complete(&mut self, result: &Result<(), String>) -> bool {
        if matches!(self, Self::Failed(_)) {
            return false;
        }
        let notify = matches!(self, Self::Waiting { notify: true }) || result.is_err();
        *self = match result {
            Ok(()) => Self::Ready,
            Err(error) => Self::Failed(error.clone()),
        };
        notify
    }
}
fn completed_value(fence: &ID3D12Fence) -> Result<u64, String> {
    let value = unsafe { fence.GetCompletedValue() };
    if value == u64::MAX { Err("DX12 device removed while checking fence".into()) } else { Ok(value) }
}
fn wait_outcome(stage: &str, status: u32) -> Result<(), String> {
    match status {
        0 => Ok(()),
        0x102 => Err(format!("DX12 {stage} timed out after {WAIT_MS}ms; terminal failure")),
        _ => Err(format!("DX12 {stage} failed: wait status={status:#x}")),
    }
}
fn wait_event(event: &Event, stage: &str) -> Result<(), String> {
    let status = unsafe { WaitForSingleObject(event.0, WAIT_MS).0 };
    if status == u32::MAX {
        return Err(format!("DX12 {stage} WAIT_FAILED: {}", std::io::Error::last_os_error()));
    }
    wait_outcome(stage, status)
}
fn wait_fence(fence: &ID3D12Fence, value: u64) -> Result<(), String> {
    unsafe {
        if completed_value(fence)? >= value {
            return Ok(());
        }
        let event = Event(CreateEventW(None, false, false, None).map_err(|e| e.to_string())?);
        fence.SetEventOnCompletion(value, event.0).map_err(|e| e.to_string())?;
        let result = wait_event(&event, "fence wait").and_then(|()| {
            completed_value(fence).and_then(|done| {
                if done >= value { Ok(()) } else { Err("DX12 fence signalled before completion".into()) }
            })
        });
        if result.is_err() && completed_value(fence).ok().is_none_or(|done| done < value) {
            // The driver may still signal this registered event. Retain both
            // the registration's handle and fence; never close/reuse it early.
            eprintln!("DX12 failed fence registration quarantined until process exit (one event and fence)");
            std::mem::forget((event, fence.clone()));
        }
        result
    }
}
impl Drop for Gpu {
    fn drop(&mut self) {
        if let Some(backend) = self.backend.take()
            && self.wait.send(Wait::Retire(Retirement(Some(backend)))).is_err()
        {
            eprintln!("DX12 retirement worker disconnected");
        }
    }
}
impl Gpu {
    pub fn adapter(&self) -> String {
        let Some(b) = &self.backend else { return "unavailable".into() };
        unsafe {
            b.factory
                .cast::<IDXGIFactory4>()
                .and_then(|factory| factory.EnumAdapterByLuid::<IDXGIAdapter1>(b.device.GetAdapterLuid()))
                .and_then(|adapter| adapter.GetDesc1())
                .map(|desc| {
                    let end = desc.Description.iter().position(|c| *c == 0).unwrap_or(desc.Description.len());
                    String::from_utf16_lossy(&desc.Description[..end])
                })
                .unwrap_or_else(|_| "DX12 device".into())
        }
    }

    pub fn init_with_wake(wake: impl Fn(Result<(), String>) + Send + 'static) -> Result<Self, String> {
        unsafe {
            let mut device = None;
            D3D12CreateDevice(None, D3D_FEATURE_LEVEL_11_0, &mut device).map_err(|e| e.to_string())?;
            let device: ID3D12Device = device.ok_or("DX12 returned no device")?;
            let queue: ID3D12CommandQueue = device
                .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                    Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                    ..Default::default()
                })
                .map_err(|e| e.to_string())?;
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).map_err(|e| e.to_string())?;
            let factory: IDXGIFactory2 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).map_err(|e| e.to_string())?;
            let mut frames = Vec::with_capacity(FRAMES);
            for _ in 0..FRAMES {
                let allocator: ID3D12CommandAllocator =
                    device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).map_err(|e| e.to_string())?;
                let list: ID3D12GraphicsCommandList = device
                    .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
                    .map_err(|e| e.to_string())?;
                list.Close().map_err(|e| e.to_string())?;
                frames.push(Frame { allocator, list, upload: None, fence: 0 });
            }
            let (tx, rx) = mpsc::channel();
            let waiter = fence.clone();
            let readiness = Arc::new(Mutex::new(Readiness::Dormant));
            let worker_state = readiness.clone();
            let thread = thread::Builder::new()
                .name("kwil-dx12-readiness".into())
                .spawn(move || {
                    let mut failure: Option<String> = None;
                    while let Ok(message) = rx.recv() {
                        match message {
                            Wait::Ready { value, latency, _swap } => {
                                let result = if let Some(error) = &failure {
                                    Err(error.clone())
                                } else {
                                    wait_fence(&waiter, value).and_then(|()| wait_event(&latency, "frame-latency wait"))
                                };
                                if let Err(error) = &result {
                                    failure = Some(error.clone());
                                }
                                let notify = worker_state.lock().unwrap_or_else(|e| e.into_inner()).complete(&result);
                                // No wake at idle: cache the next frame's credit.
                                // Failure always wakes ROOT with the actual error.
                                if notify {
                                    wake(result);
                                }
                            }
                            Wait::Retire(mut retirement) => {
                                let backend = retirement.0.as_ref().unwrap();
                                let retired = !backend.unsignalled
                                    && completed_value(&waiter).is_ok_and(|done| done >= backend.serial);
                                let result = if retired {
                                    Ok(())
                                } else if let Some(error) = &failure {
                                    Err(error.clone())
                                } else if backend.unsignalled {
                                    Err("DX12 accepted work has no retirement fence".into())
                                } else {
                                    wait_fence(&waiter, backend.serial)
                                };
                                if let Err(error) = result {
                                    eprintln!("{error}; DX12 retirement failed; no retry");
                                    return Err(error);
                                }
                                // Only this confirmed path disarms quarantine.
                                drop(retirement.0.take());
                                return failure.map_or(Ok(()), Err);
                            }
                        }
                    }
                    failure.map_or(Ok(()), Err)
                })
                .map_err(|e| e.to_string())?;
            Ok(Self {
                backend: Some(Box::new(Backend {
                    device,
                    queue,
                    fence,
                    factory,
                    swap: None,
                    buffers: Vec::new(),
                    backing: None,
                    scratch: None,
                    frames,
                    size: (0, 0),
                    serial: 0,
                    full: true,
                    latency: None,
                    pending: None,
                    presented: 0,
                    unsignalled: false,
                })),
                wait: tx,
                thread: Some(thread),
                readiness,
            })
        }
    }
    pub fn shutdown(mut self) -> Result<(), String> {
        let thread = self.thread.take();
        drop(self);
        thread.map_or(Ok(()), |t| t.join().map_err(|_| "DX12 waiter panicked".to_string()).and_then(|result| result))
    }
    pub fn invalidate(&mut self) {
        if let Some(b) = &mut self.backend {
            b.full = true;
        }
    }
    /// ROOT forwards GpuAvailable's payload. Success does not manufacture a
    /// credit: the worker published it before waking and errors stay terminal.
    #[cfg(feature = "frame-harness")]
    pub fn inject_retirement_timeout(&mut self) {
        if let Some(backend) = &mut self.backend {
            backend.unsignalled = true;
        }
        self.available(Err("Injected retirement timeout".into()));
    }
    pub fn available(&mut self, result: Result<(), String>) {
        if let Err(error) = result {
            *self.readiness.lock().unwrap_or_else(|e| e.into_inner()) = Readiness::Failed(error);
        }
    }
    fn arm(&self, notify: bool) -> Result<(), String> {
        let b = self.backend.as_ref().ok_or("DX12 backend unavailable")?;
        let mut state = self.readiness.lock().unwrap_or_else(|e| e.into_inner());
        if let Readiness::Failed(error) = &*state {
            return Err(error.clone());
        }
        // At most one readiness command is outstanding; Retire is the only
        // other command. The unbounded channel cannot accumulate frames.
        if matches!(*state, Readiness::Waiting { .. }) {
            return Err("DX12 readiness already armed".into());
        }
        *state = Readiness::Waiting { notify };
        let value = b.pending.as_ref().map_or(b.serial, |p| p.fence);
        let message = Wait::Ready {
            value,
            latency: b.latency.as_ref().ok_or("DX12 latency handle unavailable")?.clone(),
            _swap: b.swap.as_ref().ok_or("DX12 swap chain unavailable")?.clone(),
        };
        if self.wait.send(message).is_err() {
            let error = "DX12 readiness worker disconnected".to_string();
            *state = Readiness::Failed(error.clone());
            return Err(error);
        }
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    pub fn present(
        &mut self,
        hwnd: isize,
        pixels: &[u32],
        size: (usize, usize),
        damage: &[Rect],
        scroll: i64,
        ch: usize,
        body: Rect,
    ) -> Presented {
        if size.0 == 0 || size.1 == 0 || size.0.checked_mul(size.1).is_none_or(|n| n > MAX_PIXELS || n != pixels.len())
        {
            return Presented::Failed("DX12 viewport exceeds admission".into());
        }
        let result = (|| -> Result<Presented, String> {
            if let Readiness::Failed(error) = &*self.readiness.lock().unwrap_or_else(|e| e.into_inner()) {
                return Err(error.clone());
            }
            let b = self.backend.as_mut().ok_or("DX12 backend unavailable")?;
            if b.unsignalled {
                return Err("DX12 work has no retirement fence".into());
            }
            // First creation has no prior GPU work and needs its first latency
            // credit. Resize waits for ALL accepted work, not just an allocator.
            if b.size != size && b.pending.is_none() {
                let existing = b.swap.is_some();
                if existing && !self.readiness.lock().unwrap_or_else(|e| e.into_inner()).request()? {
                    b.full = true;
                    return Ok(Presented::Deferred);
                }
                if b.serial > completed_value(&b.fence)? {
                    return Err("DX12 resize readiness did not retire accepted GPU work".into());
                }
                unsafe {
                    b.resize(hwnd, size)?;
                }
                if existing {
                    // The waiter already consumed one latency credit before
                    // ResizeBuffers. Resize does not present a frame and must
                    // not consume that same credit a second time.
                    *self.readiness.lock().unwrap_or_else(|e| e.into_inner()) = Readiness::Ready;
                } else {
                    *self.readiness.lock().unwrap_or_else(|e| e.into_inner()) = Readiness::Dormant;
                    self.arm(true)?;
                    return Ok(Presented::Deferred);
                }
            }
            let ready = self.readiness.lock().unwrap_or_else(|e| e.into_inner()).request()?;
            if !ready {
                self.backend.as_mut().unwrap().full = true;
                return Ok(Presented::Deferred);
            }
            *self.readiness.lock().unwrap_or_else(|e| e.into_inner()) = Readiness::Dormant;
            let b = self.backend.as_mut().unwrap();
            if b.pending.is_some() {
                // Retry the exact submitted frame before accepting new pixels,
                // resize, scroll or invalidation. ROOT retains/coalesces those.
                let _ = unsafe { b.present_pending()? };
                b.full = true;
                self.arm(true)?;
                return Ok(Presented::Deferred);
            }
            let completed = completed_value(&b.fence)?;
            if !b.frames.iter().any(|f| f.fence <= completed) {
                return Err("DX12 readiness did not retire an allocator".into());
            }
            let plan = copy_plan(size, b.full, damage, scroll, ch, body)?;
            if plan.dirty.is_empty() && plan.scroll.is_none() {
                // DirtyRectsCount=0 means full frame, NOT no damage.
                *self.readiness.lock().unwrap_or_else(|e| e.into_inner()) = Readiness::Ready;
                return Ok(Presented::Yes);
            }
            unsafe {
                b.draw(pixels, plan)?;
            }
            let presented = unsafe { b.present_pending()? };
            if !presented {
                b.full = true;
            }
            self.arm(!presented)?;
            Ok(if presented { Presented::Yes } else { Presented::Deferred })
        })();
        match result {
            Ok(result) => result,
            Err(error) => {
                *self.readiness.lock().unwrap_or_else(|e| e.into_inner()) = Readiness::Failed(error.clone());
                Presented::Failed(error)
            }
        }
    }
}

fn texture_desc(size: (usize, usize)) -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Width: size.0 as u64,
        Height: size.1 as u32,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        ..Default::default()
    }
}
fn resource(
    device: &ID3D12Device,
    desc: &D3D12_RESOURCE_DESC,
    heap: D3D12_HEAP_TYPE,
    state: D3D12_RESOURCE_STATES,
) -> Result<ID3D12Resource, String> {
    let mut result = None;
    unsafe {
        device.CreateCommittedResource(
            &D3D12_HEAP_PROPERTIES { Type: heap, ..Default::default() },
            D3D12_HEAP_FLAG_NONE,
            desc,
            state,
            None,
            &mut result,
        )
    }
    .map_err(|e| e.to_string())?;
    result.ok_or("DX12 returned no resource".into())
}
fn location(resource: &ID3D12Resource) -> D3D12_TEXTURE_COPY_LOCATION {
    D3D12_TEXTURE_COPY_LOCATION {
        pResource: ManuallyDrop::new(Some(resource.clone())),
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
    }
}
unsafe fn barrier(
    list: &ID3D12GraphicsCommandList,
    resource: &ID3D12Resource,
    from: D3D12_RESOURCE_STATES,
    to: D3D12_RESOURCE_STATES,
) {
    let mut barrier = D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                pResource: ManuallyDrop::new(Some(resource.clone())),
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: from,
                StateAfter: to,
            }),
        },
        ..Default::default()
    };
    unsafe {
        list.ResourceBarrier(std::slice::from_ref(&barrier));
        ManuallyDrop::drop(&mut (*barrier.Anonymous.Transition).pResource);
    }
}
unsafe fn copy_region(
    list: &ID3D12GraphicsCommandList,
    source: &ID3D12Resource,
    from: Rect,
    target: &ID3D12Resource,
    to: (usize, usize),
) {
    let mut src = location(source);
    let mut dst = location(target);
    unsafe {
        list.CopyTextureRegion(
            &dst,
            to.0 as u32,
            to.1 as u32,
            0,
            &src,
            Some(&D3D12_BOX {
                left: from.x as u32,
                top: from.y as u32,
                front: 0,
                right: (from.x + from.w) as u32,
                bottom: (from.y + from.h) as u32,
                back: 1,
            }),
        );
        ManuallyDrop::drop(&mut src.pResource);
        ManuallyDrop::drop(&mut dst.pResource);
    }
}
impl Backend {
    unsafe fn resize(&mut self, hwnd: isize, size: (usize, usize)) -> Result<(), String> {
        unsafe {
            if self.pending.is_some() || self.unsignalled || completed_value(&self.fence)? < self.serial {
                return Err("DX12 resize attempted before retirement".into());
            }
            self.buffers.clear();
            if let Some(swap) = &self.swap {
                swap.ResizeBuffers(
                    FRAMES as u32,
                    size.0 as u32,
                    size.1 as u32,
                    DXGI_FORMAT_B8G8R8A8_UNORM,
                    DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT,
                )
                .map_err(|e| e.to_string())?;
            } else {
                let desc = DXGI_SWAP_CHAIN_DESC1 {
                    Width: size.0 as u32,
                    Height: size.1 as u32,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                    BufferCount: FRAMES as u32,
                    SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
                    Scaling: DXGI_SCALING_STRETCH,
                    AlphaMode: DXGI_ALPHA_MODE_IGNORE,
                    Flags: DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32,
                    ..Default::default()
                };
                let swap = self
                    .factory
                    .CreateSwapChainForHwnd(&self.queue, HWND(hwnd as _), &desc, None, None)
                    .map_err(|e| e.to_string())?;
                let swap: IDXGISwapChain3 = swap.cast().map_err(|e| e.to_string())?;
                swap.SetMaximumFrameLatency(1).map_err(|e| e.to_string())?;
                let handle = swap.GetFrameLatencyWaitableObject();
                if handle.is_invalid() {
                    return Err("DX12 swap chain returned no latency handle".into());
                }
                self.latency = Some(Arc::new(Event(handle)));
                self.swap = Some(swap);
                let _ = self.factory.MakeWindowAssociation(HWND(hwnd as _), DXGI_MWA_NO_ALT_ENTER);
            }
            for index in 0..FRAMES {
                self.buffers.push(self.swap.as_ref().unwrap().GetBuffer(index as u32).map_err(|e| e.to_string())?);
            }
            let desc = texture_desc(size);
            self.backing =
                Some(resource(&self.device, &desc, D3D12_HEAP_TYPE_DEFAULT, D3D12_RESOURCE_STATE_COPY_DEST)?);
            self.scratch =
                Some(resource(&self.device, &desc, D3D12_HEAP_TYPE_DEFAULT, D3D12_RESOURCE_STATE_COPY_DEST)?);
            let pitch = (size.0 * 4).div_ceil(256) * 256;
            let upload = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: (pitch * size.1) as u64,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                ..Default::default()
            };
            for frame in &mut self.frames {
                frame.upload =
                    Some(resource(&self.device, &upload, D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ)?);
            }
            self.size = size;
            self.full = true;
            Ok(())
        }
    }
    unsafe fn draw(&mut self, pixels: &[u32], plan: CopyPlan) -> Result<(), String> {
        unsafe {
            if self.pending.is_some() {
                return Err("DX12 attempted to overwrite a pending present".into());
            }
            let completed = completed_value(&self.fence)?;
            let serial = self.serial.checked_add(1).filter(|v| *v != u64::MAX).ok_or("DX12 serial exhausted")?;
            let frame = self.frames.iter_mut().find(|f| f.fence <= completed).ok_or("DX12 has no free frame")?;
            frame.allocator.Reset().map_err(|e| e.to_string())?;
            frame.list.Reset(&frame.allocator, None).map_err(|e| e.to_string())?;
            let list = &frame.list;
            let backing = self.backing.as_ref().unwrap();
            let scratch = self.scratch.as_ref().unwrap();
            let (w, h) = self.size;
            if let Some(scroll) = &plan.scroll {
                // Scroll only the private coherent image. DXGI owns copying
                // previous-frame scroll pixels into the swap-chain destination.
                // Dirty uploads follow this copy and win in overlapping areas.
                barrier(list, backing, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COPY_SOURCE);
                copy_region(list, backing, scroll.source, scratch, (0, 0));
                barrier(list, backing, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_COPY_DEST);
                barrier(list, scratch, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COPY_SOURCE);
                copy_region(
                    list,
                    scratch,
                    Rect { x: 0, y: 0, w: scroll.source.w, h: scroll.source.h },
                    backing,
                    (scroll.destination.x, scroll.destination.y),
                );
                barrier(list, scratch, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_COPY_DEST);
            }
            let rects = &plan.dirty;
            let upload = frame.upload.as_ref().unwrap();
            let pitch = (w * 4).div_ceil(256) * 256;
            let mut mapped = std::ptr::null_mut();
            upload.Map(0, Some(&D3D12_RANGE { Begin: 0, End: 0 }), Some(&mut mapped)).map_err(|e| e.to_string())?;
            for r in rects {
                for y in r.y..r.y + r.h {
                    let dst =
                        std::slice::from_raw_parts_mut(mapped.cast::<u8>().add(y * pitch + r.x * 4).cast::<u32>(), r.w);
                    for (d, s) in dst.iter_mut().zip(&pixels[y * w + r.x..y * w + r.x + r.w]) {
                        *d = *s | 0xff000000;
                    }
                }
            }
            upload.Unmap(0, None);
            let mut src = D3D12_TEXTURE_COPY_LOCATION {
                pResource: ManuallyDrop::new(Some(upload.clone())),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                        Offset: 0,
                        Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                            Width: w as u32,
                            Height: h as u32,
                            Depth: 1,
                            RowPitch: pitch as u32,
                        },
                    },
                },
            };
            let mut dst = location(backing);
            for r in rects {
                list.CopyTextureRegion(
                    &dst,
                    r.x as u32,
                    r.y as u32,
                    0,
                    &src,
                    Some(&D3D12_BOX {
                        left: r.x as u32,
                        top: r.y as u32,
                        front: 0,
                        right: (r.x + r.w) as u32,
                        bottom: (r.y + r.h) as u32,
                        back: 1,
                    }),
                );
            }
            ManuallyDrop::drop(&mut src.pResource);
            ManuallyDrop::drop(&mut dst.pResource);
            let swap = self.swap.as_ref().unwrap();
            let buffer_index = swap.GetCurrentBackBufferIndex() as usize;
            let target = self.buffers.get(buffer_index).ok_or("DX12 backbuffer index outside swap chain")?;
            barrier(list, backing, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COPY_SOURCE);
            barrier(list, target, D3D12_RESOURCE_STATE_PRESENT, D3D12_RESOURCE_STATE_COPY_DEST);
            // Present1 forbids any writes outside its dirty rectangles. Fully
            // replace each dirty pixel from the final persistent image, so even
            // intersections with ALL older dirty regions of this rotating
            // buffer are coherent; no per-buffer repair writes are needed.
            // DXGI reconstructs unchanged/scroll pixels outside these regions.
            // https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_2/nf-dxgi1_2-idxgiswapchain1-present1
            for &r in rects {
                copy_region(list, backing, r, target, (r.x, r.y));
            }
            barrier(list, target, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_PRESENT);
            barrier(list, backing, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_COPY_DEST);
            list.Close().map_err(|e| e.to_string())?;
            let command: ID3D12CommandList = list.cast().map_err(|e| e.to_string())?;
            // Record ownership BEFORE submission. Signal failure must quarantine
            // every accepted allocation, including the upload and command list.
            self.serial = serial;
            frame.fence = serial;
            self.unsignalled = true;
            self.queue.ExecuteCommandLists(&[Some(command)]);
            self.queue.Signal(&self.fence, serial).map_err(|e| e.to_string())?;
            self.unsignalled = false;
            self.pending = Some(PendingPresent { plan, buffer_index, fence: serial, retried: false });
            self.full = false;
            Ok(())
        }
    }
    unsafe fn present_pending(&mut self) -> Result<bool, String> {
        let pending = self.pending.as_mut().ok_or("DX12 has no pending present")?;
        let swap = self.swap.as_ref().ok_or("DX12 has no swap chain")?;
        if unsafe { swap.GetCurrentBackBufferIndex() as usize } != pending.buffer_index {
            return Err("DX12 pending backbuffer changed before successful Present1".into());
        }
        let mut dirty: Vec<RECT> = pending
            .plan
            .dirty
            .iter()
            .map(|r| RECT { left: r.x as i32, top: r.y as i32, right: (r.x + r.w) as i32, bottom: (r.y + r.h) as i32 })
            .collect();
        let mut scroll_rect = pending.plan.scroll.as_ref().map(|s| RECT {
            left: s.destination.x as i32,
            top: s.destination.y as i32,
            right: (s.destination.x + s.destination.w) as i32,
            bottom: (s.destination.y + s.destination.h) as i32,
        });
        let mut scroll_offset = pending.plan.scroll.as_ref().map(|s| POINT { x: s.offset.0, y: s.offset.1 });
        let result = unsafe {
            swap.Present1(
                0,
                DXGI_PRESENT_DO_NOT_WAIT,
                &DXGI_PRESENT_PARAMETERS {
                    DirtyRectsCount: dirty.len() as u32,
                    pDirtyRects: dirty.as_mut_ptr(),
                    pScrollRect: scroll_rect.as_mut().map_or(std::ptr::null_mut(), |v| v),
                    pScrollOffset: scroll_offset.as_mut().map_or(std::ptr::null_mut(), |v| v),
                },
            )
        };
        if !pending.outcome(result)? {
            return Ok(false);
        }
        self.presented = self.presented.saturating_add(1);
        self.pending = None;
        Ok(true)
    }
}

#[cfg(feature = "frame-harness")]
impl Gpu {
    pub fn presents(&self) -> u64 {
        self.backend.as_ref().map_or(0, |b| b.presented)
    }
    /// Dev-only actual DX12 readback. The shipping presenter never waits/maps.
    pub fn matches(&self, pixels: &[u32]) -> Result<bool, String> {
        let actual = self.read_pixels()?;
        Ok(actual.len() == pixels.len() && actual.iter().zip(pixels).all(|(a, b)| *a == (*b | 0xff000000)))
    }
    pub fn read_pixels(&self) -> Result<Vec<u32>, String> {
        unsafe {
            if let Readiness::Failed(error) = &*self.readiness.lock().unwrap_or_else(|e| e.into_inner()) {
                return Err(error.clone());
            }
            let b = self.backend.as_ref().ok_or("no DX12 backend")?;
            let (w, h) = b.size;
            if w * h == 0 {
                return Err("no admitted DX12 image".into());
            }
            let pitch = (w * 4).div_ceil(256) * 256;
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: (pitch * h) as u64,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                ..Default::default()
            };
            let readback = resource(&b.device, &desc, D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_STATE_COPY_DEST)?;
            let allocator: ID3D12CommandAllocator =
                b.device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).map_err(|e| e.to_string())?;
            let list: ID3D12GraphicsCommandList = b
                .device
                .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
                .map_err(|e| e.to_string())?;
            let backing = b.backing.as_ref().unwrap();
            barrier(&list, backing, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COPY_SOURCE);
            let mut src = location(backing);
            let mut dst = D3D12_TEXTURE_COPY_LOCATION {
                pResource: ManuallyDrop::new(Some(readback.clone())),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                        Offset: 0,
                        Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                            Width: w as u32,
                            Height: h as u32,
                            Depth: 1,
                            RowPitch: pitch as u32,
                        },
                    },
                },
            };
            list.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
            ManuallyDrop::drop(&mut src.pResource);
            ManuallyDrop::drop(&mut dst.pResource);
            barrier(&list, backing, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_COPY_DEST);
            list.Close().map_err(|e| e.to_string())?;
            let fence: ID3D12Fence = b.device.CreateFence(0, D3D12_FENCE_FLAG_NONE).map_err(|e| e.to_string())?;
            b.queue.ExecuteCommandLists(&[Some(list.cast().map_err(|e| e.to_string())?)]);
            let retired = b.queue.Signal(&fence, 1).map_err(|e| e.to_string()).and_then(|()| wait_fence(&fence, 1));
            if let Err(error) = retired {
                *self.readiness.lock().unwrap_or_else(|e| e.into_inner()) = Readiness::Failed(error.clone());
                eprintln!(
                    "{error}; quarantining one harness readback, command allocation, backing and device until process exit; no further GPU work accepted"
                );
                std::mem::forget((
                    readback,
                    allocator,
                    list,
                    fence,
                    backing.clone(),
                    b.queue.clone(),
                    b.device.clone(),
                ));
                return Err(error);
            }
            let mut mapped = std::ptr::null_mut();
            readback.Map(0, None, Some(&mut mapped)).map_err(|e| e.to_string())?;
            let mut actual = Vec::with_capacity(w * h);
            for y in 0..h {
                actual
                    .extend_from_slice(std::slice::from_raw_parts(mapped.cast::<u8>().add(y * pitch).cast::<u32>(), w));
            }
            readback.Unmap(0, Some(&D3D12_RANGE { Begin: 0, End: 0 }));
            Ok(actual)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn rect(x: usize, y: usize, w: usize, h: usize) -> Rect {
        Rect { x, y, w, h }
    }
    // Model only the app's permitted writes. DXGI's reconstruction is separate;
    // these tests cannot establish actual DWM or remote compositor behaviour.
    fn dirty_copy(plan: &CopyPlan, final_image: &[u32], target: &mut [u32], width: usize) {
        for r in &plan.dirty {
            for y in r.y..r.y + r.h {
                let span = y * width + r.x..y * width + r.x + r.w;
                target[span.clone()].copy_from_slice(&final_image[span]);
            }
        }
    }
    #[test]
    fn partial_copy_preserves_outside_dirty_across_three_rotating_buffers() {
        let size = (8, 6);
        let mut final_image: Vec<u32> = (0..48).collect();
        let mut buffers = [vec![999; 48], vec![888; 48], vec![777; 48]];
        for index in 0..9 {
            let damage = rect(1 + index % 3, 1 + index % 2, 3, 2);
            let plan = copy_plan(size, false, &[damage], 0, 1, rect(0, 0, 8, 6)).unwrap();
            assert_eq!(plan.dirty, vec![damage]);
            for y in damage.y..damage.y + damage.h {
                for x in damage.x..damage.x + damage.w {
                    final_image[y * 8 + x] = 100 + index as u32;
                }
            }
            let target = &mut buffers[index % FRAMES];
            let old = target.clone();
            dirty_copy(&plan, &final_image, target, 8);
            for y in 0..6 {
                for x in 0..8 {
                    let p = y * 8 + x;
                    assert_eq!(target[p], if damage.contains((x, y)) { final_image[p] } else { old[p] });
                }
            }
        }
    }
    #[test]
    fn fresh_full_and_empty_damage_have_distinct_plans() {
        let body = rect(0, 1, 8, 4);
        let fresh = copy_plan((8, 6), true, &[rect(2, 2, 1, 1)], 1, 1, body).unwrap();
        assert_eq!(fresh.dirty, vec![rect(0, 0, 8, 6)]);
        assert!(fresh.scroll.is_none());
        let partial = copy_plan((8, 6), false, &[rect(2, 2, 1, 1)], 0, 1, body).unwrap();
        assert_eq!(partial.dirty, vec![rect(2, 2, 1, 1)]);
        let empty = copy_plan((8, 6), false, &[], 0, 1, body).unwrap();
        assert!(empty.dirty.is_empty() && empty.scroll.is_none());
    }
    #[test]
    fn scroll_plan_owns_destination_exposure_and_dirty_overlap() {
        let size = (8, 8);
        let body = rect(1, 1, 6, 6);
        for sign in [-1, 1] {
            let overlay = rect(3, 3, 2, 2);
            let plan = copy_plan(size, false, &[overlay], sign, 2, body).unwrap();
            let scroll = plan.scroll.as_ref().unwrap();
            assert_eq!(scroll.offset, (0, -2 * sign as i32));
            assert_eq!(scroll.source, rect(1, if sign > 0 { 3 } else { 1 }, 6, 4));
            assert_eq!(scroll.destination, rect(1, if sign > 0 { 1 } else { 3 }, 6, 4));
            assert_eq!(plan.dirty, vec![overlay, rect(1, if sign > 0 { 5 } else { 1 }, 6, 2)]);
            let previous: Vec<u32> = (0..64).collect();
            let mut backing = previous.clone();
            for y in 0..scroll.source.h {
                for x in 0..scroll.source.w {
                    backing[(scroll.destination.y + y) * 8 + scroll.destination.x + x] =
                        previous[(scroll.source.y + y) * 8 + scroll.source.x + x];
                }
            }
            let mut cpu_final = backing.clone();
            for y in overlay.y..overlay.y + overlay.h {
                for x in overlay.x..overlay.x + overlay.w {
                    cpu_final[y * 8 + x] = 900;
                }
            }
            let exposed = plan.dirty[1];
            for y in exposed.y..exposed.y + exposed.h {
                for x in exposed.x..exposed.x + exposed.w {
                    cpu_final[y * 8 + x] = 800;
                }
            }
            dirty_copy(&plan, &cpu_final, &mut backing, 8);
            assert_eq!(backing, cpu_final);
            let mut stale_buffer = vec![999; 64];
            dirty_copy(&plan, &backing, &mut stale_buffer, 8);
            // The runtime owns reconstruction outside app dirty writes; dirty
            // overlays win even where they intersect the scroll destination.
            let mut composed = previous.clone();
            for y in 0..scroll.source.h {
                for x in 0..scroll.source.w {
                    composed[(scroll.destination.y + y) * 8 + scroll.destination.x + x] =
                        previous[(scroll.source.y + y) * 8 + scroll.source.x + x];
                }
            }
            dirty_copy(&plan, &stale_buffer, &mut composed, 8);
            assert_eq!(composed, cpu_final);
        }
    }
    #[test]
    fn scroll_range_and_overflow_repaint_body_or_reject_without_wrapping() {
        let body = rect(1, 1, 6, 6);
        for scroll in [3, -3, i64::MAX, i64::MIN] {
            let plan = copy_plan((8, 8), false, &[], scroll, 2, body).unwrap();
            assert_eq!(plan.dirty, vec![body]);
            assert!(plan.scroll.is_none());
        }
        assert!(copy_plan((8, 8), false, &[rect(usize::MAX, 0, 2, 1)], 0, 1, body).is_err());
        assert!(copy_plan((8, 8), false, &[], 1, 1, rect(1, usize::MAX, 6, 6)).is_err());
        assert!(copy_plan((8, 8), false, &[], 1, 0, body).is_err());
        assert!(copy_plan((usize::MAX, 8), true, &[], 0, 1, body).is_err());
    }
    #[test]
    fn pending_present_keeps_exact_dirty_scroll_buffer_and_fence_until_success() {
        let plan = copy_plan((8, 8), false, &[rect(2, 2, 2, 2)], -1, 1, rect(1, 1, 6, 6)).unwrap();
        let mut pending = PendingPresent { plan: plan.clone(), buffer_index: 2, fence: 42, retried: false };
        assert!(!pending.outcome(DXGI_ERROR_WAS_STILL_DRAWING).unwrap());
        assert_eq!((&pending.plan, pending.buffer_index, pending.fence), (&plan, 2, 42));
        assert!(pending.outcome(windows::core::HRESULT(0)).unwrap());
        assert_eq!((&pending.plan, pending.buffer_index, pending.fence), (&plan, 2, 42));
    }
    #[test]
    fn pending_present_busy_retry_and_occlusion_are_terminal_not_idle_loops() {
        let plan = copy_plan((2, 2), true, &[], 0, 1, rect(0, 0, 2, 2)).unwrap();
        let mut pending = PendingPresent { plan, buffer_index: 0, fence: 1, retried: false };
        assert!(!pending.outcome(DXGI_ERROR_WAS_STILL_DRAWING).unwrap());
        assert!(pending.outcome(DXGI_ERROR_WAS_STILL_DRAWING).is_err());
        assert!(pending.outcome(windows::core::HRESULT(0x087A0001)).is_err());
        assert!(pending.outcome(DXGI_ERROR_DEVICE_REMOVED).is_err());
    }
    #[test]
    fn readiness_caches_idle_success_and_coalesces_demand_but_failure_wakes_once() {
        let mut state = Readiness::Waiting { notify: false };
        assert!(!state.complete(&Ok(()))); // Idle success must not cause redraw.
        assert!(state.request().unwrap());
        state = Readiness::Waiting { notify: false };
        assert!(!state.request().unwrap());
        assert!(!state.request().unwrap()); // Many CPU frames, one wake.
        assert!(state.complete(&Ok(())));
        assert!(state.request().unwrap());
        state = Readiness::Waiting { notify: false };
        assert!(state.complete(&Err("injected WAIT_FAILED".into())));
        assert_eq!(state.request().unwrap_err(), "injected WAIT_FAILED");
        assert!(!state.complete(&Ok(()))); // Late success cannot erase failure.
        assert_eq!(state.request().unwrap_err(), "injected WAIT_FAILED");
        assert!(!state.complete(&Err("second failure".into())));
    }
    #[test]
    fn timeout_and_failed_wait_are_errors_never_available_success() {
        assert!(wait_outcome("fence", 0).is_ok());
        for status in [0x102, u32::MAX, 0x80] {
            let result = wait_outcome("injected readiness", status);
            assert!(result.is_err());
            let mut state = Readiness::Waiting { notify: false };
            assert!(state.complete(&result));
            assert!(state.request().is_err());
            assert!(!state.complete(&Ok(())));
            assert!(state.request().is_err());
        }
    }
}
