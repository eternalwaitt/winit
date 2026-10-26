//! Copy and paste through the seat's `wl_data_device`, the one winit also uses for drag and drop.
//!
//! A client should keep one data device per seat. Some compositors, Hyprland among them, send
//! the selection and drags to only one data device of a client, so a toolkit that binds a second
//! device for its clipboard stops receiving the selection once winit binds one for drops. Winit
//! therefore offers the clipboard on its own device.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, ErrorKind, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use tracing::warn;

use sctk::data_device_manager::data_device::DataDeviceData;
use sctk::data_device_manager::data_source::DataSourceData;
use sctk::data_device_manager::WritePipe;
use sctk::reexports::client::backend::ObjectId;
use sctk::reexports::client::protocol::wl_data_device::WlDataDevice;
use sctk::reexports::client::protocol::wl_data_device_manager::{DndAction, WlDataDeviceManager};
use sctk::reexports::client::protocol::wl_data_source::WlDataSource;
use sctk::reexports::client::protocol::wl_surface::WlSurface;
use sctk::reexports::client::{Connection, Proxy, QueueHandle};

use crate::platform_impl::wayland::state::WinitState;

/// The text types winit offers, and accepts in this order of preference.
const TEXT_MIME_TYPES: [&str; 5] =
    ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain", "TEXT", "STRING"];

/// How long a paste waits for the source to send more data before giving up.
const READ_TIMEOUT: Duration = Duration::from_secs(2);

/// Every live clipboard, so one can be found from the display handle a toolkit is given.
static CLIPBOARDS: Mutex<Vec<Weak<ClipboardState>>> = Mutex::new(Vec::new());

/// The clipboard of one event loop, shared between the loop and its users.
#[derive(Debug)]
pub struct ClipboardState {
    connection: Connection,
    queue_handle: QueueHandle<WinitState>,
    manager: Option<WlDataDeviceManager>,
    /// The `wl_display` pointer, to match a raw display handle.
    display: usize,
    /// A marker type offered with our own selection, so we recognise it when it comes back.
    marker: String,
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    seats: Vec<SeatClipboard>,
    /// The seat that last gave one of our windows input.
    active_seat: Option<ObjectId>,
    /// The selection we offer, until another client takes the selection.
    owned: Option<OwnedSelection>,
    drag: Option<OwnedDrag>,
}

#[derive(Debug)]
struct SeatClipboard {
    seat: ObjectId,
    device: WlDataDevice,
    /// The serial of the latest input event on this seat, which setting the selection needs.
    serial: Option<u32>,
    pressed: HashMap<u32, (ObjectId, u32)>,
}

#[derive(Debug)]
struct OwnedSelection {
    source: WlDataSource,
    text: Arc<str>,
}

struct OwnedDrag {
    source: WlDataSource,
    payload: Arc<str>,
    completed: Option<Box<dyn FnOnce(bool) + Send>>,
    copied: bool,
}

impl std::fmt::Debug for OwnedDrag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnedDrag").field("source", &self.source).finish_non_exhaustive()
    }
}

/// Percent-encode paths without assuming Unix filenames are UTF-8.
fn file_uri_list(files: &[std::path::PathBuf]) -> io::Result<String> {
    const PATH: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
        .remove(b'/')
        .remove(b'-')
        .remove(b'.')
        .remove(b'_')
        .remove(b'~');
    if files.is_empty() {
        return Err(io::Error::new(ErrorKind::InvalidInput, "no files to drag"));
    }
    let mut text = String::new();
    for path in files {
        if !path.is_absolute() {
            return Err(io::Error::new(ErrorKind::InvalidInput, "file paths must be absolute"));
        }
        text.push_str("file://");
        text.push_str(
            &percent_encoding::percent_encode(path.as_os_str().as_bytes(), PATH).to_string(),
        );
        text.push_str("\r\n");
    }
    Ok(text)
}

