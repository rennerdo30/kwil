//! Native Metal/Vulkan/GLES presentation with a damage-preserving backing store.
//! wgpu 30 has no public partial-present metadata: uploads and scroll copies are
//! damage-only, but each changed frame blits the whole backing to the swapchain.
//! The host owns the CPU first frame, remote-mode CPU path and all redraw wakes.
//! Only attach_window creates the native surface on the UI (required by Metal).
//! Every configure/acquire/upload/blit/present/readback/drop runs off the UI.
use super::Rect;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Presented {
    /// Accepted asynchronously (or an empty, already-current frame). Clear only
    /// the submitted host damage; completion arrives through init_with_wake.
    Yes,
    /// Nothing was accepted. Retain/coalesce full pending host damage and retry
    /// once on completion wake (if busy), or a real resize event (if zero-size).
    /// A completion without pending damage must NOT request another redraw.
    Deferred,
    /// Drop this GPU and continue on CPU; do not repeatedly reinitialize it.
    Failed(String),
}

const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(5);
const MAX_DAMAGE_RECTS: usize = 256;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ShutdownStatus {
    Stopped,
    /// A public driver call did not return. The detached renderer still owns
    /// GPU/surface/window/pixels until it returns or the process exits.
    Detached(String),
}

pub(crate) struct Shutdown {
    result: mpsc::Receiver<ShutdownStatus>,
}
impl Shutdown {
    /// Off-UI shutdown aggregation only; driver resources remain worker-owned.
    pub fn wait(self) -> Result<(), String> {
        match self.result.recv_timeout(Duration::from_secs(6)) {
            Ok(ShutdownStatus::Stopped) => Ok(()),
            Ok(ShutdownStatus::Detached(error)) => Err(error),
            Err(error) => Err(format!("GPU retirement status unavailable: {error}")),
        }
    }
}

struct Shared {
    busy: AtomicBool,
    stopping: AtomicBool,
    failure: Arc<Mutex<Option<String>>>,
    owner: Ownership,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OwnerState {
    Active,
    Presenting,
    PauseRequested,
    Paused,
}

struct Ownership {
    state: AtomicU8,
    epoch: AtomicU64,
}

impl Ownership {
    fn new() -> Self {
        Self { state: AtomicU8::new(OwnerState::Active as u8), epoch: AtomicU64::new(0) }
    }

