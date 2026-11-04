//! Boot the KVM microVM prototype and hold it until SIGTERM / Ctrl-C.
//!
//! Usage: `boot <initramfs.cpio>` — engine paths come from
//! `MYCEL_CH_BIN` / `MYCEL_CH_REMOTE` / `MYCEL_KERNEL` (see
//! `bench/microvm/README.md`). This is the measurement vehicle for the
//! microVM boot benchmark (`bench/microvm/run.sh`).

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

static STOP: AtomicBool = AtomicBool::new(false);

fn main() {
    let initramfs = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: boot <initramfs.cpio>");
        std::process::exit(2);
    });

    let engine = myc_backend::kvm::KvmMicroVm::from_env();
    let mut vm = match engine.launch(Path::new(&initramfs)) {
        Ok(vm) => vm,
        Err(e) => {
            eprintln!("boot: {e}");
            std::process::exit(1);
        }
    };

    match vm.wait_ready(Duration::from_secs(20)) {
        Ok(elapsed) => eprintln!("boot: guest ready in {elapsed:?}"),
        Err(e) => {
            eprintln!("boot: {e}");
            let _ = vm.stop();
            std::process::exit(1);
        }
    }

    ctrlc::set_handler(|| STOP.store(true, Ordering::SeqCst)).expect("ctrlc handler");
    while !STOP.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(20));
    }

    match vm.stop() {
        Ok(()) => eprintln!("boot: stopped cleanly"),
        Err(e) => {
            eprintln!("boot: dirty stop: {e}");
            std::process::exit(1);
        }
    }
}