impl ClipboardState {
    pub fn new(
        connection: &Connection,
        queue_handle: &QueueHandle<WinitState>,
        manager: Option<WlDataDeviceManager>,
    ) -> Arc<Self> {
        let display = connection.display().id().as_ptr() as usize;
        let state = Arc::new(Self {
            connection: connection.clone(),
            queue_handle: queue_handle.clone(),
            manager,
            display,
            marker: format!(
                "application/x-winit-selection;pid={};display={display:x}",
                std::process::id()
            ),
            inner: Mutex::new(Inner::default()),
        });

        let mut clipboards = CLIPBOARDS.lock().unwrap();
        clipboards.retain(|clipboard| clipboard.strong_count() > 0);
        clipboards.push(Arc::downgrade(&state));
        state
    }

    /// The clipboard of the event loop connected through this `wl_display`.
    pub fn for_display(display: *mut std::ffi::c_void) -> Option<Arc<Self>> {
        let clipboards = CLIPBOARDS.lock().unwrap();
        clipboards
            .iter()
            .filter_map(Weak::upgrade)
            .find(|clipboard| clipboard.display == display as usize)
    }

    pub fn add_seat(&self, seat: ObjectId, device: WlDataDevice) {
        let mut inner = self.inner.lock().unwrap();
        inner.seats.retain(|entry| entry.seat != seat);
        inner.seats.push(SeatClipboard { seat, device, serial: None, pressed: HashMap::new() });
    }

    pub fn remove_seat(&self, seat: &ObjectId) {
        let mut inner = self.inner.lock().unwrap();
        inner.seats.retain(|entry| &entry.seat != seat);
        if inner.active_seat.as_ref() == Some(seat) {
            inner.active_seat = None;
        }
    }

