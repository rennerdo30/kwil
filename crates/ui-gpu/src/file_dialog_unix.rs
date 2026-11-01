//! rfd uses NSSavePanel/NSOpenPanel on macOS and the desktop portal on Linux.
//! Construct futures on the GUI thread, await off-thread, and feed the same
//! identity-checked Gate used by Windows. No native dialog is used by tests.
use super::{Event, Outcome, Port, Request};
use kwil_view_model::FilePromptKind;
use std::{
    future::Future,
    io,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
};

type Reply = Pin<Box<dyn Future<Output = Option<rfd::FileHandle>> + Send>>;
type Job = (Request, Reply);

pub(crate) struct Worker {
    dialog: rfd::AsyncFileDialog,
    sender: Option<mpsc::Sender<Job>>,
    cancelled: Arc<AtomicBool>,
    busy: Arc<AtomicBool>,
}

impl Worker {
    pub fn set_parent(&mut self, parent: &winit::window::Window) {
        self.dialog = rfd::AsyncFileDialog::new().set_parent(parent);
    }
    pub fn spawn(complete: impl Fn(Event) + Send + 'static) -> io::Result<Self> {
        let (sender, receiver) = mpsc::channel::<Job>();
        let cancelled = Arc::new(AtomicBool::new(false));
        let busy = Arc::new(AtomicBool::new(false));
        let cancel = cancelled.clone();
        let occupied = busy.clone();
        thread::Builder::new().name("kwil-file-dialog".into()).spawn(move || {
            complete(Event::Ready);
            while let Ok((request, reply)) = receiver.recv() {
                let path = pollster::block_on(reply).map(|file| file.path().to_path_buf());
                let outcome = match path {
                    Some(path) if !cancel.load(Ordering::Acquire) => Outcome::Accepted(path),
                    _ => Outcome::Cancelled,
                };
                occupied.store(false, Ordering::Release);
                complete(Event::Completed { ticket: *request.ticket, outcome });
            }
            complete(Event::Stopped(Ok(())));
        })?;
        Ok(Self { dialog: rfd::AsyncFileDialog::new(), sender: Some(sender), cancelled, busy })
    }
}

impl Port for Worker {
    fn submit(&mut self, request: Request) -> Result<(), (Request, String)> {
        let Some(sender) = &self.sender else { return Err((request, "file dialog stopped".into())) };
        if self.busy.swap(true, Ordering::AcqRel) {
            return Err((request, "file dialog already active".into()));
        }
        self.cancelled.store(false, Ordering::Release);
        let mut dialog = self.dialog.clone().set_directory(&request.directory);
        if let Some(name) = &request.suggested_name {
            dialog = dialog.set_file_name(name.to_string_lossy());
        }
        // rfd constructs the AppKit sheet here, on winit's main thread. Its
        // future is awaited on the worker so the GUI keeps pumping events.
        let future: Reply = match request.ticket.kind() {
            FilePromptKind::SaveAs => Box::pin(dialog.save_file()),
            FilePromptKind::Open => Box::pin(dialog.pick_file()),
            _ => unreachable!("native ticket is Open or SaveAs"),
        };
        if let Err(error) = sender.send((request, future)) {
            self.busy.store(false, Ordering::Release);
            return Err((error.0.0, "file dialog worker stopped".into()));
        }
        Ok(())
    }
    fn cancel(&mut self) {
        // rfd does not expose programmatic dismissal. Retain the native owner
        // until the user dismisses the panel, and reject its late result.
        self.cancelled.store(true, Ordering::Release);
    }
    fn shutdown(&mut self) {
        self.cancel();
        self.sender = None;
        // Stopped follows the active panel's terminal receipt, never precedes it.
    }
}
