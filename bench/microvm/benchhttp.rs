//! benchhttp — la charge de mesure microVM de Mycel (bench/microvm).
//!
//! Compilé en statique musl par mkinitramfs.sh (`rustc -O --target
//! x86_64-unknown-linux-musl`) : EXACTEMENT le même binaire sert dans la
//! microVM (initramfs) et dans le conteneur Docker (FROM scratch) — la
//! comparaison de démarrage est donc à charge identique.
//!
//! Comportement : tout GET reçoit `200 OK` + corps `ok\n` ; un battement de
//! cœur console toutes les 200 ms rend l'exécution invité observable après
//! un restore. Zéro dépendance, std uniquement.

use std::io::{Read, Write};
use std::net::TcpListener;

const RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\n\
Content-Type: text/plain\r\n\
Content-Length: 3\r\n\
Connection: close\r\n\
\r\n\
ok\n";

fn handle(mut stream: impl Read + Write) -> std::io::Result<()> {
    let mut buf = [0u8; 1024];
    if stream.read(&mut buf)? == 0 {
        return Ok(());
    }
    stream.write_all(RESPONSE)?;
    stream.flush()
}

fn main() {
    let listener = match TcpListener::bind("0.0.0.0:8080") {
        Ok(l) => l,
        Err(e) => {
            eprintln!("benchhttp: cannot listen on 0.0.0.0:8080: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("benchhttp: ready on 0.0.0.0:8080");
    std::thread::spawn(|| loop {
        eprintln!("benchhttp: heartbeat");
        std::thread::sleep(std::time::Duration::from_millis(200));
    });
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                if let Err(e) = handle(stream) {
                    eprintln!("benchhttp: conn: {e}");
                }
            }
            Err(e) => eprintln!("benchhttp: accept: {e}"),
        }
    }
}
