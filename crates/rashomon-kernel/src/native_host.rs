//! Firefox native-messaging entry point for the Firefox-extension Host
//! pivot (see `docs/FIREFOX_EXTENSION_SPIKE.md`). Firefox spawns this
//! binary once per `browser.runtime.connectNative()` call and talks to
//! it over stdin/stdout using Firefox's native messaging protocol: each
//! message is a 4-byte little-endian length prefix followed by that
//! many bytes of UTF-8 JSON, in both directions. Deliberately doesn't
//! touch `rashomon:graph`/`rashomon:browser`/`rashomon:ui` yet — Stage 0
//! only proves the transport by logging whatever it receives and
//! acking it back. Kept as its own `[[bin]]` rather than a CLI flag on
//! `rashomon-kernel`'s existing entry point, which assumes it's already
//! the CEF browser process (see `main.rs`) and must never be launched
//! that way by Firefox.

use std::io::{self, Read, Write};
use std::path::PathBuf;

use directories::ProjectDirs;
use serde_json::Value;

/// Firefox gives native hosts no terminal and discards stderr, so a
/// log file under the same data dir the kernel already uses (see
/// `extensions_config_path` in `lib.rs`) is the only way to observe
/// this process while developing against it.
fn log_path() -> Option<PathBuf> {
    let dirs = ProjectDirs::from("dev", "rashomon", "rashomon")?;
    let data_dir = dirs.data_dir();
    std::fs::create_dir_all(data_dir).ok()?;
    Some(data_dir.join("native-host.log"))
}

fn log(line: &str) {
    if let Some(path) = log_path() {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "{line}");
        }
    }
}

fn read_message(stdin: &mut io::Stdin) -> io::Result<Option<Value>> {
    let mut len_buf = [0u8; 4];
    match stdin.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    stdin.read_exact(&mut buf)?;
    Ok(Some(serde_json::from_slice(&buf).unwrap_or(Value::Null)))
}

fn write_message(stdout: &mut io::Stdout, value: &Value) -> io::Result<()> {
    let bytes = serde_json::to_vec(value).expect("Value always serializes");
    let len = (bytes.len() as u32).to_le_bytes();
    stdout.write_all(&len)?;
    stdout.write_all(&bytes)?;
    stdout.flush()
}

fn main() {
    log(&format!("--- native host started, pid {} ---", std::process::id()));

    let mut stdin = io::stdin();
    let mut stdout = io::stdout();

    loop {
        match read_message(&mut stdin) {
            Ok(Some(message)) => {
                log(&format!("recv: {message}"));
                let ack = serde_json::json!({ "ack": true, "received": message });
                if let Err(e) = write_message(&mut stdout, &ack) {
                    log(&format!("write failed, exiting: {e}"));
                    break;
                }
            }
            Ok(None) => {
                log("stdin closed, exiting");
                break;
            }
            Err(e) => {
                log(&format!("read failed, exiting: {e}"));
                break;
            }
        }
    }
}
