//! A clustering server.
//!
//! Ingest position reports for moving things; serve clustered vector tiles.
//!
//! Geospatial databases index points for proximity and leave clustering to the
//! client, so a map of moving things ends up running two systems: one for the
//! queries, and a separate supercluster instance rebuilt on a timer for the
//! markers. This closes that seam: the primary index is a net hierarchy, so
//! clustering is a first-class query and the index is never rebuilt.
//!
//! Configuration is entirely environment variables, because a process with no
//! durable state has nothing else to configure:
//!
//! | variable | default | meaning |
//! |---|---|---|
//! | `NETCLUSTER_ADDR` | `0.0.0.0:8080` | listen address |
//! | `NETCLUSTER_SWEEP_SECONDS` | `10` | how often to drop expired devices |
//! | `NETCLUSTER_AUTO_CREATE` | `1` | create a collection on first write |

use netcluster_server::{collection, routes};

use collection::now_ms;
use routes::AppState;
use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, RwLock};
use std::time::Duration;

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// `netcluster-server --health`: connect to our own listener and check `/healthz`.
///
/// This exists so the container image can carry no shell and no curl. Kubernetes
/// and ALB probe over HTTP directly and do not need it, but `docker run` and
/// compose do, and a HEALTHCHECK that shells out is the reason most images end up
/// with a package manager in them.
fn health_check(addr: &str) -> ! {
    use std::io::{Read, Write};
    // A container listening on 0.0.0.0 is reached from inside as loopback.
    let target = addr
        .replace("0.0.0.0", "127.0.0.1")
        .replace("[::]", "[::1]");
    let ok = (|| -> Option<bool> {
        let sa: std::net::SocketAddr = target.parse().ok()?;
        let mut s = std::net::TcpStream::connect_timeout(&sa, Duration::from_secs(2)).ok()?;
        s.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
        s.write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .ok()?;
        let mut buf = [0u8; 64];
        let n = s.read(&mut buf).ok()?;
        Some(buf[..n].windows(3).any(|w| w == b"200"))
    })()
    .unwrap_or(false);
    std::process::exit(i32::from(!ok));
}

#[tokio::main]
async fn main() {
    let addr: String = env_or("NETCLUSTER_ADDR", "0.0.0.0:8080".to_string());
    if std::env::args().any(|a| a == "--health") {
        health_check(&addr);
    }
    let sweep_secs: u64 = env_or("NETCLUSTER_SWEEP_SECONDS", 10);
    let auto_create: u8 = env_or("NETCLUSTER_AUTO_CREATE", 1);

    let state = Arc::new(AppState {
        collections: RwLock::new(HashMap::new()),
        started_ms: now_ms(),
        auto_create: auto_create != 0,
        requests: AtomicU64::new(0),
    });

    // Expiry sweep. A vehicle that stops reporting has to disappear, or clusters
    // slowly fill with ghosts and every count on the map drifts upward.
    if sweep_secs > 0 {
        let s = state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(sweep_secs));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let cs: Vec<_> = s.collections.read().unwrap().values().cloned().collect();
                for c in cs {
                    // sweep() blocks, and only briefly, but never on the async runtime's
                    // worker: a long write-lock hold would stall unrelated requests.
                    let c2 = c.clone();
                    let dropped = tokio::task::spawn_blocking(move || c2.sweep())
                        .await
                        .unwrap_or(0);
                    if dropped > 0 {
                        eprintln!("[sweep] {} dropped {dropped} stale devices", c.name);
                    }
                }
            }
        });
    }

    let app = routes::router(state);
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("netcluster-server: cannot bind {addr}: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("netcluster-server listening on http://{addr}");
    eprintln!("  demo      http://{addr}/");
    eprintln!("  health    http://{addr}/healthz");
    eprintln!("  metrics   http://{addr}/metrics");
    eprintln!(
        "  sweep every {sweep_secs}s, auto-create {}",
        if auto_create != 0 { "on" } else { "off" }
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            eprintln!("\nnetcluster-server: shutting down");
        })
        .await
        .unwrap();
}
