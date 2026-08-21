//! A clustering server.
//!
//! Ingest position reports for moving things; serve clustered vector tiles.
//!
//! Tile38 answers "which devices are near here" and "which crossed this fence".
//! It has no clustering, so people run it *and* a separate supercluster instance
//! that gets rebuilt on a timer for the map. This closes that seam: the primary
//! index is a net hierarchy, so clustering is a first-class query and the index is
//! never rebuilt.
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

#[tokio::main]
async fn main() {
    let addr: String = env_or("NETCLUSTER_ADDR", "0.0.0.0:8080".to_string());
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