    /// Single UI producer, one accepted frame: at most one worker entry/exit
    /// can race this call. Three CAS operations suffice; no retry/spin loop.
    fn pause(&self) -> bool {
        use OwnerState::*;
        let paused = match self.state.compare_exchange(Active as u8, Paused as u8, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => true,
            Err(value) if value == Paused as u8 => return true,
            Err(value) if value == Presenting as u8 => {
                match self.state.compare_exchange(
                    Presenting as u8,
                    PauseRequested as u8,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => false,
                    Err(value) if value == Paused as u8 => return true,
                    Err(value) if value == Active as u8 => self
                        .state
                        .compare_exchange(Active as u8, Paused as u8, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok(),
                    _ => return false,
                }
            }
            _ => return false,
        };
        // Published before the UI can resume. The state exclusion is what
        // makes CPU takeover safe; epoch additionally rejects acquire ABA.
        self.epoch.fetch_add(1, Ordering::AcqRel);
        paused
    }

    /// Some(true): this CAS resumed Paused; Some(false): already active.
    /// Reporting the actual transition avoids a stale UI state pre-read when
    /// the worker acknowledges PauseRequested immediately before this call.
    fn resume(&self) -> Option<bool> {
        use OwnerState::*;
        match self.state.load(Ordering::Acquire) {
            value if value == Active as u8 || value == Presenting as u8 => Some(false),
            value if value == Paused as u8 => {
                // Must precede publication of Active: a blocked old acquire
                // can enter immediately after the CAS, but must see new epoch.
                self.epoch.fetch_add(1, Ordering::AcqRel);
                self.state
                    .compare_exchange(Paused as u8, Active as u8, Ordering::AcqRel, Ordering::Acquire)
                    .ok()
                    .map(|_| true)
            }
            _ => None, // PauseRequested cannot be cancelled before worker ack.
        }
    }

    fn enter(&self) -> bool {
        self.state
            .compare_exchange(
                OwnerState::Active as u8,
                OwnerState::Presenting as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    /// Called only by the sole presentation lease. Return true on pause ack.
    fn finish(&self, success: bool) -> bool {
        use OwnerState::*;
        let next = if success { Active } else { Paused };
        match self.state.compare_exchange(Presenting as u8, next as u8, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => !success,
            Err(value) if value == PauseRequested as u8 => self
                .state
                .compare_exchange(PauseRequested as u8, Paused as u8, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            _ => false,
        }
    }
}

/// The atomic lease spans EVERY backing/onscreen submit through native present
/// return. A pause racing this section must wait for its typed acknowledgement.
/// Unwind/error releases ownership and wakes Err before backend teardown.
struct PresentLease<'a, F: Fn(Result<(), String>)> {
    state: &'a Shared,
    wake: &'a F,
    result: Option<Result<(), String>>,
}
impl<'a, F: Fn(Result<(), String>)> PresentLease<'a, F> {
    fn enter(state: &'a Shared, wake: &'a F) -> Option<Self> {
        if state.owner.enter() { Some(Self { state, wake, result: None }) } else { None }
    }
    fn for_frame(state: &'a Shared, wake: &'a F, epoch: u64) -> Option<Self> {
        let lease = Self::enter(state, wake)?;
        if epoch != state.owner.epoch.load(Ordering::Acquire) || state.stopping.load(Ordering::Acquire) {
            lease.complete(Ok(()));
            None
        } else {
            Some(lease)
        }
    }
    fn complete(mut self, result: Result<(), String>) {
        self.result = Some(result);
    }
}
impl<F: Fn(Result<(), String>)> Drop for PresentLease<'_, F> {
    fn drop(&mut self) {
        let result =
            self.result.take().unwrap_or_else(|| Err("GPU presentation aborted or panicked; using CPU".into()));
        if let Err(error) = &result {
            record_failure(&self.state.failure, error.clone());
        }
        let acknowledged = self.state.owner.finish(result.is_ok());
        if acknowledged || result.is_err() {
            // This wake acknowledges ownership, not necessarily mailbox
            // availability. ROOT rechecks set_active(false) before CPU present.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (self.wake)(result)));
        }
    }
}

/// UI facade: drawing resources belong to the worker. Instance is used ONLY
/// for main-thread attachment; a rejected attachment is retained for off-UI drop.
pub(crate) struct Gpu {
    tx: Option<mpsc::SyncSender<Command>>,
    retire: Option<mpsc::SyncSender<Retirement>>,
    shutdown: Option<mpsc::Receiver<ShutdownStatus>>,
    instance: Option<wgpu::Instance>,
    shared: Arc<Shared>,
    adapter_name: String,
    max_dimension: u32,
    max_bytes: u64,
    attached: Option<winit::window::WindowId>,
    size: (usize, usize),
    full: bool,
    local_error: Option<String>,
    stranded: Option<Command>,
}

struct Retirement {
    // Even a rejected attachment is dropped on the retirement lane, never UI.
    _instance: Option<wgpu::Instance>,
    _stranded: Option<Command>,
}

enum Command {
    Attach {
        surface: wgpu::Surface<'static>,
        window: Arc<winit::window::Window>,
    },
    Frame {
        packet: Packet,
        onscreen: bool,
        epoch: u64,
    },
    #[cfg(feature = "frame-harness")]
    Matches {
        expected: Vec<u32>,
        reply: mpsc::SyncSender<Result<bool, String>>,
    },
}

/// raw is either one full image or concatenated rows for each damage rectangle.
/// Both its length and rectangle metadata are bounded BEFORE allocation.
struct Packet {
    raw: Vec<u32>,
    damage: Vec<Rect>,
    size: (usize, usize),
    body: Rect,
    ch: usize,
    rows: usize,
    scroll: i64,
    full: bool,
}

impl Gpu {
    /// Call on ROOT's initialization worker, after the CPU first frame. This
    /// creates one renderer and one sleeping retirement supervisor, never a
    /// per-frame thread. The callback must only enqueue a host event, not wait.
    /// Ok means the accepted operation finished; Err is sticky CPU fallback.
    pub fn init_with_wake(wake: impl Fn(Result<(), String>) + Send + 'static) -> Result<Self, String> {
        let backend = Backend::init()?;
        let adapter_name = backend.adapter.get_info().name;
        let instance = backend.instance.clone();
        let max_dimension = backend.device.limits().max_texture_dimension_2d;
        // Admission uses the device's advertised allocation limit, not file
        // size. At most one viewport cache + one viewport handoff is retained.
        let max_bytes = backend.device.limits().max_buffer_size;
        let shared = Arc::new(Shared {
            busy: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            failure: backend.failure.clone(),
            owner: Ownership::new(),
        });
        let (tx, rx) = mpsc::sync_channel(1);
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let state = shared.clone();
        let renderer = thread::Builder::new()
            .name("kwil-native-renderer".into())
            .spawn(move || {
                // All GPU objects and queued surface commands are dropped INSIDE
                // this closure, before the termination receipt is sent.
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    renderer_loop(backend, rx, &state, &wake);
                }));
                if result.is_err() {
                    let error = "GPU renderer panicked; using CPU".to_string();
                    record_failure(&state.failure, error.clone());
                    state.owner.pause();
                    state.busy.store(false, Ordering::Release);
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| wake(Err(error))));
                }
                let _ = done_tx.send(());
            })
            .map_err(|e| e.to_string())?;
        let renderer_id = renderer.thread().id();
        let (retire_tx, retire_rx) = mpsc::sync_channel::<Retirement>(1);
        let (shutdown_tx, shutdown_rx) = mpsc::sync_channel(1);
        let _retirement = thread::Builder::new().name("kwil-renderer-retirement".into()).spawn(move || {
            // No idle polling: sleep until the single lifetime stop request.
            let retirement = retire_rx.recv().ok();
            let status = retire_renderer(renderer, done_rx, SHUTDOWN_DEADLINE);
            // Any extra Instance or rejected surface belongs to this off-UI
            // lane. Report worker status before their potentially slow drop.
            let _ = shutdown_tx.send(status);
            drop(retirement);
        }).map_err(|e| format!("cannot start GPU retirement supervisor: {e}; renderer {renderer_id:?} detached and will retire when command receiver closes"))?;
        Ok(Self {
            tx: Some(tx),
            retire: Some(retire_tx),
            shutdown: Some(shutdown_rx),
            instance: Some(instance),
            shared,
            adapter_name,
            max_dimension,
            max_bytes,
            attached: None,
            size: (0, 0),
            full: true,
            local_error: None,
            stranded: None,
        })
    }

    /// MAIN/UI THREAD ONLY. Call after GpuReady, and again when replacing the
    /// window. Safe create_surface retains ArcWindow and satisfies Metal's
    /// main-thread requirement. The worker does every subsequent surface call.
    /// Yes accepts attachment, Deferred leaves it untouched; wait for wake.
    pub fn attach_window(&mut self, window: Arc<winit::window::Window>) -> Presented {
        if self.attached == Some(window.id()) {
            return self.status();
        }
        match self.reserve() {
            Presented::Yes => {}
            other => return other,
        }
        let surface = match self.instance.as_ref().unwrap().create_surface(window.clone()) {
            Ok(surface) => surface,
            Err(error) => {
                self.shared.busy.store(false, Ordering::Release);
                return self.fail(error.to_string());
            }
        };
        let id = window.id();
        let result = self.send(Command::Attach { surface, window });
        if result == Presented::Yes {
            self.attached = Some(id);
            self.full = true;
        }
        result
    }

    pub fn adapter(&self) -> &str {
        &self.adapter_name
    }
    pub fn invalidate(&mut self) {
        self.full = true;
    }

    /// Nonblocking presenter handoff. false: ROOT must retain this presenter
    /// and all CPU damage, then retry on ownership wake; it MUST NOT CPU-present.
    /// true for inactive: no old worker can submit/present after CPU takeover.
    /// Acquire may remain blocked outside the lease without delaying that ack.
    /// Resume from Paused invalidates the full image; PauseRequested must first
    /// be acknowledged. Repeating the already-active state is harmless.
    pub fn set_active(&mut self, active: bool) -> bool {
        if active {
            if matches!(self.status(), Presented::Failed(_)) {
                return false;
            }
            match self.shared.owner.resume() {
                Some(transitioned) => {
                    self.full |= transitioned;
                    true
                }
                None => false,
            }
        } else {
            self.full = true;
            self.shared.owner.pause()
        }
    }
    /// UI: validate/admit/copy/try_send only. No driver/native surface calls,
    /// blocking channel send, mutex wait, readback, polling or joining.
    #[allow(clippy::too_many_arguments)]
    pub fn present(
        &mut self,
        window: Arc<winit::window::Window>,
        pixels: &[u32],
        size: (usize, usize),
        damage: &[Rect],
        scroll: i64,
        ch: usize,
        body: Rect,
    ) -> Presented {
        if self.shared.owner.state.load(Ordering::Acquire) != OwnerState::Active as u8 {
            self.full = true;
            return Presented::Deferred;
        }
        if size.0 == 0 || size.1 == 0 {
            self.full = true;
            return Presented::Deferred;
        }
        if self.attached != Some(window.id()) {
            return self.fail("call attach_window on UI before GPU present".into());
        }
        self.submit(pixels, size, damage, scroll, ch, body.h / ch.max(1), body, true)
    }

    #[allow(clippy::too_many_arguments)]
    fn submit(
        &mut self,
        pixels: &[u32],
        size: (usize, usize),
        damage: &[Rect],
        scroll: i64,
        ch: usize,
        rows: usize,
        body: Rect,
        onscreen: bool,
    ) -> Presented {
        match self.reserve() {
            Presented::Yes => {}
            other => {
                self.full = true;
                return other;
            }
        }
        if !valid_pixels(pixels.len(), size, self.max_dimension) || (size.0 * size.1) as u64 > self.max_bytes / 4 {
            self.shared.busy.store(false, Ordering::Release);
            return self.fail("GPU viewport exceeds device admission or pixel buffer".into());
        }
        if !needs_frame(self.full || self.size != size, false, damage, scroll) {
            self.shared.busy.store(false, Ordering::Release);
            return Presented::Yes;
        }
        let packet = match pack(pixels, size, damage, scroll, ch, rows, body, self.full || self.size != size) {
            Ok(packet) => packet,
            Err(error) => {
                self.shared.busy.store(false, Ordering::Release);
                return self.fail(error);
            }
        };
        let result =
            self.send(Command::Frame { packet, onscreen, epoch: self.shared.owner.epoch.load(Ordering::Acquire) });
        if result == Presented::Yes {
            self.size = size;
            self.full = false;
        }
        result
    }

    #[cfg(feature = "frame-harness")]
    pub fn update(
        &mut self,
        pixels: &[u32],
        size: (usize, usize),
        damage: &[Rect],
        scroll: i64,
        ch: usize,
        rows: usize,
    ) -> bool {
        self.update_body(pixels, size, damage, scroll, ch, rows, None)
    }

    /// Legacy headless backing seam, now asynchronous with the same one-frame
    /// admission as present. Do not call it in addition to present for a frame.
    #[cfg(feature = "frame-harness")]
    #[allow(clippy::too_many_arguments)]
    pub fn update_body(
        &mut self,
        pixels: &[u32],
        size: (usize, usize),
        damage: &[Rect],
        scroll: i64,
        ch: usize,
        rows: usize,
        body: Option<Rect>,
    ) -> bool {
        let body = match body {
            Some(body) => body,
            None => match rows.checked_mul(ch) {
                Some(h) if h <= size.1 => Rect { x: 0, y: 0, w: size.0, h },
                _ => return false,
            },
        };
        self.submit(pixels, size, damage, scroll, ch, rows, body, false) == Presented::Yes
    }

    fn status(&self) -> Presented {
        if let Some(error) = &self.local_error {
            return Presented::Failed(error.clone());
        }
        if self.shared.stopping.load(Ordering::Acquire) {
            return Presented::Failed("GPU renderer stopped".into());
        }
        match self.shared.failure.try_lock() {
            Ok(failure) => match failure.as_ref() {
                Some(error) => Presented::Failed(error.clone()),
                None => Presented::Yes,
            },
            Err(std::sync::TryLockError::WouldBlock) => {
                if self.shared.busy.load(Ordering::Acquire) {
                    Presented::Deferred
                } else {
                    Presented::Failed("GPU error notification in progress; using CPU".into())
                }
            }
            Err(_) => Presented::Failed("GPU error mailbox poisoned".into()),
        }
    }

    fn reserve(&self) -> Presented {
        match self.status() {
            Presented::Yes => {}
            other => return other,
        }
        if self.shared.busy.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            Presented::Yes
        } else {
            Presented::Deferred
        }
    }

    fn send(&mut self, command: Command) -> Presented {
        match self.tx.as_ref().unwrap().try_send(command) {
            Ok(()) => Presented::Yes,
            Err(error) => {
                // Only one producer and one reservation: a full/disconnected
                // mailbox is terminal. Preserve any Surface for off-UI drop.
                self.stranded = Some(unsent(error));
                self.shared.busy.store(false, Ordering::Release);
                self.fail("GPU command mailbox unavailable; using CPU".into())
            }
        }
    }

    fn fail(&mut self, error: String) -> Presented {
        if self.local_error.is_none() {
            self.local_error = Some(error);
        }
        Presented::Failed(self.local_error.as_ref().unwrap().clone())
    }

    /// Returns immediately; teardown/deadline run on the fixed retirement lane.
    /// Dropping without calling shutdown requests the same bounded retirement.
    pub fn shutdown(mut self) -> Shutdown {
        self.stop();
        Shutdown { result: self.shutdown.take().unwrap() }
    }

    fn stop(&mut self) {
        if self.retire.is_none() {
            return;
        }
        self.shared.stopping.store(true, Ordering::Release);
        self.shared.owner.pause(); // timeout/detach must never fabricate pause ack
        drop(self.tx.take()); // wakes recv; no UI-side queue/driver drain
        let retirement = Retirement { _instance: self.instance.take(), _stranded: self.stranded.take() };
        if let Err(error) = self.retire.take().unwrap().try_send(retirement) {
            eprintln!("GPU retirement supervisor unavailable; retaining native handles until process exit");
            std::mem::forget(unsent(error)); // never drop GPU handles on UI
        }
    }

    /// DEV ONLY, call off UI after the completion wake. Busy returns immediately;
    /// the accepted readback request waits at most 12s, even if driver hangs.
    #[cfg(feature = "frame-harness")]
    pub fn matches(&self, pixels: &[u32]) -> Result<bool, String> {
        match self.reserve() {
            Presented::Yes => {}
            Presented::Deferred => return Err("GPU harness busy; wait for completion wake".into()),
            Presented::Failed(error) => return Err(error),
        }
        if !valid_pixels(pixels.len(), self.size, self.max_dimension) {
            self.shared.busy.store(false, Ordering::Release);
            return Err("invalid harness viewport".into());
        }
        let mut expected = Vec::new();
        if expected.try_reserve_exact(self.size.0 * self.size.1).is_err() {
            self.shared.busy.store(false, Ordering::Release);
            return Err("harness allocation refused".into());
        }
        expected.extend_from_slice(&pixels[..self.size.0 * self.size.1]);
        let (reply, result) = mpsc::sync_channel(1);
        if self.tx.as_ref().unwrap().try_send(Command::Matches { expected, reply }).is_err() {
            self.shared.busy.store(false, Ordering::Release);
            return Err("GPU harness mailbox unavailable".into());
        }
        result.recv_timeout(Duration::from_secs(12)).map_err(|e| format!("bounded GPU harness request: {e}"))?
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        self.stop();
    }
}

