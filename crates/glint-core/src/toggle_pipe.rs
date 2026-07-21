//! Toggle signal pipe: glide-shell's Win-key hook (and anything else) pokes
//! this to show/hide the palette without a keyboard shortcut. Payload is
//! ignored — a connection IS the signal. Name is duplicated on the shell
//! side (glide-shell winkey.rs), which must not depend on this crate.

use windows::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows::Win32::Storage::FileSystem::{PIPE_ACCESS_INBOUND, ReadFile};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
    PIPE_WAIT,
};
use windows::core::PCWSTR;

pub const PIPE: &str = r"\\.\pipe\glint-toggle";

/// Own the pipe on a background thread; call `on_signal` once per client
/// connection. Same ownership loop as glide's single-instance pipe.
pub fn listen(on_signal: impl Fn() + Send + 'static) {
    let name: Vec<u16> = PIPE.encode_utf16().chain(std::iter::once(0)).collect();
    std::thread::spawn(move || {
        loop {
            unsafe {
                let pipe = CreateNamedPipeW(
                    PCWSTR(name.as_ptr()),
                    PIPE_ACCESS_INBOUND,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                    1,
                    0,
                    256,
                    0,
                    None,
                );
                if pipe == INVALID_HANDLE_VALUE {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    continue;
                }
                let _ = ConnectNamedPipe(pipe, None);
                // Drain whatever the client wrote; content is irrelevant.
                let mut buf = [0u8; 64];
                let mut n = 0u32;
                while ReadFile(pipe, Some(&mut buf), Some(&mut n), None).is_ok() && n > 0 {}
                let _ = DisconnectNamedPipe(pipe);
                let _ = CloseHandle(pipe);
                on_signal();
            }
        }
    });
}
