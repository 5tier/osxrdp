use std::io::Write as _;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ironrdp_cliprdr::backend::{CliprdrBackend, CliprdrBackendFactory, ClipboardMessage};
use ironrdp_cliprdr::pdu::{
    ClipboardFormat, ClipboardFormatId, ClipboardGeneralCapabilityFlags, FileContentsRequest,
    FileContentsResponse, FormatDataRequest, FormatDataResponse, LockDataId,
};
use ironrdp_core::{AsAny, IntoOwned};
use ironrdp_server::{CliprdrServerFactory, ServerEvent, ServerEventSender};
use tokio::sync::mpsc;
use tracing::{debug, warn};

fn read_mac_clipboard() -> Option<String> {
    let out = Command::new("pbpaste").output().ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        None
    }
}

fn write_mac_clipboard(text: &str) {
    let mut child = match Command::new("pbcopy").stdin(Stdio::piped()).spawn() {
        Ok(c) => c,
        Err(e) => {
            warn!("pbcopy spawn failed: {e}");
            return;
        }
    };
    if let Some(stdin) = child.stdin.as_mut() {
        let _ = stdin.write_all(text.as_bytes());
    }
    let _ = child.wait();
}

fn send_initiate_copy(sender: &mpsc::UnboundedSender<ServerEvent>) {
    let formats = vec![ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)];
    let _ = sender.send(ServerEvent::Clipboard(ClipboardMessage::SendInitiateCopy(formats)));
}

// ── Backend ────────────────────────────────────────────────────────────────

struct MacClipboardBackend {
    sender: mpsc::UnboundedSender<ServerEvent>,
    stop: Arc<AtomicBool>,
    /// Shared with the polling thread; updated when we write to the clipboard
    /// ourselves so the poller doesn't echo it back to the RDP client.
    known_clipboard: Arc<Mutex<String>>,
}

impl Drop for MacClipboardBackend {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl std::fmt::Debug for MacClipboardBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MacClipboardBackend").finish()
    }
}

impl AsAny for MacClipboardBackend {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

impl CliprdrBackend for MacClipboardBackend {
    fn temporary_directory(&self) -> &str {
        "/tmp"
    }

    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        ClipboardGeneralCapabilityFlags::USE_LONG_FORMAT_NAMES
    }

    fn on_ready(&mut self) {
        debug!("Clipboard channel ready; advertising current macOS clipboard");
        self.advertise_local_clipboard();
    }

    fn on_request_format_list(&mut self) {
        self.advertise_local_clipboard();
    }

    fn on_process_negotiated_capabilities(&mut self, _caps: ClipboardGeneralCapabilityFlags) {}

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        // Proactively pull CF_UNICODETEXT so it lands in the macOS clipboard immediately.
        let has_unicode = available_formats
            .iter()
            .any(|f| f.id() == ClipboardFormatId::CF_UNICODETEXT);
        if has_unicode {
            let _ = self.sender.send(ServerEvent::Clipboard(
                ClipboardMessage::SendInitiatePaste(ClipboardFormatId::CF_UNICODETEXT),
            ));
        }
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        let text = (request.format == ClipboardFormatId::CF_UNICODETEXT)
            .then(read_mac_clipboard)
            .flatten();
        let response = match text {
            Some(ref s) => FormatDataResponse::new_unicode_string(s).into_owned(),
            None => FormatDataResponse::new_error().into_owned(),
        };
        let _ = self
            .sender
            .send(ServerEvent::Clipboard(ClipboardMessage::SendFormatData(response)));
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        if response.is_error() {
            return;
        }
        match response.to_unicode_string() {
            Ok(text) if !text.is_empty() => {
                write_mac_clipboard(&text);
                // Suppress the inevitable poll echo.
                *self.known_clipboard.lock().unwrap() = text;
            }
            _ => {}
        }
    }

    fn on_file_contents_request(&mut self, _request: FileContentsRequest) {}
    fn on_file_contents_response(&mut self, _response: FileContentsResponse<'_>) {}
    fn on_lock(&mut self, _data_id: LockDataId) {}
    fn on_unlock(&mut self, _data_id: LockDataId) {}
}

impl MacClipboardBackend {
    fn advertise_local_clipboard(&self) {
        if read_mac_clipboard().is_some_and(|t| !t.is_empty()) {
            send_initiate_copy(&self.sender);
        }
    }
}

// ── Factory ────────────────────────────────────────────────────────────────

pub struct MacClipboardFactory {
    sender: Option<mpsc::UnboundedSender<ServerEvent>>,
}

impl MacClipboardFactory {
    pub fn new() -> Self {
        Self { sender: None }
    }
}

impl ServerEventSender for MacClipboardFactory {
    fn set_sender(&mut self, sender: mpsc::UnboundedSender<ServerEvent>) {
        self.sender = Some(sender);
    }
}

impl CliprdrBackendFactory for MacClipboardFactory {
    fn build_cliprdr_backend(&self) -> Box<dyn CliprdrBackend> {
        let sender = self
            .sender
            .clone()
            .expect("set_sender called before build_cliprdr_backend");

        let stop = Arc::new(AtomicBool::new(false));
        let known_clipboard = Arc::new(Mutex::new(read_mac_clipboard().unwrap_or_default()));

        let stop_bg = stop.clone();
        let known_bg = known_clipboard.clone();
        let sender_bg = sender.clone();
        std::thread::spawn(move || {
            while !stop_bg.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(500));
                let current = read_mac_clipboard().unwrap_or_default();
                let mut known = known_bg.lock().unwrap();
                if current != *known && !current.is_empty() {
                    *known = current;
                    send_initiate_copy(&sender_bg);
                }
            }
        });

        Box::new(MacClipboardBackend {
            sender,
            stop,
            known_clipboard,
        })
    }
}

impl CliprdrServerFactory for MacClipboardFactory {}
