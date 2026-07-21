//! Single-instance plumbing: the first glide process owns a named pipe; later
//! launches hand their folder args over it (→ tabs in the existing window) and
//! exit, instead of piling up one ~100MB egui process per folder open.
use std::path::PathBuf;
use std::sync::mpsc;

use windows::Win32::Foundation::{
    CloseHandle, ERROR_PIPE_BUSY, GENERIC_WRITE, INVALID_HANDLE_VALUE,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_MODE, OPEN_EXISTING, PIPE_ACCESS_INBOUND,
    ReadFile, WriteFile,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
    PIPE_WAIT,
};
use windows::core::PCWSTR;

const PIPE: &str = r"\\.\pipe\glide-single-instance";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Try to hand `paths` to an already-running glide. `true` = delivered, the
/// caller should exit; `false` = no instance, the caller becomes the owner.
/// An empty `paths` still counts as a message ("open a 내 PC tab").
pub fn send_to_existing(paths: &[PathBuf]) -> bool {
    let name = wide(PIPE);
    unsafe {
        for _ in 0..3 {
            match CreateFileW(
                PCWSTR(name.as_ptr()),
                GENERIC_WRITE.0,
                FILE_SHARE_MODE(0),
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            ) {
                Ok(h) => {
                    let msg = paths
                        .iter()
                        .map(|p| p.to_string_lossy().into_owned())
                        .collect::<Vec<_>>()
                        .join("\n");
                    let mut written = 0u32;
                    let ok = WriteFile(h, Some(msg.as_bytes()), Some(&mut written), None).is_ok();
                    let _ = CloseHandle(h);
                    return ok;
                }
                // Owner is busy with another client → brief retry. Any other
                // error (notably file-not-found) means no instance: we own it.
                Err(e) if e.code() == ERROR_PIPE_BUSY.to_hresult() => {
                    std::thread::sleep(std::time::Duration::from_millis(80));
                }
                Err(_) => return false,
            }
        }
    }
    false
}

/// Own the pipe. Each client connection delivers newline-separated dirs
/// (empty payload = "open 내 PC"); they land on `tx` for update() to drain.
pub fn listen(tx: mpsc::Sender<Vec<PathBuf>>, ctx: egui::Context) {
    let name = wide(PIPE);
    std::thread::spawn(move || {
        loop {
            unsafe {
                let pipe = CreateNamedPipeW(
                    PCWSTR(name.as_ptr()),
                    PIPE_ACCESS_INBOUND,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                    1,
                    0,
                    4096,
                    0,
                    None,
                );
                if pipe == INVALID_HANDLE_VALUE {
                    // Name taken (second instance raced us) or pipes exhausted —
                    // retry keeps the owner alive without spinning.
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    continue;
                }
                // Blocks until a client connects; a client that connected between
                // create and here surfaces as an "already connected" error → read anyway.
                let _ = ConnectNamedPipe(pipe, None);
                let mut data = Vec::new();
                let mut buf = [0u8; 1024];
                loop {
                    let mut n = 0u32;
                    if ReadFile(pipe, Some(&mut buf), Some(&mut n), None).is_err() || n == 0 {
                        break;
                    }
                    data.extend_from_slice(&buf[..n as usize]);
                }
                let _ = DisconnectNamedPipe(pipe);
                let _ = CloseHandle(pipe);
                let paths: Vec<PathBuf> = String::from_utf8_lossy(&data)
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .map(PathBuf::from)
                    .collect();
                if tx.send(paths).is_err() {
                    return; // app is gone
                }
                ctx.request_repaint();
            }
        }
    });
}
