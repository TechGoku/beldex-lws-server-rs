//! New-block notifications over beldexd's OxenMQ socket.
//!
//! beldexd announces every new block to subscribers: a client connects a
//! ZeroMQ DEALER socket (by default to `ipc://<data-dir>/beldexd.sock`), sends
//! `HI` and receives `HELLO`, then requests `sub.block` and is sent
//! `[notify.block, height, hash]` for each block. Subscriptions expire after
//! 30 minutes; they are renewed every 10 seconds, and a renewal answered `OK`
//! instead of `ALREADY` means the daemon restarted and forgot us (ZeroMQ
//! reconnects underneath silently), so the scanner is woken to catch up.
//!
//! Notifications only shorten the wait: the scanner still polls, so a missed
//! notification, a restarted daemon or an unreachable socket costs at most one
//! poll interval. Each endpoint gets its own thread, which reconnects with
//! backoff for as long as the process runs.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::watch;
use tracing::{debug, info, warn};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const RESUBSCRIBE_EVERY: Duration = Duration::from_secs(10);
/// No message at all (not even a subscription reply) for this long means the
/// connection is dead, whatever ZeroMQ thinks.
const SILENCE_LIMIT: Duration = Duration::from_secs(150);

/// Shared view of what the subscriptions have heard.
#[derive(Default)]
pub struct NotifyState {
    /// Highest block height announced by any endpoint (0 = none yet).
    pub height: AtomicU64,
    pub notifications: AtomicU64,
    pub connected: AtomicBool,
}

/// Starts one subscriber thread per endpoint. The returned receiver changes
/// whenever a new, higher block is announced.
pub fn subscribe(endpoints: &[String], stop: Arc<AtomicBool>) -> (watch::Receiver<u64>, Arc<NotifyState>) {
    let (tx, rx) = watch::channel(0u64);
    let tx = Arc::new(tx);
    let state = Arc::new(NotifyState::default());
    let context = zmq::Context::new();
    for endpoint in endpoints {
        let (endpoint, tx, state, stop, context) =
            (endpoint.clone(), tx.clone(), state.clone(), stop.clone(), context.clone());
        let _ = std::thread::Builder::new()
            .name("lws-notify".into())
            .spawn(move || run(&context, &endpoint, &tx, &state, &stop));
    }
    (rx, state)
}

fn run(context: &zmq::Context, endpoint: &str, tx: &watch::Sender<u64>, state: &NotifyState, stop: &AtomicBool) {
    let mut backoff = Duration::from_secs(1);
    while !stop.load(Ordering::Relaxed) {
        match session(context, endpoint, tx, state, stop) {
            Ok(()) => backoff = Duration::from_secs(1),
            Err(e) => {
                state.connected.store(false, Ordering::Relaxed);
                warn!(%endpoint, error = %e, retry_in_s = backoff.as_secs(), "block notifications unavailable");
            }
        }
        let until = Instant::now() + backoff;
        while Instant::now() < until && !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(200));
        }
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