fn unsent<T>(error: mpsc::TrySendError<T>) -> T {
    match error {
        mpsc::TrySendError::Full(value) | mpsc::TrySendError::Disconnected(value) => value,
    }
}

fn retire_renderer(renderer: thread::JoinHandle<()>, done: mpsc::Receiver<()>, deadline: Duration) -> ShutdownStatus {
    if done.recv_timeout(deadline).is_ok() {
        // Receipt follows every worker GPU/resource destructor. Do not join:
        // arbitrary callback captures may still run destructors at thread exit.
        drop(renderer);
        ShutdownStatus::Stopped
    } else {
        let diagnostic = format!(
            "GPU renderer {:?} exceeded {}ms shutdown deadline; detached owner retains GPU/surface/ArcWindow/viewport until driver returns or process exit",
            renderer.thread().id(),
            deadline.as_millis()
        );
        eprintln!("{diagnostic}");
        drop(renderer); // detach; NEVER kill or free in-use resources
        ShutdownStatus::Detached(diagnostic)
    }
}

#[allow(clippy::too_many_arguments)]
fn pack(
    pixels: &[u32],
    size: (usize, usize),
    damage: &[Rect],
    scroll: i64,
    ch: usize,
    rows: usize,
    body: Rect,
    force_full: bool,
) -> Result<Packet, String> {
    let count = size.0.checked_mul(size.1).filter(|&n| n <= pixels.len()).ok_or("invalid packet pixels")?;
    if !valid_rect(body, size) || (scroll != 0 && ch == 0) {
        return Err("invalid GPU body/cell height".into());
    }
    let plan = scroll_plan(body, scroll, ch, rows);
    // Ignore pathological rectangle lists in favour of one admitted snapshot;
    // metadata scanning/allocation never scales beyond MAX_DAMAGE_RECTS.
    let mut full = force_full || damage.len() >= MAX_DAMAGE_RECTS;
    let mut rects = Vec::new();
    rects.try_reserve_exact(if full { 1 } else { damage.len() + 1 }).map_err(|_| "GPU damage allocation refused")?;
    if !full {
        for &rect in damage {
            if !valid_rect(rect, size) {
                return Err("GPU damage outside viewport".into());
            }
            if rect.w != 0 && rect.h != 0 {
                rects.push(rect);
            }
        }
        // Always retain/upload the exposed strip, even when the caller gives
        // only other damage. Chrome and side panes remain byte-identical.
        match plan {
            ScrollPlan::Copy { from, to, keep } => {
                let height = body.h - keep;
                let y = if from > to { body.y + keep } else { body.y };
                rects.push(Rect { x: body.x, y, w: body.w, h: height });
            }
            ScrollPlan::Repaint => rects.push(body),
            ScrollPlan::None => {}
        }
        let area = rects.iter().try_fold(0usize, |sum, r| r.w.checked_mul(r.h).and_then(|n| sum.checked_add(n)));
        full = area.is_none_or(|area| area > count);
    }
    if full {
        rects.clear();
        rects.push(Rect { x: 0, y: 0, w: size.0, h: size.1 });
    }
    let area = rects.iter().map(|r| r.w * r.h).sum();
    let mut raw = Vec::new();
    raw.try_reserve_exact(area).map_err(|_| "GPU viewport handoff allocation refused")?;
    for rect in &rects {
        for y in rect.y..rect.y + rect.h {
            raw.extend_from_slice(&pixels[y * size.0 + rect.x..y * size.0 + rect.x + rect.w]);
        }
    }
    Ok(Packet { raw, damage: rects, size, body, ch, rows, scroll: if full { 0 } else { scroll }, full })
}

fn apply_packet(packet: &mut Packet, pixels: &mut Vec<u32>, size: &mut (usize, usize)) -> Result<(), String> {
    if packet.full {
        *pixels = std::mem::take(&mut packet.raw);
        *size = packet.size;
        return Ok(());
    }
    if *size != packet.size {
        return Err("GPU partial packet has no matching CPU viewport".into());
    }
    if let ScrollPlan::Copy { from, to, keep } = scroll_plan(packet.body, packet.scroll, packet.ch, packet.rows) {
        for row in 0..keep {
            let row = if to > from { keep - row - 1 } else { row };
            let source = (from + row) * size.0 + packet.body.x;
            let destination = (to + row) * size.0 + packet.body.x;
            pixels.copy_within(source..source + packet.body.w, destination);
        }
    }
    let mut offset = 0;
    for rect in &packet.damage {
        for y in rect.y..rect.y + rect.h {
            let destination = y * size.0 + rect.x;
            pixels[destination..destination + rect.w].copy_from_slice(&packet.raw[offset..offset + rect.w]);
            offset += rect.w;
        }
    }
    Ok(())
}

