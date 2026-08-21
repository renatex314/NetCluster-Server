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
//! | `NETCLUSTER_DATA_DIR` | *(unset)* | where to keep snapshots; unset means no persistence |
//! | `NETCLUSTER_SNAPSHOT_SECONDS` | `60` | how often to snapshot, when a data dir is set |

use netcluster_server::collection::Collection;
use netcluster_server::{collection, routes, snapshot};

use collection::now_ms;
use routes::AppState;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
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

/// Wait for a shutdown signal.
///
/// SIGTERM matters as much as SIGINT and was previously missing: `tokio::signal::ctrl_c`
/// is SIGINT only, and Docker and Kubernetes both send SIGTERM. Unhandled, its
/// default disposition kills the process outright -- so the graceful path never
/// ran under `docker stop` or a rolling deploy, and a snapshot on shutdown would
/// never have fired.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(e) => {
                eprintln!("netcluster-server: cannot listen for SIGTERM: {e}");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => eprintln!("\nnetcluster-server: SIGINT"),
        _ = term => eprintln!("\nnetcluster-server: SIGTERM"),
    }
}

/// Load every snapshot in `dir`.
///
/// A snapshot that will not load is skipped loudly and that collection starts
/// empty. Refusing to boot would be worse: one corrupt file would keep the whole
/// service down, when the position stream will refill it anyway.
fn restore_all(dir: &Path) -> Vec<Arc<Collection>> {
    let files = match snapshot::list(dir) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("[restore] cannot read {}: {e}", dir.display());
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for path in files {
        match snapshot::read(&path) {
            Ok((meta, records)) => {
                let total = records.len();
                let (c, skipped) = Collection::restore(&meta.name, meta.config, &records);
                let age = collection::now_ms()
                    .saturating_sub(records.iter().map(|r| r.last_seen_ms).max().unwrap_or(0));
                eprintln!(
                    "[restore] {}: {} of {total} devices (snapshot {}s old{})",
                    meta.name,
                    c.len(),
                    age / 1000,
                    if skipped > 0 {
                        format!(", {skipped} past their TTL")
                    } else {
                        String::new()
                    }
                );
                out.push(Arc::new(c));
            }
            Err(e) => eprintln!(
                "[restore] SKIPPING {}: {e} -- this collection starts empty",
                path.display()
            ),
        }
    }
    out
}

/// Snapshot every collection. Returns how many succeeded and how many failed.
fn snapshot_all(state: &routes::AppState, dir: &Path, why: &str) -> (usize, usize) {
    let cs: Vec<_> = state
        .collections
        .read()
        .unwrap()
        .values()
        .cloned()
        .collect();
    let (mut ok, mut failed) = (0, 0);
    let mut bytes = 0u64;
    for c in &cs {
        match c.snapshot_to(&snapshot::path_for(dir, &c.name)) {
            Ok(n) => {
                ok += 1;
                bytes += n;
            }
            Err(e) => {
                failed += 1;
                eprintln!("[snapshot] {} FAILED: {e}", c.name);
            }
        }
    }
    if ok > 0 || failed > 0 {
        eprintln!(
            "[snapshot] {why}: {ok} collection(s), {:.1} MB{}",
            bytes as f64 / 1e6,
            if failed > 0 {
                format!(", {failed} FAILED")
            } else {
                String::new()
            }
        );
    }
    (ok, failed)
}

#[tokio::main]
async fn main() {
    let addr: String = env_or("NETCLUSTER_ADDR", "0.0.0.0:8080".to_string());
    if std::env::args().any(|a| a == "--health") {
        health_check(&addr);
    }
    let sweep_secs: u64 = env_or("NETCLUSTER_SWEEP_SECONDS", 10);
    let auto_create: u8 = env_or("NETCLUSTER_AUTO_CREATE", 1);
    let snapshot_secs: u64 = env_or("NETCLUSTER_SNAPSHOT_SECONDS", 60);
    // Unset means no persistence, which is the documented default: the index is a
    // materialised view of your position stream and normally refills itself.
    let data_dir: Option<PathBuf> = std::env::var("NETCLUSTER_DATA_DIR")
        .ok()
        .filter(|d| !d.trim().is_empty())
        .map(PathBuf::from);

    let mut collections = HashMap::new();
    if let Some(dir) = &data_dir {
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!("netcluster-server: cannot use {}: {e}", dir.display());
            std::process::exit(1);
        }
        // Prove the directory is writable now, rather than discovering at the
        // first snapshot that it is not. A mounted volume owned by root while the
        // server runs as non-root is the usual cause, and the failure would
        // otherwise surface at shutdown -- exactly when the data is needed.
        let probe = dir.join(".netcluster-write-test");
        if let Err(e) = std::fs::write(&probe, b"ok").and_then(|_| std::fs::remove_file(&probe)) {
            eprintln!(
                "netcluster-server: NETCLUSTER_DATA_DIR={} is not writable: {e}\n  \
                 the server runs as uid 65532; a Docker named volume or a Kubernetes \
                 volume mounted here must be owned by it (see docs/DEPLOY.md)",
                dir.display()
            );
            std::process::exit(1);
        }
        for c in restore_all(dir) {
            collections.insert(c.name.clone(), c);
        }
    }

    let state = Arc::new(AppState {
        collections: RwLock::new(collections),
        started_ms: now_ms(),
        auto_create: auto_create != 0,
        requests: AtomicU64::new(0),
        data_dir: data_dir.clone(),
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

    if let (Some(dir), true) = (data_dir.clone(), snapshot_secs > 0) {
        let s = state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(snapshot_secs));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await; // the first tick is immediate; we just restored
            loop {
                tick.tick().await;
                let (s2, d2) = (s.clone(), dir.clone());
                // Disk I/O never belongs on a runtime worker, same as the sweep.
                let _ =
                    tokio::task::spawn_blocking(move || snapshot_all(&s2, &d2, "periodic")).await;
            }
        });
    }

    let app = routes::router(state.clone());
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
    match &data_dir {
        Some(d) => eprintln!(
            "  persistence {} , snapshot every {snapshot_secs}s",
            d.display()
        ),
        None => eprintln!("  persistence off (set NETCLUSTER_DATA_DIR to enable)"),
    }

    if let Err(e) = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
    {
        eprintln!("netcluster-server: {e}");
    }

    // After serving stops, so nothing can be mutating the index while it is
    // copied, and the final snapshot is exactly the state we shut down with.
    if let Some(dir) = &data_dir {
        snapshot_all(&state, dir, "shutdown");
    }
    eprintln!("netcluster-server: shutting down");
}