fn recv(socket: &zmq::Socket) -> Result<Option<Vec<Vec<u8>>>, String> {
    match socket.recv_multipart(0) {
        Ok(parts) => Ok(Some(parts)),
        Err(zmq::Error::EAGAIN) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// A fresh reply tag, so replies can be matched to requests.
fn tag() -> Vec<u8> {
    rand::random::<[u8; 8]>().to_vec()
}

/// One connection: handshake, subscribe, and relay notifications until the
/// connection goes quiet or fails.
fn session(
    context: &zmq::Context,
    endpoint: &str,
    tx: &watch::Sender<u64>,
    state: &NotifyState,
    stop: &AtomicBool,
) -> Result<(), String> {
    let socket = context.socket(zmq::DEALER).map_err(|e| e.to_string())?;
    socket.set_linger(0).map_err(|e| e.to_string())?;
    socket.set_rcvtimeo(1000).map_err(|e| e.to_string())?;
    socket.set_maxmsgsize(1 << 20).map_err(|e| e.to_string())?;
    socket.connect(endpoint).map_err(|e| e.to_string())?;

    socket.send("HI", 0).map_err(|e| e.to_string())?;
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    loop {
        if Instant::now() > deadline {
            return Err("no HELLO from the daemon (is this beldexd's OxenMQ socket?)".into());
        }
        if let Some(parts) = recv(&socket)? {
            if parts.first().map(Vec::as_slice) == Some(b"HELLO") {
                break;
            }
        }
    }

    let mut subscribed_at = Instant::now() - RESUBSCRIBE_EVERY;
    let mut last_heard = Instant::now();
    let mut pending: Option<Vec<u8>> = None;
    let mut announced = false;
    while !stop.load(Ordering::Relaxed) {
        if subscribed_at.elapsed() >= RESUBSCRIBE_EVERY {
            let t = tag();
            socket.send_multipart([b"sub.block".to_vec(), t.clone()], 0).map_err(|e| e.to_string())?;
            pending = Some(t);
            subscribed_at = Instant::now();
        }
        let Some(parts) = recv(&socket)? else {
            if last_heard.elapsed() > SILENCE_LIMIT {
                return Err("daemon went silent".into());
            }
            continue;
        };
        last_heard = Instant::now();
        match parts.first().map(Vec::as_slice) {
            Some(b"REPLY") => {
                let status = parts.get(2).map(|s| String::from_utf8_lossy(s).into_owned()).unwrap_or_default();
                if pending.as_deref() == parts.get(1).map(Vec::as_slice) {
                    pending = None;
                    if status != "OK" && status != "ALREADY" {
                        return Err(format!("sub.block refused: {status}"));
                    }
                    state.connected.store(true, Ordering::Relaxed);
                    if !announced {
                        info!(%endpoint, "subscribed to block notifications");
                        announced = true;
                    } else if status == "OK" {
                        // The daemon had no subscription for us: it restarted.
                        info!(%endpoint, "daemon restarted; resubscribed to block notifications");
                        tx.send_replace(state.height.load(Ordering::Relaxed));
                    }
                }
            }
            Some(b"notify.block") => {
                let height = parts.get(1).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| h.parse::<u64>().ok());
                if let Some(height) = height {
                    state.notifications.fetch_add(1, Ordering::Relaxed);
                    let previous = state.height.fetch_max(height, Ordering::Relaxed);
                    debug!(%endpoint, height, "new block announced");
                    if height > previous {
                        tx.send_replace(height);
                    }
                }
            }
            Some(b"FORBIDDEN") | Some(b"UNKNOWNCOMMAND") | Some(b"NOT_A_SERVICE_NODE") => {
                return Err(format!("daemon refused the subscription: {}", String::from_utf8_lossy(&parts[0])));
            }
            Some(b"BYE") => return Err("daemon closed the connection".into()),
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for beldexd's OxenMQ listener.
    fn mock_daemon(endpoint: &str, announce: u64) {
        let context = zmq::Context::new();
        let router = context.socket(zmq::ROUTER).unwrap();
        router.bind(endpoint).unwrap();
        router.set_rcvtimeo(5000).unwrap();
        std::thread::spawn(move || {
            let mut subscribed = false;
            while let Ok(parts) = router.recv_multipart(0) {
                let route = parts[0].clone();
                match parts[1].as_slice() {
                    b"HI" => router.send_multipart([route, b"HELLO".to_vec()], 0).unwrap(),
                    b"sub.block" => {
                        let tag = parts[2].clone();
                        let status = if subscribed { b"ALREADY".to_vec() } else { b"OK".to_vec() };
                        router.send_multipart([route.clone(), b"REPLY".to_vec(), tag, status], 0).unwrap();
                        if !subscribed {
                            subscribed = true;
                            let msg = [route, b"notify.block".to_vec(), announce.to_string().into_bytes(), vec![7; 32]];
                            router.send_multipart(msg, 0).unwrap();
                        }
                    }
                    _ => {}
                }
            }
            drop(context);
        });
    }

    #[tokio::test]
    async fn relays_block_announcements() {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let endpoint = format!("tcp://127.0.0.1:{port}");
        mock_daemon(&endpoint, 4242);
        let stop = Arc::new(AtomicBool::new(false));
        let (mut rx, state) = subscribe(&[endpoint], stop.clone());
        tokio::time::timeout(Duration::from_secs(10), rx.changed()).await.expect("announcement").unwrap();
        assert_eq!(*rx.borrow(), 4242);
        assert!(state.connected.load(Ordering::Relaxed));
        assert_eq!(state.notifications.load(Ordering::Relaxed), 1);
        stop.store(true, Ordering::Relaxed);
    }

    /// A renewal answered OK means the daemon forgot the subscription
    /// (it restarted): the scanner is woken even without a new height.
    #[tokio::test]
    async fn a_daemon_restart_wakes_the_scanner() {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let endpoint = format!("tcp://127.0.0.1:{port}");
        let context = zmq::Context::new();
        let router = context.socket(zmq::ROUTER).unwrap();
        router.bind(&endpoint).unwrap();
        router.set_rcvtimeo(15_000).unwrap();
        std::thread::spawn(move || {
            while let Ok(parts) = router.recv_multipart(0) {
                let route = parts[0].clone();
                match parts[1].as_slice() {
                    b"HI" => router.send_multipart([route, b"HELLO".to_vec()], 0).unwrap(),
                    // Always "OK": every renewal looks like a fresh daemon.
                    b"sub.block" => {
                        router.send_multipart([route, b"REPLY".to_vec(), parts[2].clone(), b"OK".to_vec()], 0).unwrap()
                    }
                    _ => {}
                }
            }
            drop(context);
        });
        let stop = Arc::new(AtomicBool::new(false));
        let (mut rx, _) = subscribe(&[endpoint], stop.clone());
        tokio::time::timeout(Duration::from_secs(15), rx.changed()).await.expect("woken after renewal").unwrap();
        stop.store(true, Ordering::Relaxed);
    }

    #[tokio::test]
    async fn unreachable_endpoints_do_not_block() {
        let stop = Arc::new(AtomicBool::new(false));
        let (rx, state) = subscribe(&["tcp://127.0.0.1:1".to_owned()], stop.clone());
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(*rx.borrow(), 0);
        assert!(!state.connected.load(Ordering::Relaxed));
        stop.store(true, Ordering::Relaxed);
    }
}