fn renderer_loop(
    mut backend: Backend,
    rx: mpsc::Receiver<Command>,
    state: &Shared,
    wake: &impl Fn(Result<(), String>),
) {
    let mut pixels = Vec::new();
    let mut size = (0, 0);
    // Blocking receive is the only idle activity. No acquire/poll/submission or
    // frame-ready retry happens without an accepted host command.
    while let Ok(command) = rx.recv() {
        if state.stopping.load(Ordering::Acquire) {
            break;
        }
        let result = match command {
            Command::Attach { surface, window } => {
                backend.onscreen = None;
                backend.size = (0, 0);
                match backend.attach(surface, window.clone(), (1, 1)) {
                    Ok(onscreen) => { backend.window = Some(window); backend.onscreen = Some(onscreen); Ok(()) },
                    Err(error) => Err(error),
                }
            }
            Command::Frame { mut packet, onscreen, epoch } => {
                apply_packet(&mut packet, &mut pixels, &mut size).and_then(|()| {
                    if packet.full { backend.invalidate(); }
                    if onscreen {
                        let window = backend.window.clone().ok_or("GPU window attachment missing")?;
                        match backend.present(window, &pixels, size, &packet.damage, packet.scroll,
                            packet.ch, packet.body, state, epoch, wake) {
                            Presented::Yes => Ok(()),
                            Presented::Deferred => Err("GPU surface could not present the accepted frame; using CPU without an acquisition retry".into()),
                            Presented::Failed(error) => Err(error),
                        }
                    } else if backend.update_body(&pixels, size, &packet.damage, packet.scroll,
                        packet.ch, packet.rows, Some(packet.body)) { Ok(()) }
                    else { Err(backend.error().unwrap_or_else(|| "GPU backing update failed".into())) }
                })
            }
            #[cfg(feature = "frame-harness")]
            Command::Matches { expected, reply } => {
                let result = backend.matches(&expected);
                let completion = result.as_ref().map(|_| ()).map_err(Clone::clone);
                let _ = reply.try_send(result);
                completion
            }
        };
        if let Err(error) = &result {
            record_failure(&state.failure, error.clone());
            state.owner.pause();
        }
        // Cache and command payload are settled BEFORE making admission free.
        state.busy.store(false, Ordering::Release);
        if state.stopping.load(Ordering::Acquire) {
            break;
        }
        wake(result);
    }
}

struct Onscreen {
    // Safe create_surface(Arc<Window>) retains the window until surface drop.
    surface: wgpu::Surface<'static>,
    window: winit::window::WindowId,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    configure: bool,
}