    /// Records input on a seat: its serial, and that this seat is the one in use.
    pub fn note_input(&self, seat: &ObjectId, serial: u32) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(entry) = inner.seats.iter_mut().find(|entry| &entry.seat == seat) {
            entry.serial = Some(serial);
            inner.active_seat = Some(seat.clone());
        }
    }

    /// A drag needs the current pointer grab, not a clipboard or keyboard serial.
    pub fn note_pointer_button(
        &self,
        seat: &ObjectId,
        surface: &WlSurface,
        button: u32,
        serial: u32,
        pressed: bool,
    ) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(entry) = inner.seats.iter_mut().find(|entry| &entry.seat == seat) {
            if pressed {
                entry.pressed.insert(button, (surface.id(), serial));
            } else {
                entry.pressed.remove(&button);
            }
        }
    }

    pub fn start_file_drag(
        &self,
        surface: &WlSurface,
        files: &[std::path::PathBuf],
        completed: Box<dyn FnOnce(bool) + Send>,
    ) -> io::Result<()> {
        let payload: Arc<str> = file_uri_list(files)?.into();
        let manager = self
            .manager
            .as_ref()
            .ok_or_else(|| io::Error::new(ErrorKind::Unsupported, "no data device manager"))?;
        if manager.version() < 3 {
            return Err(io::Error::new(
                ErrorKind::Unsupported,
                "copy-only dragging requires data device version 3",
            ));
        }
        let mut inner = self.inner.lock().unwrap();
        if inner.drag.is_some() {
            return Err(io::Error::new(ErrorKind::WouldBlock, "a file drag is already active"));
        }
        let (entry, serial) = inner
            .seats
            .iter_mut()
            .find_map(|entry| {
                let (origin, serial) = entry.pressed.get(&0x110)?;
                if *origin != surface.id() {
                    return None;
                }
                let serial = *serial;
                Some((entry, serial))
            })
            .ok_or_else(|| {
                io::Error::new(
                    ErrorKind::PermissionDenied,
                    "start a drag while the primary pointer button is held on this window",
                )
            })?;
        let source = manager.create_data_source(&self.queue_handle, DataSourceData::default());
        source.offer("text/uri-list".to_owned());
        source.set_actions(DndAction::Copy);
        entry.device.start_drag(Some(&source), surface, None, serial);
        // The compositor owns this grab now. Its serial cannot start another drag.
        entry.pressed.remove(&0x110);
        inner.drag = Some(OwnedDrag { source, payload, completed: Some(completed), copied: false });
        drop(inner);
        if let Err(error) = self.connection.flush() {
            if let Some(drag) = self.inner.lock().unwrap().drag.take() {
                drag.source.destroy();
            }
            return Err(io::Error::new(ErrorKind::Other, error));
        }
        Ok(())
    }

    pub fn drag_action(&self, source: &WlDataSource, action: DndAction) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(drag) = inner.drag.as_mut().filter(|drag| &drag.source == source) {
            drag.copied = action == DndAction::Copy;
        }
    }

    pub fn finish_drag(&self, source: &WlDataSource, finished: bool) -> bool {
        let drag = {
            let mut inner = self.inner.lock().unwrap();
            if !inner.drag.as_ref().is_some_and(|drag| &drag.source == source) {
                return false;
            }
            inner.drag.take().unwrap()
        };
        drag.source.destroy();
        if let Some(completed) = drag.completed {
            completed(finished && drag.copied);
        }
        true
    }

    /// The text of our own selection for a source the compositor asks to send.
    pub fn owned_text(&self, source: &WlDataSource) -> Option<Arc<str>> {
        let inner = self.inner.lock().unwrap();
        inner.owned.as_ref().filter(|owned| &owned.source == source).map(|owned| owned.text.clone())
    }

    /// Forgets our selection once another client has taken it.
    pub fn source_cancelled(&self, source: &WlDataSource) {
        if self.finish_drag(source, false) {
            return;
        }
        let mut inner = self.inner.lock().unwrap();
        if inner.owned.as_ref().is_some_and(|owned| &owned.source == source) {
            inner.owned = None;
        }
        source.destroy();
    }

    /// Sends our selection's text through the pipe the compositor gave us.
    pub fn send(&self, source: &WlDataSource, mime_type: &str, pipe: WritePipe) {
        let drag = {
            let inner = self.inner.lock().unwrap();
            inner
                .drag
                .as_ref()
                .filter(|drag| &drag.source == source && mime_type == "text/uri-list")
                .map(|drag| drag.payload.clone())
        };
        let text = match drag {
            Some(payload) => payload,
            None => {
                if !TEXT_MIME_TYPES.contains(&mime_type) && mime_type != self.marker {
                    return;
                }
                let Some(text) = self.owned_text(source) else { return };
                text
            },
        };
        let mut file = File::from(OwnedFd::from(pipe));
        // The reader may be slow, or be this very process: write off the event loop.
        let spawned = std::thread::Builder::new().name("winit-clipboard".into()).spawn(move || {
            if let Err(error) = file.write_all(text.as_bytes()) {
                if error.kind() != ErrorKind::BrokenPipe {
                    warn!("Failed to send the clipboard: {error}");
                }
            }
        });
        if let Err(error) = spawned {
            warn!("Failed to send the clipboard: {error}");
        }
    }

    fn active_device(inner: &Inner) -> Option<&SeatClipboard> {
        inner
            .active_seat
            .as_ref()
            .and_then(|seat| inner.seats.iter().find(|entry| &entry.seat == seat))
            .or_else(|| inner.seats.first())
    }

    pub fn load_text(&self) -> io::Result<String> {
        let inner = self.inner.lock().unwrap();
        let offer = Self::active_device(&inner)
            .and_then(|entry| entry.device.data::<DataDeviceData>())
            .and_then(|data| data.selection_offer());

        let Some(offer) = offer else {
            // The compositor hasn't told us about a selection, but one we set is still ours.
            return match &inner.owned {
                Some(owned) => Ok(owned.text.to_string()),
                None => Err(io::Error::new(ErrorKind::NotFound, "the clipboard is empty")),
            };
        };

        let types = offer.with_mime_types(|types| types.to_vec());
        if types.iter().any(|mime| mime == &self.marker) {
            // Our own selection: reading it through the compositor would wait on this thread.
            if let Some(owned) = &inner.owned {
                return Ok(owned.text.to_string());
            }
        }

        let Some(mime) = TEXT_MIME_TYPES.iter().find(|mime| types.iter().any(|t| t == *mime))
        else {
            return Err(io::Error::new(ErrorKind::InvalidData, "the clipboard holds no text"));
        };

        let pipe = offer.receive(mime.to_string()).map_err(|error| match error {
            sctk::data_device_manager::data_offer::DataOfferError::Io(error) => error,
            other => io::Error::new(ErrorKind::Other, other.to_string()),
        })?;
        drop(inner);
        // The source only writes once the compositor has our request.
        self.connection.flush().map_err(|error| io::Error::new(ErrorKind::Other, error))?;

        let bytes = read_with_timeout(File::from(OwnedFd::from(pipe)))?;
        let text = String::from_utf8_lossy(&bytes);
        // `text/*` types use CRLF line endings, but applications expect LF.
        Ok(text.replace("\r\n", "\n").replace('\r', "\n"))
    }

    pub fn store_text(&self, text: String) -> io::Result<()> {
        let Some(manager) = &self.manager else {
            return Err(io::Error::new(ErrorKind::Unsupported, "no data device manager"));
        };
        let mut inner = self.inner.lock().unwrap();
        let Some(entry) = Self::active_device(&inner) else {
            return Err(io::Error::new(ErrorKind::NotFound, "no seat"));
        };
        let Some(serial) = entry.serial else {
            return Err(io::Error::new(ErrorKind::Other, "no input to set the clipboard for"));
        };

        let source = manager.create_data_source(&self.queue_handle, DataSourceData::default());
        for mime in TEXT_MIME_TYPES {
            source.offer(mime.to_owned());
        }
        source.offer(self.marker.clone());
        entry.device.set_selection(Some(&source), serial);

        if let Some(previous) = inner.owned.replace(OwnedSelection { source, text: text.into() }) {
            previous.source.destroy();
        }
        drop(inner);
        self.connection.flush().map_err(|error| io::Error::new(ErrorKind::Other, error))
    }
}

