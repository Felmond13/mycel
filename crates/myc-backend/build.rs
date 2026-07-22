//! Embeds the Linux "twin" binary into Windows builds of myc.
//!
//! When cross-building (or native-building) for Windows, set
//! `MYCEL_LINUX_BINARY` to a Linux release build of `myc`; its bytes are
//! embedded into `myc.exe` (via `include_bytes!`), which makes the .exe
//! fully self-contained: on first run it provisions the twin into WSL2 at
//! `~/.mycel-bin/myc` and proxies every command to it.
//!
//! When the variable is unset (every Linux/macOS build, plus Windows dev
//! builds), an empty placeholder is embedded and provisioning falls back to
//! the `MYCEL_WSL_BINARY` runtime override.

use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=MYCEL_LINUX_BINARY");
    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR is set by cargo"));
    let dest = out.join("myc-linux-twin");
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    match env::var("MYCEL_LINUX_BINARY") {
        Ok(src) if target_os == "windows" && !src.is_empty() => {
            println!("cargo:rerun-if-changed={src}");
            fs::copy(&src, &dest).unwrap_or_else(|e| {
                panic!("cannot embed MYCEL_LINUX_BINARY ({src}): {e}");
            });
        }
        _ => {
            // Placeholder so include_bytes! always compiles.
            fs::write(&dest, []).expect("write empty twin placeholder");
        }
    }
}
