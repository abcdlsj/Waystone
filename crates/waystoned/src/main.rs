//! The `waystoned` binary: argument parsing and wiring of the three services.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use waystone_proto::{DEFAULT_HTTPS_PORT, DEFAULT_TURN_PORT};
use waystoned::http::{parse_addr, router, AppState};
use waystoned::hub::Hub;
use waystoned::instance::Instance;
use waystoned::store::Store;

/// Self hosted signaling, relay and admin panel for Waystone.
#[derive(Parser, Debug)]
#[command(name = "waystoned", version, about)]
struct Args {
    /// Directory holding the database, certificate and key.
    #[arg(long, env = "WAYSTONE_DATA_DIR", default_value = "./data")]
    data_dir: PathBuf,

    /// Address of the HTTPS listener carrying the panel and the signaling socket.
    #[arg(long, env = "WAYSTONE_LISTEN")]
    listen: Option<String>,

    /// Public authority clients should connect to, e.g. `home.example.com:8443`.
    #[arg(long, env = "WAYSTONE_ENDPOINT")]
    endpoint: Option<String>,

    /// Public authority of the STUN/TURN service.
    #[arg(long, env = "WAYSTONE_TURN_ENDPOINT")]
    turn_endpoint: Option<String>,

    /// Address the TURN server binds.
    #[arg(long, env = "WAYSTONE_TURN_LISTEN", default_value = "0.0.0.0:3478")]
    turn_listen: SocketAddr,

    /// Public IP the TURN server advertises for relayed candidates.
    #[arg(long, env = "WAYSTONE_TURN_PUBLIC_IP")]
    turn_public_ip: Option<String>,

    /// Shared secret for TURN long term credentials.
    #[arg(long, env = "WAYSTONE_TURN_SECRET")]
    turn_secret: Option<String>,

    /// Additional DNS name or IP to put into the certificate.
    #[arg(long = "hostname", env = "WAYSTONE_HOSTNAMES", value_delimiter = ',')]
    hostnames: Vec<String>,

    /// Disable the built-in TURN relay.
    #[arg(long, env = "WAYSTONE_NO_TURN", default_value_t = false)]
    no_turn: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "waystoned=info,warn".into()),
        )
        .init();

    let args = Args::parse();
    std::fs::create_dir_all(&args.data_dir)
        .with_context(|| format!("creating {}", args.data_dir.display()))?;

    let mut hostnames = args.hostnames.clone();
    if hostnames.is_empty() {
        hostnames.push("waystone.local".to_string());
    }
    for extra in ["localhost", "127.0.0.1"] {
        if !hostnames.iter().any(|value| value == extra) {
            hostnames.push(extra.to_string());
        }
    }

    let instance = Instance::load_or_create(&args.data_dir, &hostnames)?;
    info!(
        "instance {} fingerprint {}",
        instance.server_id, instance.fingerprint
    );

    let store = Arc::new(Store::open(Store::default_path(&args.data_dir))?);
    {
        // The instance row must exist before the hub starts signing challenges.
        let mut store = Store::open(Store::default_path(&args.data_dir))?;
        store.ensure_instance(&instance.server_id, &instance.fingerprint)?;
        drop(store);
    }

    let listen = parse_addr(
        &args
            .listen
            .clone()
            .unwrap_or_else(|| format!("0.0.0.0:{DEFAULT_HTTPS_PORT}")),
    );
    let endpoint = args
        .endpoint
        .clone()
        .unwrap_or_else(|| format!("localhost:{}", listen.port()));
    let turn_endpoint = args
        .turn_endpoint
        .clone()
        .unwrap_or_else(|| format!("localhost:{DEFAULT_TURN_PORT}"));

    let password_hash = store.admin_password_hash()?;
    let state = AppState {
        store: store.clone(),
        hub: Hub::new(instance.server_id.clone(), store.clone()),
        fingerprint: instance.fingerprint.clone(),
        endpoint,
        turn_endpoint,
        admin_password_hash: password_hash,
    };

    let cancel = CancellationToken::new();

    if !args.no_turn {
        let secret = args
            .turn_secret
            .clone()
            .unwrap_or_else(|| instance.server_id.clone());
        let public_ip = args
            .turn_public_ip
            .clone()
            .unwrap_or_else(|| "127.0.0.1".to_string());
        let turn_listen = args.turn_listen;
        let cancel_turn = cancel.clone();
        tokio::spawn(async move {
            if let Err(error) =
                waystoned::turnserver::run(turn_listen, &public_ip, &secret, cancel_turn).await
            {
                warn!("turn server stopped: {error}");
            }
        });
    }

    spawn_token_pruner(store.clone(), cancel.clone());

    let tls = waystoned::tls::config(&instance).await?;
    let app = router(state);
    let server = axum_server::bind_rustls(listen, tls)
        .serve(app.into_make_service_with_connect_info::<SocketAddr>());

    info!("admin panel and signaling on https://{listen}");
    tokio::select! {
        result = server => result.context("https server failed")?,
        _ = shutdown_signal() => {
            info!("shutting down");
            cancel.cancel();
        }
    }
    Ok(())
}

fn spawn_token_pruner(store: Arc<Store>, cancel: CancellationToken) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(300));
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if let Err(error) = store.prune_tokens() {
                        warn!("pruning pairing tokens failed: {error}");
                    }
                }
                _ = cancel.cancelled() => break,
            }
        }
    });
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
