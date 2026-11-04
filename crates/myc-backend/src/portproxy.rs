//! Userland TCP relay for published ports — the `docker-proxy` pattern.
//!
//! On paper, publishing `127.0.0.1:<host>` into a VM only needs an iptables
//! `nat OUTPUT` DNAT rule. In practice that silently does nothing under
//! WSL2's kernel (the SYN never reaches the bridge — martian-source
//! handling around loopback; verified 2026-11-04 with tcpdump on `br-myc`,
//! while a direct connection to the guest IP worked). Docker itself falls
//! back to exactly this userspace relay for loopback publishes, so we do
//! the same: bind `127.0.0.1:<host>` and splice bytes to
//! `<guest_ip>:<container>`. The `nat PREROUTING` DNAT (external clients)
//! stays in `kvm.rs`; the two mechanisms compose without interference.

use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// One running relay: `127.0.0.1:host_port` → `target`.
///
/// `stop()` releases the listening socket; established connections drain
/// to completion (a shutdown must not cut an in-flight response — the VM
/// teardown happens right after).
pub struct PortProxy {
    /// The port actually bound (useful when constructed with 0).
    pub host_port: u16,
    pub target: SocketAddr,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl PortProxy {
    /// Bind `127.0.0.1:host_port` (0 = any free port) and start relaying.
    /// Fails if the port is already taken — the caller surfaces that as a
    /// run error.
    pub fn start(host_port: u16, target: SocketAddr) -> io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", host_port))?;
        let bound_port = listener.local_addr()?.port();
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let handle = thread::Builder::new()
            .name(format!("myc-proxy-{bound_port}"))
            .spawn(move || {
                while !flag.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((sock, _)) => {
                            let _ = sock.set_nodelay(true);
                            // Detached: lives exactly as long as both ends.
                            let _ = thread::Builder::new()
                                .name("myc-proxy-conn".into())
                                .spawn(move || relay(sock, target));
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            })?;
        Ok(Self {
            host_port: bound_port,
            target,
            stop,
            handle: Some(handle),
        })
    }

    /// Stop accepting and release the port (idempotent).
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Copy bytes both ways until either side closes. Each direction is a
/// thread; the finished direction shuts the other one's write side down so
/// the pair always terminates with proper half-close semantics.
fn relay(client: TcpStream, target: SocketAddr) {
    let Ok(upstream) = TcpStream::connect_timeout(&target, Duration::from_secs(5)) else {
        // Unreachable guest: behave like a kernel RST.
        let _ = client.shutdown(Shutdown::Both);
        return;
    };
    let _ = upstream.set_nodelay(true);
    let Ok(mut c2u) = client.try_clone() else {
        return;
    };
    let Ok(mut up_read) = upstream.try_clone() else {
        return;
    };
    let mut up_write = upstream;
    let mut client = client;
    let t1 = thread::spawn(move || pump(&mut c2u, &mut up_write));
    let t2 = thread::spawn(move || pump(&mut up_read, &mut client));
    let _ = t1.join();
    let _ = t2.join();
}

fn pump(src: &mut TcpStream, dst: &mut TcpStream) {
    let mut buf = [0u8; 8192];
    loop {
        match src.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if dst.write_all(&buf[..n]).is_err() {
                    break;
                }
                // Errors on close (peer raced us) are not failures.
                let _ = dst.flush();
            }
        }
    }
    // Half-close: signal EOF downstream so the peer's read() returns 0.
    let _ = dst.shutdown(Shutdown::Write);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relays_round_trips_and_releases_the_port() {
        // Echo server on a random loopback port — one connection per client
        // request, so accept in a LOOP (the single-accept version failed the
        // second relay: the proxy happily connects to a backlog-queued echo
        // that nobody serves — test bug, not an implementation bug).
        let echo = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let echo_addr = echo.local_addr().unwrap();
        thread::spawn(move || {
            for mut sock in echo.incoming().flatten() {
                let mut buf = [0u8; 64];
                if let Ok(n) = sock.read(&mut buf) {
                    let _ = sock.write_all(&buf[..n]);
                }
            }
        });

        let mut proxy = PortProxy::start(0, echo_addr).unwrap();
        let port = proxy.host_port;
        assert_ne!(port, 0);

        let ask = |msg: &[u8]| {
            let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
            c.write_all(msg).unwrap();
            let mut out = [0u8; 64];
            let n = c.read(&mut out).unwrap();
            out[..n].to_vec()
        };
        assert_eq!(ask(b"ping-vm"), b"ping-vm".to_vec());
        assert_eq!(ask(b"second"), b"second".to_vec());

        proxy.stop();
        // After stop() the port is released (bind succeeds again).
        assert!(
            TcpListener::bind(("127.0.0.1", port)).is_ok(),
            "proxy port not released after stop()"
        );
    }
}