impl Drop for ClipboardState {
    fn drop(&mut self) {
        if let Some(drag) = self.inner.get_mut().unwrap().drag.take() {
            drag.source.destroy();
            if let Some(completed) = drag.completed {
                completed(false);
            }
        }
        if let Some(owned) = self.inner.get_mut().unwrap().owned.take() {
            owned.source.destroy();
        }
    }
}

/// Reads a pipe to its end, giving up if the writer stays silent for [`READ_TIMEOUT`].
fn read_with_timeout(mut file: File) -> io::Result<Vec<u8>> {
    let fd = file.as_raw_fd();
    // SAFETY: `fd` is a valid descriptor owned by `file`.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }

    let mut bytes = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match file.read(&mut chunk) {
            Ok(0) => return Ok(bytes),
            Ok(read) => bytes.extend_from_slice(&chunk[..read]),
            Err(error) if error.kind() == ErrorKind::Interrupted => {},
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                let mut poll = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
                // SAFETY: `poll` points to one valid `pollfd`.
                let ready = unsafe { libc::poll(&mut poll, 1, READ_TIMEOUT.as_millis() as i32) };
                if ready == 0 {
                    return Err(io::Error::new(
                        ErrorKind::TimedOut,
                        "the clipboard's owner did not send it",
                    ));
                }
                if ready < 0 {
                    let error = io::Error::last_os_error();
                    if error.kind() != ErrorKind::Interrupted {
                        return Err(error);
                    }
                }
            },
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod drag_tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;

    #[test]
    fn outbound_paths_are_absolute_and_uri_delimiters_are_escaped() {
        let list = file_uri_list(&[std::path::PathBuf::from("/tmp/a b#c%?.pdf")]).unwrap();
        assert_eq!(list, "file:///tmp/a%20b%23c%25%3F.pdf\r\n");
        assert!(file_uri_list(&[]).is_err());
        assert!(file_uri_list(&[std::path::PathBuf::from("relative.pdf")]).is_err());
    }

    #[test]
    fn uri_lists_keep_non_utf8_filenames_and_multiple_entries() {
        let path = std::path::PathBuf::from(std::ffi::OsString::from_vec(b"/tmp/\xff".to_vec()));
        assert_eq!(
            file_uri_list(&[path, "/tmp/second".into()]).unwrap(),
            "file:///tmp/%FF\r\nfile:///tmp/second\r\n"
        );
    }
}
