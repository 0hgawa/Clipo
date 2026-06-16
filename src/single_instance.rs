//! Single-instance guard.
//!
//! A tray-resident app must not stack copies: clicking the exe or its shortcut
//! while Clipo is already running should reuse the live instance, not spawn a
//! new one. The mechanism is a Windows named pipe.
//!
//! The first launch owns the pipe. A later launch connects, forwards its
//! argument (a media path, or empty for "just show the menu") and exits; the
//! owner reacts on its UI thread.

use std::io::Write as _;
use std::thread;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    GetLastError, ERROR_PIPE_CONNECTED, HANDLE, INVALID_HANDLE_VALUE,
};
use windows::Win32::Storage::FileSystem::{
    ReadFile, FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_INBOUND,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
    PIPE_WAIT,
};

/// Pipe names are machine-global, so namespace by user to avoid colliding with
/// another account's Clipo on the same PC.
fn pipe_path() -> String {
    let user = std::env::var("USERNAME").unwrap_or_default();
    format!(r"\\.\pipe\Clipo.SingleInstance.{user}")
}

pub enum Instance {
    /// We are the first launch; own the pipe and serve later launches.
    Primary(Server),
    /// Another instance is running; our arg was forwarded — the caller exits.
    Secondary,
}

/// Owns the listening end of the pipe. Created by [`acquire`], consumed by
/// [`Server::run`].
pub struct Server {
    pipe: HANDLE,
}

/// Carries the raw pipe handle across the thread boundary. The handle is owned
/// exclusively by the listener thread, so moving it is sound.
struct PipeHandle(HANDLE);
unsafe impl Send for PipeHandle {}

/// Try to own the single-instance pipe. `Some` → we're the primary; `None` →
/// another instance already holds it.
fn create_pipe() -> Option<HANDLE> {
    let path = pipe_path();
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    // FILE_FLAG_FIRST_PIPE_INSTANCE makes this fail if the pipe already exists,
    // so the create itself is the lock — no separate mutex needed.
    // SAFETY: `wide` is a NUL-terminated UTF-16 buffer that outlives the call;
    // the remaining arguments are sizes/flags with no pointers.
    let pipe = unsafe {
        CreateNamedPipeW(
            PCWSTR(wide.as_ptr()),
            PIPE_ACCESS_INBOUND | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            1,    // one instance, reused for the (rare, sequential) relaunches
            0,    // out buffer (we never write back)
            4096, // in buffer
            0,    // default timeout
            None,
        )
    };
    (pipe != INVALID_HANDLE_VALUE).then_some(pipe)
}

/// Become the primary instance, or — if one already exists — forward `arg` to
/// it and report [`Instance::Secondary`] so the caller can exit.
#[must_use]
pub fn acquire(arg: &str) -> Instance {
    create_pipe().map_or_else(
        || {
            forward(&pipe_path(), arg);
            Instance::Secondary
        },
        |pipe| Instance::Primary(Server { pipe }),
    )
}

/// The relaunch right after a self-update: the prior instance is exiting and
/// about to release the pipe, so retry owning it for a short window before
/// falling back to the normal forward-and-exit. Without this the fresh build
/// could race the old one, lose, and exit — leaving no instance running.
#[must_use]
pub fn acquire_after_update(arg: &str) -> Instance {
    for _ in 0..20 {
        if let Some(pipe) = create_pipe() {
            return Instance::Primary(Server { pipe });
        }
        thread::sleep(std::time::Duration::from_millis(150));
    }
    acquire(arg)
}

/// Open the pipe as a file and write the forwarded argument. Best-effort: if the
/// owner is momentarily busy the relaunch simply does nothing.
fn forward(path: &str, arg: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new().write(true).open(path) {
        let _ = f.write_all(arg.as_bytes());
    }
}

impl Server {
    /// Spawn the listener. For every later launch, `on_msg` runs on the listener
    /// thread with the forwarded argument; it should hop to the UI thread.
    pub fn run(self, on_msg: impl Fn(String) + Send + 'static) {
        let handle = PipeHandle(self.pipe);
        thread::spawn(move || {
            // Bind the whole wrapper (not the `.0` field) so the closure captures
            // the Send `PipeHandle`, not the bare non-Send HANDLE.
            let handle = handle;
            let pipe = handle.0;
            loop {
                // SAFETY: `pipe` is the valid handle from `acquire`, owned solely
                // by this thread; ConnectNamedPipe only waits for a client.
                let connected = unsafe { ConnectNamedPipe(pipe, None) };
                // ERROR_PIPE_CONNECTED means a client beat us here — still fine.
                if connected.is_err() && unsafe { GetLastError() } != ERROR_PIPE_CONNECTED {
                    // SAFETY: same valid, thread-owned handle.
                    unsafe {
                        let _ = DisconnectNamedPipe(pipe);
                    }
                    continue;
                }
                let mut buf = [0u8; 4096];
                let mut read = 0u32;
                // SAFETY: `buf`/`read` are live stack locals; the handle is valid.
                let ok = unsafe { ReadFile(pipe, Some(&mut buf), Some(&raw mut read), None) };
                if ok.is_ok() {
                    on_msg(String::from_utf8_lossy(&buf[..read as usize]).into_owned());
                }
                // SAFETY: same valid, thread-owned handle.
                unsafe {
                    let _ = DisconnectNamedPipe(pipe);
                }
            }
        });
    }
}