struct Backend {
    instance: wgpu::Instance,
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipelines: [wgpu::RenderPipeline; 4],
    failure: Arc<Mutex<Option<String>>>,
    onscreen: Option<Onscreen>,
    window: Option<Arc<winit::window::Window>>,
    texture: Option<wgpu::Texture>,
    scratch: Option<wgpu::Texture>,
    size: (usize, usize),
    full: bool,
}
impl Backend {
    pub fn invalidate(&mut self) {
        // A coalesced CPU snapshot requires a full upload, not a new texture or
        // swapchain. Only actual size/window changes recreate GPU allocations.
        self.full = true;
    }
    #[cfg(feature = "frame-harness")]
    pub fn matches(&self, pixels: &[u32]) -> Result<bool, String> {
        let texture = self.texture.as_ref().ok_or("backing not initialized")?;
        let (w, h) = self.size;
        if !valid_pixels(pixels.len(), (w, h), self.device.limits().max_texture_dimension_2d) {
            return Err("invalid readback dimensions or pixel buffer".into());
        }
        let stride = (w * 4).div_ceil(256) * 256;
        let bytes = stride.checked_mul(h).ok_or("readback size overflow")?;
        if bytes as u64 > self.device.limits().max_buffer_size {
            return Err("readback exceeds device buffer limit".into());
        }
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("kwil harness readback"),
            size: bytes as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = self.device.create_command_encoder(&Default::default());
        encoder.copy_texture_to_buffer(
            copy(texture, 0, 0),
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(stride as u32),
                    rows_per_image: Some(h as u32),
                },
            },
            extent(w, h),
        );
        self.queue.submit([encoder.finish()]);
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        buffer.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        self.device
            .poll(wgpu::PollType::Wait { submission_index: None, timeout: Some(std::time::Duration::from_secs(5)) })
            .map_err(|e| e.to_string())?;
        rx.recv_timeout(std::time::Duration::from_secs(5)).map_err(|e| e.to_string())?.map_err(|e| e.to_string())?;
        let bytes = buffer.slice(..).get_mapped_range().map_err(|e| e.to_string())?;
        let equal = (0..h).all(|y| {
            (0..w).all(|x| {
                let p = pixels[y * w + x];
                let i = y * stride + x * 4;
                bytes[i..i + 4] == [(p >> 16) as u8, (p >> 8) as u8, p as u8, 255]
            })
        });
        drop(bytes);
        buffer.unmap();
        Ok(equal)
    }
    pub fn init() -> Result<Self, String> {
        #[cfg(target_os = "macos")]
        let backends = wgpu::Backends::METAL;
        #[cfg(not(target_os = "macos"))]
        let backends = wgpu::Backends::VULKAN | wgpu::Backends::GL;
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let adapter = pollster::block_on(instance.request_adapter(&Default::default())).map_err(|e| e.to_string())?;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
            .map_err(|e| e.to_string())?;
        let failure = Arc::new(Mutex::new(None));
        let errors = failure.clone();
        device.on_uncaptured_error(Arc::new(move |error| record_failure(&errors, error.to_string())));
        let lost = failure.clone();
        device.set_device_lost_callback(move |reason, message| {
            record_failure(&lost, format!("GPU device lost ({reason:?}): {message}"));
        });
        // init is worker-owned, including shader parsing. ROOT must enable the
        // existing wgpu `wgsl` feature for the macOS and Linux dependencies.
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("kwil backing blit"),
            source: wgpu::ShaderSource::Wgsl(BLIT.into()),
        });
        // All four small SDR variants compile on the init worker. Attachment
        // only chooses a pipeline; no shader/pipeline compilation on the UI.
        let pipelines = SURFACE_FORMATS.map(|format| blit_pipeline(&device, &shader, format));
        if let Some(error) = failure.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            return Err(error);
        }
        Ok(Self {
            instance,
            adapter,
            device,
            queue,
            pipelines,
            failure,
            onscreen: None,
            window: None,
            texture: None,
            scratch: None,
            size: (0, 0),
            full: true,
        })
    }

    /// RENDERER WORKER ONLY, after the CPU first frame. `pixels` is
    /// the complete current 0x00RRGGBB image, including copied scroll contents.
    /// Acquire BEFORE updating backing, so Deferred cannot consume/replay scroll.
    ///
    /// Acquisition limitation, audited against cached wgpu 30.0.1 sources:
    /// public Surface::get_current_texture has no timeout/nonblocking argument;
    /// wgpu-core/src/present.rs requests 1000 ms. Metal ignores that timeout and
    /// sets allowsNextDrawableTimeout(false), so it has NO finite acquire bound
    /// (it does skip known occluded windows). GLES swap/present and configure
    /// can also wait on the driver. FIFO is not a nonblocking guarantee. No
    /// internal poll, timer, retry, request_redraw or device.wait is used here.
    #[allow(clippy::too_many_arguments)]
    pub fn present(
        &mut self,
        window: Arc<winit::window::Window>,
        pixels: &[u32],
        size: (usize, usize),
        damage: &[Rect],
        scroll: i64,
        ch: usize,
        body: Rect,
        state: &Shared,
        epoch: u64,
        wake: &impl Fn(Result<(), String>),
    ) -> Presented {
        if state.stopping.load(Ordering::Acquire) {
            return Presented::Failed("GPU renderer stopped".into());
        }
        if let Some(error) = self.error() {
            return Presented::Failed(error);
        }
        if size.0 == 0 || size.1 == 0 {
            self.invalidate();
            return Presented::Deferred;
        }
        if !valid_pixels(pixels.len(), size, self.device.limits().max_texture_dimension_2d)
            || !valid_rect(body, size)
            || damage.iter().any(|&r| !valid_rect(r, size))
            || (scroll != 0 && ch == 0)
        {
            return Presented::Failed("invalid GPU frame dimensions, pixels, damage or cell height".into());
        }
        let native_size = window.inner_size();
        if (native_size.width as usize, native_size.height as usize) != size {
            // A resize raced layout; wait for the host's resize event/frame.
            self.invalidate();
            return Presented::Deferred;
        }
        if self.onscreen.as_ref().is_none_or(|s| s.window != window.id()) {
            return Presented::Failed("GPU surface not attached".into());
        }
        let onscreen = self.onscreen.as_mut().unwrap();
        let changed_size = (onscreen.config.width as usize, onscreen.config.height as usize) != size;
        if !needs_frame(self.full || self.size != size, onscreen.configure || changed_size, damage, scroll) {
            return Presented::Yes; // No acquisition, allocation or submission at idle.
        }
        if onscreen.configure || changed_size {
            onscreen.config.width = size.0 as u32;
            onscreen.config.height = size.1 as u32;
            onscreen.surface.configure(&self.device, &onscreen.config);
            onscreen.configure = false;
        }
        if let Some(error) = self.error() {
            return Presented::Failed(error);
        }
        let onscreen = self.onscreen.as_mut().unwrap();
        let frame = match onscreen.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame) => frame,
            wgpu::CurrentSurfaceTexture::Suboptimal(frame) => {
                // Finish this frame; reconfigure on the next real host event.
                onscreen.configure = true;
                frame
            }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                return self.deferred();
            }
            wgpu::CurrentSurfaceTexture::Outdated => {
                onscreen.configure = true;
                return self.deferred();
            }
            wgpu::CurrentSurfaceTexture::Lost => {
                self.onscreen = None; // Recreate, not merely reconfigure, next event.
                return self.deferred();
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                return Presented::Failed(
                    self.error().unwrap_or_else(|| "GPU surface acquisition validation failed".into()),
                );
            }
        };
        // Drop every acquired frame before any later configure/recreate.
        // Acquire is intentionally OUTSIDE ownership. The CAS, rather than a
        // racy generation check, excludes CPU takeover throughout all submits
        // and the native present. Epoch is checked only AFTER taking the lease.
        let Some(lease) = PresentLease::for_frame(state, wake, epoch) else {
            self.invalidate();
            return Presented::Yes; // accepted old frame cancelled, no GPU submit
        };
        let result = (|| -> Result<(), String> {
            if !self.update_body(pixels, size, damage, scroll, ch, body.h / ch.max(1), Some(body)) {
                return Err(self.error().unwrap_or_else(|| "GPU backing update failed".into()));
            }
            let onscreen = self.onscreen.as_ref().unwrap();
            let backing_view = self.texture.as_ref().unwrap().create_view(&Default::default());
            let bindings = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("kwil backing blit"),
                layout: &onscreen.pipeline.get_bind_group_layout(0),
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&backing_view),
                }],
            });
            let target = frame.texture.create_view(&Default::default());
            let mut encoder = self.device.create_command_encoder(&Default::default());
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("kwil onscreen blit"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &target,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    ..Default::default()
                });
                pass.set_pipeline(&onscreen.pipeline);
                pass.set_bind_group(0, &bindings, &[]);
                pass.draw(0..3, 0..1);
            }
            self.queue.submit([encoder.finish()]);
            if let Some(error) = self.error() {
                return Err(error);
            }
            if state.stopping.load(Ordering::Acquire) {
                return Err("GPU renderer stopped".into());
            }
            window.pre_present_notify(); // Wayland frame callback before commit.
            self.queue.present(frame);
            self.error().map_or(Ok(()), Err)
        })();
        lease.complete(result.clone());
        result.map_or_else(Presented::Failed, |()| Presented::Yes)
    }

    fn error(&self) -> Option<String> {
        self.failure.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn deferred(&self) -> Presented {
        match self.error() {
            Some(error) => Presented::Failed(error),
            None => Presented::Deferred,
        }
    }

    fn attach(
        &self,
        surface: wgpu::Surface<'static>,
        window: Arc<winit::window::Window>,
        size: (usize, usize),
    ) -> Result<Onscreen, String> {
        let id = window.id();
        // init had no UI window. Reject an incompatible headless-selected
        // adapter rather than initialize another device on the UI thread.
        let caps = surface.get_capabilities(&self.adapter);
        let format = surface_format(&caps.formats).ok_or("GPU adapter has no supported SDR window format")?;
        if !caps.usages.contains(wgpu::TextureUsages::RENDER_ATTACHMENT)
            || !caps.present_modes.contains(&wgpu::PresentMode::Fifo)
        {
            return Err("GPU surface lacks render attachment or FIFO support".into());
        }
        let alpha_mode = caps
            .alpha_modes
            .iter()
            .copied()
            .find(|m| *m == wgpu::CompositeAlphaMode::Opaque)
            .or_else(|| caps.alpha_modes.first().copied())
            .ok_or("GPU surface has no alpha mode")?;
        let mut config = surface
            .get_default_config(&self.adapter, size.0 as u32, size.1 as u32)
            .ok_or("GPU adapter cannot present this window")?;
        config.format = format;
        config.present_mode = wgpu::PresentMode::Fifo;
        config.alpha_mode = alpha_mode;
        config.desired_maximum_frame_latency = 2;
        let index = SURFACE_FORMATS.iter().position(|&candidate| candidate == format).unwrap();
        let pipeline = self.pipelines[index].clone();
        if let Some(error) = self.error() {
            return Err(error);
        }
        Ok(Onscreen { surface, window: id, config, pipeline, configure: true })
    }
    /// Upload only new damage; preserve scroll pixels via two texture copies
    /// because overlapping self-copies are not valid WebGPU operations.
    #[allow(clippy::too_many_arguments)]
    pub fn update_body(
        &mut self,
        pixels: &[u32],
        size: (usize, usize),
        rects: &[Rect],
        scroll: i64,
        cell_height: usize,
        rows: usize,
        body: Option<Rect>,
    ) -> bool {
        let (w, h) = size;
        if !valid_pixels(pixels.len(), size, self.device.limits().max_texture_dimension_2d)
            || rects.iter().any(|&r| !valid_rect(r, size))
            || body.is_some_and(|r| !valid_rect(r, size))
            || self.error().is_some()
        {
            return false;
        }
        let body = match body {
            Some(body) => body,
            None => match rows.checked_mul(cell_height) {
                Some(height) if height <= h => Rect { x: 0, y: 0, w, h: height },
                _ => return false,
            },
        };
        let fresh = self.size != size;
        let repaint_all = fresh || self.full;
        if !needs_frame(repaint_all, false, rects, scroll) {
            return true;
        }
        if fresh {
            // CPU bytes carry sRGB values. A matching sRGB backing decodes on
            // textureLoad before an sRGB target encodes them again. Unorm
            // targets copy bytes unchanged. No VIEW_FORMATS downlevel feature
            // is required (important for GLES). Attachment invalidates size.
            let format = if self.onscreen.as_ref().is_some_and(|s| s.config.format.is_srgb()) {
                wgpu::TextureFormat::Rgba8UnormSrgb
            } else {
                wgpu::TextureFormat::Rgba8Unorm
            };
            let descriptor = wgpu::TextureDescriptor {
                label: Some("kwil damage backing"),
                size: extent(w, h),
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::COPY_DST
                    | wgpu::TextureUsages::COPY_SRC
                    | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            };
            self.texture = Some(self.device.create_texture(&descriptor));
            self.scratch = Some(self.device.create_texture(&descriptor));
            self.size = size;
        }
        let texture = self.texture.as_ref().unwrap();
        let plan = if repaint_all { ScrollPlan::None } else { scroll_plan(body, scroll, cell_height, rows) };
        if let ScrollPlan::Copy { from, to, keep } = plan {
            let mut encoder = self.device.create_command_encoder(&Default::default());
            let scratch = self.scratch.as_ref().unwrap();
            encoder.copy_texture_to_texture(copy(texture, body.x, from), copy(scratch, 0, 0), extent(body.w, keep));
            encoder.copy_texture_to_texture(copy(scratch, 0, 0), copy(texture, body.x, to), extent(body.w, keep));
            // Flush copies BEFORE queued writes overwrite the exposed strip.
            self.queue.submit([encoder.finish()]);
        }
        let full = [Rect { x: 0, y: 0, w, h }];
        let repaint = if plan == ScrollPlan::Repaint { Some(body) } else { None };
        let mut uploaded = false;
        for r in (if repaint_all { &full[..] } else { rects }).iter().copied().chain(repaint) {
            if r.w == 0 || r.h == 0 {
                continue;
            }
            // valid_pixels proved all viewport byte arithmetic fits usize.
            let mut data = Vec::new();
            if data.try_reserve_exact(r.w * r.h * 4).is_err() {
                self.full = true; // A partial update must never be reused.
                return false;
            }
            for y in r.y..r.y + r.h {
                for &color in &pixels[y * w + r.x..y * w + r.x + r.w] {
                    data.extend_from_slice(&[(color >> 16) as u8, (color >> 8) as u8, color as u8, 255]);
                }
            }
            self.queue.write_texture(
                copy(texture, r.x, r.y),
                &data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(r.w as u32 * 4),
                    rows_per_image: Some(r.h as u32),
                },
                extent(r.w, r.h),
            );
            uploaded = true;
        }
        if uploaded {
            self.queue.submit([]);
        }
        let success = self.error().is_none();
        self.full = !success;
        success
    }
}

fn record_failure(failure: &Mutex<Option<String>>, error: String) {
    // One sticky, bounded mailbox; never accumulate driver errors per frame.
    let mut slot = failure.lock().unwrap_or_else(|e| e.into_inner());
    if slot.is_none() {
        *slot = Some(error);
    }
}

fn valid_pixels(len: usize, (w, h): (usize, usize), max: u32) -> bool {
    w != 0
        && h != 0
        && w <= max as usize
        && h <= max as usize
        && w.checked_mul(h).filter(|&count| count <= len).and_then(|n| n.checked_mul(4)).is_some()
        && w.checked_mul(4).is_some_and(|stride| stride <= u32::MAX as usize)
}

fn valid_rect(r: Rect, (w, h): (usize, usize)) -> bool {
    r.x <= w && r.y <= h && r.w <= w - r.x && r.h <= h - r.y
}

fn needs_frame(fresh: bool, configure: bool, rects: &[Rect], scroll: i64) -> bool {
    fresh || configure || scroll != 0 || rects.iter().any(|r| r.w != 0 && r.h != 0)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScrollPlan {
    None,
    Copy { from: usize, to: usize, keep: usize },
    Repaint,
}

fn scroll_plan(body: Rect, scroll: i64, ch: usize, rows: usize) -> ScrollPlan {
    if scroll == 0 || body.w == 0 || body.h == 0 {
        return ScrollPlan::None;
    }
    let lines = match usize::try_from(scroll.unsigned_abs()) {
        Ok(lines) if lines < rows && ch != 0 => lines,
        _ => return ScrollPlan::Repaint,
    };
    let dy = match lines.checked_mul(ch) {
        Some(dy) if dy < body.h => dy,
        _ => return ScrollPlan::Repaint,
    };
    let Some(shifted) = body.y.checked_add(dy) else {
        return ScrollPlan::Repaint;
    };
    let (from, to) = if scroll > 0 { (shifted, body.y) } else { (body.y, shifted) };
    ScrollPlan::Copy { from, to, keep: body.h - dy }
}

const SURFACE_FORMATS: [wgpu::TextureFormat; 4] = [
    wgpu::TextureFormat::Bgra8UnormSrgb,
    wgpu::TextureFormat::Rgba8UnormSrgb,
    wgpu::TextureFormat::Bgra8Unorm,
    wgpu::TextureFormat::Rgba8Unorm,
];

fn surface_format(formats: &[wgpu::TextureFormat]) -> Option<wgpu::TextureFormat> {
    SURFACE_FORMATS.into_iter().find(|format| formats.contains(format))
}

fn blit_pipeline(
    device: &wgpu::Device,
    shader: &wgpu::ShaderModule,
    format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("kwil backing blit"),
        layout: None,
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vertex"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        primitive: Default::default(),
        depth_stencil: None,
        multisample: Default::default(),
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some("fragment"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState { format, blend: None, write_mask: wgpu::ColorWrites::ALL })],
        }),
        multiview_mask: None,
        cache: None,
    })
}

// A single oversized triangle covers every pixel without a vertex buffer,
// sampler, scale/filter, Y flip or half-texel offset. Fragment position is in
// top-left pixel coordinates on all wgpu backends, including Metal and GL.
const BLIT: &str = r#"
@group(0) @binding(0) var backing: texture_2d<f32>;
@vertex fn vertex(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    let x = f32((index << 1u) & 2u);
    let y = f32(index & 2u);
    return vec4<f32>(x * 2.0 - 1.0, y * 2.0 - 1.0, 0.0, 1.0);
}
@fragment fn fragment(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    return textureLoad(backing, vec2<i32>(position.xy), 0);
}
"#;

fn extent(w: usize, h: usize) -> wgpu::Extent3d {
    wgpu::Extent3d { width: w as u32, height: h as u32, depth_or_array_layers: 1 }
}
fn copy(texture: &wgpu::Texture, x: usize, y: usize) -> wgpu::TexelCopyTextureInfo<'_> {
    wgpu::TexelCopyTextureInfo {
        texture,
        mip_level: 0,
        origin: wgpu::Origin3d { x: x as u32, y: y as u32, z: 0 },
        aspect: wgpu::TextureAspect::All,
    }
}

// R12 windowless protocol fixture: actual Gpu stop/retire_renderer/shutdown
// paths, with no device/window/native GPU work. Not GPU qualification.
#[cfg(test)]
pub(super) struct NotificationFixture {
    pub(super) identity: usize,
    supervisor: thread::JoinHandle<()>,
    exited: mpsc::Receiver<()>,
}
#[cfg(test)]
impl NotificationFixture {
    pub(super) fn finish(self, deadline: std::time::Instant) {
        self.exited
            .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
            .expect("original windowless retirement supervisor must finish on time");
        self.supervisor.join().unwrap();
        assert!(std::time::Instant::now() < deadline);
    }
}
#[cfg(test)]
pub(super) fn notification_fixture(
    wake: impl Fn(Result<(), String>) + Send + 'static,
    deadline: std::time::Instant,
) -> (Gpu, NotificationFixture) {
    let (mut gpu, commands) = tests::fake_gpu();
    let identity = Arc::as_ptr(&gpu.shared) as usize;
    let (done, finished) = mpsc::sync_channel(1);
    let renderer = thread::spawn(move || {
        if matches!(
            commands.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ) {
            drop(commands);
            drop(wake);
            let _ = done.send(()); // follows actual resource/callback disposal
        }
    });
    let (retire, retirement) = mpsc::sync_channel::<Retirement>(1);
    let (shutdown, status) = mpsc::sync_channel(1);
    let (exited, exit) = mpsc::sync_channel(1);
    let supervisor = thread::spawn(move || {
        let original = retirement.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()));
        let retired =
            retire_renderer(renderer, finished, deadline.saturating_duration_since(std::time::Instant::now()));
        let result = match original {
            Ok(original) => {
                drop(original);
                retired
            }
            Err(error) => ShutdownStatus::Detached(format!("windowless original retirement missing: {error}")),
        };
        let _ = shutdown.send(result);
        let _ = exited.send(());
    });
    gpu.retire = Some(retire);
    gpu.shutdown = Some(status);
    (gpu, NotificationFixture { identity, supervisor, exited: exit })
}
#[cfg(test)]
impl Gpu {
    pub(super) fn notification_fixture_identity(&self) -> usize {
        Arc::as_ptr(&self.shared) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // No device, native window, timer, polling loop or driver required. Leaving
    // the fake mailbox reserved models an acquire that never returns.
    pub(super) fn fake_gpu() -> (Gpu, mpsc::Receiver<Command>) {
        let (tx, rx) = mpsc::sync_channel(1);
        (
            Gpu {
                tx: Some(tx),
                retire: None,
                shutdown: None,
                instance: None,
                shared: Arc::new(Shared {
                    busy: AtomicBool::new(false),
                    stopping: AtomicBool::new(false),
                    failure: Arc::new(Mutex::new(None)),
                    owner: Ownership::new(),
                }),
                adapter_name: "fake device-independent renderer".into(),
                max_dimension: 64,
                max_bytes: 64 * 64 * 4,
                attached: None,
                size: (0, 0),
                full: true,
                local_error: None,
                stranded: None,
            },
            rx,
        )
    }

    #[test]
    fn blocked_acquire_can_pause_immediately_and_cannot_present_after_takeover_or_resume() {
        for resume_before_acquire_returns in [false, true] {
            let (mut gpu, _rx) = fake_gpu();
            let state = gpu.shared.clone();
            let epoch = state.owner.epoch.load(Ordering::Acquire);
            let (entered, acquiring) = mpsc::sync_channel(1);
            let (release, blocked) = mpsc::sync_channel(1);
            let (returned, finished) = mpsc::sync_channel(1);
            let worker = thread::spawn(move || {
                entered.try_send(()).unwrap();
                blocked.recv_timeout(Duration::from_secs(1)).unwrap(); // finite fake acquire
                let wake = |_| {};
                let presented = if let Some(lease) = PresentLease::for_frame(&state, &wake, epoch) {
                    lease.complete(Ok(()));
                    true // models the ONLY path allowed to submit/present
                } else {
                    false
                };
                returned.try_send(presented).unwrap();
            });
            acquiring.recv_timeout(Duration::from_secs(1)).unwrap();
            assert!(gpu.set_active(false)); // safe CPU takeover despite blocked acquire
            assert_eq!(gpu.shared.owner.state.load(Ordering::Acquire), OwnerState::Paused as u8);
            if resume_before_acquire_returns {
                assert!(gpu.set_active(true));
            }
            release.try_send(()).unwrap();
            assert!(!finished.recv_timeout(Duration::from_secs(1)).unwrap());
            drop(worker); // all fake waits have deadlines; no driver/thread join
        }
    }

    #[test]
    fn blocked_present_keeps_cpu_gate_closed_even_after_retirement_deadline_until_ack() {
        let (mut gpu, _rx) = fake_gpu();
        let state = gpu.shared.clone();
        let epoch = state.owner.epoch.load(Ordering::Acquire);
        let (entered, presenting) = mpsc::sync_channel(1);
        let (release, blocked) = mpsc::sync_channel(1);
        let (wake, acknowledged) = mpsc::sync_channel(1);
        let (done, receipt) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            let wake = |result| {
                let _ = wake.try_send(result);
            };
            let lease = PresentLease::for_frame(&state, &wake, epoch).unwrap();
            entered.try_send(()).unwrap();
            blocked.recv_timeout(Duration::from_secs(1)).unwrap(); // finite fake native present
            lease.complete(Ok(())); // native present has now RETURNED
            done.try_send(()).unwrap();
        });
        presenting.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(!gpu.set_active(false));
        assert_eq!(gpu.shared.owner.state.load(Ordering::Acquire), OwnerState::PauseRequested as u8);
        assert!(!gpu.set_active(true)); // must not cancel pending pause
        assert!(!gpu.set_active(false));
        assert!(matches!(acknowledged.try_recv(), Err(mpsc::TryRecvError::Empty)));
        assert!(matches!(retire_renderer(worker, receipt, Duration::ZERO), ShutdownStatus::Detached(_)));
        assert!(!gpu.set_active(false)); // detach/timeout is NOT CPU ownership
        release.try_send(()).unwrap();
        assert_eq!(acknowledged.recv_timeout(Duration::from_secs(1)).unwrap(), Ok(()));
        assert!(gpu.set_active(false));
        assert_eq!(gpu.shared.owner.state.load(Ordering::Acquire), OwnerState::Paused as u8);
    }

    #[test]
    fn presentation_error_and_unwind_release_ownership_and_wake_failure() {
        for panic_in_present in [false, true] {
            let (mut gpu, _rx) = fake_gpu();
            let (notification, result) = mpsc::sync_channel(1);
            let wake = |value| {
                let _ = notification.try_send(value);
            };
            let state = gpu.shared.clone();
            let epoch = state.owner.epoch.load(Ordering::Acquire);
            let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let lease = PresentLease::for_frame(&state, &wake, epoch).unwrap();
                assert!(!gpu.set_active(false));
                if panic_in_present {
                    panic!("finite fake native present panic");
                }
                lease.complete(Err("finite fake native present error".into()));
            }));
            assert_eq!(unwound.is_err(), panic_in_present);
            assert!(result.recv_timeout(Duration::from_secs(1)).unwrap().is_err());
            assert!(gpu.set_active(false));
            assert!(!gpu.set_active(true)); // sticky failure cannot resume GPU
            assert_eq!(state.owner.state.load(Ordering::Acquire), OwnerState::Paused as u8);
        }
    }

    #[test]
    fn cached_active_state_is_harmless_and_resume_requires_full_snapshot() {
        let (mut gpu, _rx) = fake_gpu();
        gpu.full = false;
        assert!(gpu.set_active(true));
        assert!(!gpu.full);
        let state = gpu.shared.clone();
        let wake = |_| {};
        let lease = PresentLease::for_frame(&state, &wake, state.owner.epoch.load(Ordering::Acquire)).unwrap();
        assert!(gpu.set_active(true)); // already active, currently presenting
        assert!(!gpu.full);
        assert!(!gpu.set_active(false));
        lease.complete(Ok(()));
        assert!(gpu.set_active(false));
        gpu.full = false;
        assert!(gpu.set_active(true));
        assert!(gpu.full);
    }

    #[test]
    fn busy_deferral_keeps_cache_and_next_acceptance_resynchronizes_full() {
        let (mut gpu, rx) = fake_gpu();
        let body = Rect { x: 0, y: 0, w: 4, h: 4 };
        let original: Vec<_> = (0..16).collect();
        assert_eq!(gpu.submit(&original, (4, 4), &[body], 0, 1, 4, body, false), Presented::Yes);
        let Command::Frame { mut packet, .. } = rx.try_recv().unwrap() else { panic!("expected frame") };
        let mut cache = Vec::new();
        let mut size = (0, 0);
        apply_packet(&mut packet, &mut cache, &mut size).unwrap();
        let changed = vec![0xaabbccdd; 16];
        // Simulate a hung acquire: no worker completion frees the reservation.
        assert_eq!(gpu.submit(&changed, (4, 4), &[], 1, 1, 4, body, false), Presented::Deferred);
        assert_eq!(cache, original);
        assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
        assert!(gpu.shared.busy.load(Ordering::Acquire));
        // One completion makes the pending host frame admissible. Skipped
        // scrolls require a full authoritative snapshot, never partial replay.
        gpu.shared.busy.store(false, Ordering::Release);
        assert_eq!(gpu.submit(&changed, (4, 4), &[], 2, 1, 4, body, false), Presented::Yes);
        let Command::Frame { mut packet, .. } = rx.try_recv().unwrap() else { panic!("expected frame") };
        assert!(packet.full);
        assert_eq!(packet.scroll, 0);
        apply_packet(&mut packet, &mut cache, &mut size).unwrap();
        assert_eq!(cache, changed); // retains the exact raw u32, including high byte
    }

    #[test]
    fn damage_packet_scroll_matches_cpu_and_preserves_chrome_both_directions() {
        let size = (9, 8);
        let body = Rect { x: 2, y: 2, w: 4, h: 4 };
        let base: Vec<u32> = (0..72).collect();
        for scroll in [-2i64, -1, 1, 2] {
            let mut expected = base.clone();
            for row in 0..body.h {
                for column in 0..body.w {
                    let source = row as i64 + scroll;
                    expected[(body.y + row) * size.0 + body.x + column] = if (0..body.h as i64).contains(&source) {
                        base[(body.y + source as usize) * size.0 + body.x + column]
                    } else {
                        0xff112233
                    };
                }
            }
            let mut packet = pack(&expected, size, &[], scroll, 1, 4, body, false).unwrap();
            assert!(!packet.full);
            assert_eq!(packet.raw.len(), body.w * scroll.unsigned_abs() as usize);
            let mut cache = base.clone();
            let mut cache_size = size;
            apply_packet(&mut packet, &mut cache, &mut cache_size).unwrap();
            assert_eq!(cache, expected);
        }
    }

    #[test]
    fn overlapping_damage_and_metadata_are_capped_to_one_viewport() {
        let pixels = vec![0xabcdef12; 16];
        let body = Rect { x: 0, y: 0, w: 4, h: 4 };
        for damage in [vec![body, body], vec![body; MAX_DAMAGE_RECTS + 1]] {
            let packet = pack(&pixels, (4, 4), &damage, 0, 1, 4, body, false).unwrap();
            assert!(packet.full);
            assert_eq!(packet.damage, [body]);
            assert_eq!(packet.raw, pixels);
        }
        let rect = Rect { x: 1, y: 2, w: 1, h: 1 };
        let packet = pack(&pixels, (4, 4), &[rect], 0, 1, 4, body, false).unwrap();
        assert_eq!(packet.raw, [0xabcdef12]);
        assert!(!packet.full);
    }

    #[test]
    fn idle_facade_does_not_enqueue_and_failure_stays_sticky() {
        let (mut gpu, rx) = fake_gpu();
        gpu.full = false;
        gpu.size = (4, 4);
        let body = Rect { x: 0, y: 0, w: 4, h: 4 };
        assert_eq!(gpu.submit(&[0; 16], (4, 4), &[], 0, 1, 4, body, false), Presented::Yes);
        assert!(!gpu.shared.busy.load(Ordering::Acquire));
        assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
        record_failure(&gpu.shared.failure, "lost device".into());
        for _ in 0..2 {
            assert_eq!(
                gpu.submit(&[0; 16], (4, 4), &[body], 0, 1, 4, body, false),
                Presented::Failed("lost device".into())
            );
        }
        assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
    }

    #[test]
    fn contended_error_mailbox_never_defers_without_a_completion_owner() {
        let (gpu, _rx) = fake_gpu();
        let _writing_error = gpu.shared.failure.lock().unwrap();
        assert!(matches!(gpu.reserve(), Presented::Failed(_)));
        gpu.shared.busy.store(true, Ordering::Release);
        assert_eq!(gpu.reserve(), Presented::Deferred);
    }

    #[test]
    fn viewport_admission_and_disconnected_send_do_not_advance_source() {
        let (mut gpu, rx) = fake_gpu();
        gpu.max_bytes = 4;
        let body = Rect { x: 0, y: 0, w: 4, h: 4 };
        assert!(matches!(gpu.submit(&[0; 16], (4, 4), &[body], 0, 1, 4, body, false), Presented::Failed(_)));
        assert_eq!(gpu.size, (0, 0));
        assert!(!gpu.shared.busy.load(Ordering::Acquire));
        assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
        let (mut gpu, rx) = fake_gpu();
        drop(rx);
        assert!(matches!(gpu.submit(&[0; 16], (4, 4), &[body], 0, 1, 4, body, false), Presented::Failed(_)));
        assert_eq!(gpu.size, (0, 0));
        assert!(gpu.stranded.is_some());
    }

    #[test]
    fn shutdown_deadline_detaches_fake_blocked_owner_without_joining() {
        let (release, blocked) = mpsc::sync_channel::<()>(1);
        let (done, receipt) = mpsc::sync_channel(1);
        let (returned, finished) = mpsc::sync_channel(1);
        let owner = thread::spawn(move || {
            let _ = blocked.recv(); // fake driver hang, no native/GPU activity
            let _ = done.send(());
            let _ = returned.send(());
        });
        assert!(matches!(retire_renderer(owner, receipt, Duration::ZERO), ShutdownStatus::Detached(_)));
        release.try_send(()).unwrap();
        finished.recv_timeout(Duration::from_secs(1)).unwrap();
    }

    #[test]
    fn frame_bounds_reject_short_buffers_and_overflow() {
        assert!(valid_pixels(12, (4, 3), 4096));
        assert!(!valid_pixels(11, (4, 3), 4096));
        assert!(!valid_pixels(12, (0, 3), 4096));
        assert!(!valid_pixels(usize::MAX, (usize::MAX, 2), u32::MAX));
        assert!(!valid_pixels(usize::MAX, (u32::MAX as usize, 1), u32::MAX));
        assert!(!valid_pixels(100, (5, 5), 4));
        assert!(valid_rect(Rect { x: 3, y: 2, w: 1, h: 1 }, (4, 3)));
        assert!(!valid_rect(Rect { x: usize::MAX, y: 0, w: 2, h: 1 }, (4, 3)));
        assert!(!valid_rect(Rect { x: 3, y: 2, w: usize::MAX, h: 1 }, (4, 3)));
    }

    #[test]
    fn scroll_copy_preserves_offset_body_and_direction() {
        let body = Rect { x: 17, y: 23, w: 100, h: 80 };
        assert_eq!(scroll_plan(body, 1, 10, 8), ScrollPlan::Copy { from: 33, to: 23, keep: 70 });
        assert_eq!(scroll_plan(body, -2, 10, 8), ScrollPlan::Copy { from: 23, to: 43, keep: 60 });
        assert_eq!(scroll_plan(body, 0, 10, 8), ScrollPlan::None);
        assert_eq!(scroll_plan(Rect { w: 0, ..body }, 1, 10, 8), ScrollPlan::None);
    }

    #[test]
    fn excessive_or_overflowing_scroll_repaints_body() {
        let body = Rect { x: 17, y: 23, w: 100, h: 80 };
        for scroll in [8, -8, i64::MIN, i64::MAX] {
            assert_eq!(scroll_plan(body, scroll, 10, 8), ScrollPlan::Repaint);
        }
        assert_eq!(scroll_plan(body, 2, usize::MAX, 8), ScrollPlan::Repaint);
        assert_eq!(scroll_plan(body, 1, 0, 8), ScrollPlan::Repaint);
        assert_eq!(scroll_plan(body, 1, 80, 8), ScrollPlan::Repaint);
    }

    #[test]
    fn idle_skips_work_but_resize_damage_and_scroll_draw() {
        assert!(!needs_frame(false, false, &[], 0));
        assert!(!needs_frame(false, false, &[Rect::default()], 0));
        assert!(needs_frame(true, false, &[], 0));
        assert!(needs_frame(false, true, &[], 0));
        assert!(needs_frame(false, false, &[], -1));
        assert!(needs_frame(false, false, &[Rect { x: 0, y: 0, w: 1, h: 1 }], 0));
    }

    #[test]
    fn surface_formats_preserve_sdr_and_reject_incompatible_adapter() {
        use wgpu::TextureFormat::*;
        assert_eq!(surface_format(&[]), None);
        assert_eq!(surface_format(&[Rgba16Float]), None);
        assert_eq!(surface_format(&[Bgra8Unorm, Bgra8UnormSrgb]), Some(Bgra8UnormSrgb));
        assert_eq!(surface_format(&[Rgba8Unorm]), Some(Rgba8Unorm));
    }

    #[test]
    fn first_gpu_error_is_sticky_without_queue_growth() {
        let errors = Mutex::new(None);
        record_failure(&errors, "device lost".into());
        record_failure(&errors, "secondary validation error".into());
        assert_eq!(errors.lock().unwrap().as_deref(), Some("device lost"));
    }
}
