//! The `waystoned` binary: argument parsing and wiring of the three services.
//!
//! `waystoned` (no subcommand) runs the daemon. `waystoned pair --role ...`
//! mints a pairing URI straight from the data directory, for scripts that need
//! to enroll devices without going through the panel.

use std::net::SocketAddr;
use std::ops::RangeInclusive;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use waystone_proto::device::Role;
use waystone_proto::{DEFAULT_HTTPS_PORT, DEFAULT_TURN_PORT};
use waystoned::auth::AdminAuth;
use waystoned::http::{router, AppState};
use waystoned::hub::{Hub, TurnConfig, TURN_CREDENTIAL_TTL};
use waystoned::instance::{self, Instance};
use waystoned::store::Store;

/// Settings keys under which the daemon records what it advertises, so the
/// `pair` subcommand produces the same URIs as the panel.
const ENDPOINT_SETTING: &str = "endpoint";
const TURN_ENDPOINT_SETTING: &str = "turn_endpoint";

/// Self hosted signaling, relay and admin panel for Waystone.
#[derive(Parser, Debug)]
#[command(name = "waystoned", version, about)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,

    /// Directory holding the database, certificate, key and TURN secret.
    #[arg(
        long,
        env = "WAYSTONE_DATA_DIR",
        default_value = "./data",
        global = true
    )]
    data_dir: PathBuf,

    /// Public authority clients connect to, e.g. `home.example.com:8443`.
    /// Defaults to the primary LAN IPv4 and the listen port.
    #[arg(long, env = "WAYSTONE_ENDPOINT", global = true)]
    endpoint: Option<String>,

    /// Public authority of the STUN/TURN service. Defaults to the primary LAN
    /// IPv4 and the TURN listen port.
    #[arg(long, env = "WAYSTONE_TURN_ENDPOINT", global = true)]
    turn_endpoint: Option<String>,

    /// Address of the HTTPS listener carrying the panel and the signaling socket.
    #[arg(long, env = "WAYSTONE_LISTEN", default_value_t = SocketAddr::from(([0, 0, 0, 0], DEFAULT_HTTPS_PORT)))]
    listen: SocketAddr,

    /// Address the TURN server binds.
    #[arg(long, env = "WAYSTONE_TURN_LISTEN", default_value_t = SocketAddr::from(([0, 0, 0, 0], DEFAULT_TURN_PORT)))]
    turn_listen: SocketAddr,

    /// Public IP the TURN server advertises for relayed candidates. Defaults to
    /// the primary LAN IPv4; set it to the public IP behind a NAT.
    #[arg(long, env = "WAYSTONE_TURN_PUBLIC_IP")]
    turn_public_ip: Option<String>,

    /// UDP ports relayed allocations use, e.g. `49160-49200`, so a cloud
    /// firewall can open exactly these. Any ephemeral port by default.
    #[arg(long, env = "WAYSTONE_RELAY_PORTS", value_parser = parse_port_range)]
    relay_ports: Option<RangeInclusive<u16>>,

    /// Shared secret for TURN credentials. Defaults to a random secret stored
    /// in `<data-dir>/turn.secret`.
    #[arg(long, env = "WAYSTONE_TURN_SECRET", hide_env_values = true)]
    turn_secret: Option<String>,

    /// Video bitrate ceiling (bits per second) hosts must respect while a
    /// session is relayed through TURN. `0` disables the clamp.
    #[arg(
        long,
        env = "WAYSTONE_RELAY_BITRATE_CEILING",
        default_value_t = 4_000_000
    )]
    relay_bitrate_ceiling: u32,

    /// Additional DNS name or IP to put into a newly generated certificate.
    #[arg(long = "hostname", env = "WAYSTONE_HOSTNAMES", value_delimiter = ',')]
    hostnames: Vec<String>,

    /// Disable the built-in TURN relay.
    #[arg(long, env = "WAYSTONE_NO_TURN", default_value_t = false)]
    no_turn: bool,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Print a one-time pairing URI for a new device and exit.
    ///
    /// Reads the data directory directly, so it needs no admin password; it
    /// works whether or not the daemon is running (start it once first so the
    /// instance exists). Honors `WAYSTONE_PAIR_TTL` like the panel.
    Pair {
        /// Role the enrolled device gets.
        #[arg(long, value_enum)]
        role: RoleArg,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum RoleArg {
    Host,
    Controller,
}

impl From<RoleArg> for Role {
    fn from(role: RoleArg) -> Role {
        match role {
            RoleArg::Host => Role::Host,
            RoleArg::Controller => Role::Controller,
        }
    }
}

fn main() -> Result<()> {
    // Logs go to stderr so `waystoned pair` leaves only the URI on stdout.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "waystoned=info,warn".into()),
        )
        .init();

    let args = Args::parse();
    match args.command {
        Some(Command::Pair { role }) => pair(&args, role.into()),
        None => tokio::runtime::Runtime::new()
            .context("starting the async runtime")?
            .block_on(serve(args)),
    }
}

/// Endpoints advertised in pairing URIs and relay credentials.
struct Advertised {
    endpoint: String,
    turn_endpoint: String,
    lan_ip: Option<String>,
}

/// Resolves what to advertise: explicit flags first, then (for `pair`) what
/// the daemon recorded, then the detected LAN address.
fn advertised(args: &Args, store: Option<&Store>) -> Result<Advertised> {
    let recorded = |key: &str| -> Result<Option<String>> {
        match store {
            Some(store) => store.setting(key),
            None => Ok(None),
        }
    };
    let endpoint = match &args.endpoint {
        Some(value) => Some(value.clone()),
        None => recorded(ENDPOINT_SETTING)?,
    };
    let turn_endpoint = match &args.turn_endpoint {
        Some(value) => Some(value.clone()),
        None => recorded(TURN_ENDPOINT_SETTING)?,
    };

    // The relay address only matters to the daemon, not to `pair`.
    let needs_relay_ip = store.is_none() && args.turn_public_ip.is_none();
    let needs_lan = endpoint.is_none() || turn_endpoint.is_none() || needs_relay_ip;
    let lan_ip = if needs_lan {
        match waystoned::lan::primary_ipv4() {
            Some(candidate) => {
                info!(
                    "detected LAN address {} on {}",
                    candidate.ip, candidate.interface
                );
                Some(candidate.ip.to_string())
            }
            None => {
                warn!("no LAN IPv4 found; advertising 127.0.0.1, pass --endpoint to fix");
                None
            }
        }
    } else {
        None
    };
    let host = lan_ip.clone().unwrap_or_else(|| "127.0.0.1".to_string());
    Ok(Advertised {
        endpoint: endpoint.unwrap_or_else(|| format!("{host}:{}", args.listen.port())),
        turn_endpoint: turn_endpoint
            .unwrap_or_else(|| format!("{host}:{}", args.turn_listen.port())),
        lan_ip,
    })
}

fn pair(args: &Args, role: Role) -> Result<()> {
    let initialized =
        instance::cert_path(&args.data_dir).exists() && instance::key_path(&args.data_dir).exists();
    if !initialized {
        bail!(
            "{} holds no instance yet; start the daemon once with the same --data-dir",
            args.data_dir.display()
        );
    }
    let instance = Instance::load_or_create(&args.data_dir, &[])?;
    let store = Store::open(Store::default_path(&args.data_dir))?;
    store.ensure_instance(&instance.server_id, &instance.fingerprint)?;
    let advertised = advertised(args, Some(&store))?;
    let uri = waystoned::pairing::create(
        &store,
        &advertised.endpoint,
        &advertised.turn_endpoint,
        &instance.fingerprint,
        role,
    )?;
    println!("{}", uri.to_uri());
    Ok(())
}

async fn serve(args: Args) -> Result<()> {
    std::fs::create_dir_all(&args.data_dir)
        .with_context(|| format!("creating {}", args.data_dir.display()))?;
    let advertised = advertised(&args, None)?;
    info!(
        "advertising signaling {} and relay {}",
        advertised.endpoint, advertised.turn_endpoint
    );

    let instance =
        Instance::load_or_create(&args.data_dir, &certificate_names(&args, &advertised))?;
    info!(
        "instance {} fingerprint {}",
        instance.server_id, instance.fingerprint
    );

    let store = Arc::new(Store::open(Store::default_path(&args.data_dir))?);
    // The instance row must exist before the hub starts signing challenges.
    store.ensure_instance(&instance.server_id, &instance.fingerprint)?;
    store.set_setting(ENDPOINT_SETTING, &advertised.endpoint)?;
    store.set_setting(TURN_ENDPOINT_SETTING, &advertised.turn_endpoint)?;

    let cancel = CancellationToken::new();
    let turn = if args.no_turn {
        None
    } else {
        Some(start_turn(&args, &advertised, &cancel)?)
    };

    let auth = Arc::new(AdminAuth::load(store.clone())?);
    if !auth.is_claimed() {
        info!(
            "instance is unclaimed: open https://{}/ to set the admin password",
            advertised.endpoint
        );
    }
    let state = AppState {
        store: store.clone(),
        hub: Hub::new(instance.server_id.clone(), store.clone())?.with_turn(turn),
        auth,
        fingerprint: instance.fingerprint.clone(),
        endpoint: advertised.endpoint.clone(),
        turn_endpoint: advertised.turn_endpoint.clone(),
    };

    spawn_token_pruner(store.clone(), cancel.clone());

    let tls = waystoned::tls::config(&instance).await?;
    let server = axum_server::bind_rustls(args.listen, tls)
        .serve(router(state).into_make_service_with_connect_info::<SocketAddr>());

    info!("admin panel and signaling on https://{}", args.listen);
    tokio::select! {
        result = server => result.context("https server failed")?,
        _ = shutdown_signal() => {
            info!("shutting down");
            cancel.cancel();
        }
    }
    Ok(())
}

/// Names put into a newly generated certificate.
fn certificate_names(args: &Args, advertised: &Advertised) -> Vec<String> {
    let mut names = args.hostnames.clone();
    if names.is_empty() {
        names.push("waystone.local".to_string());
    }
    let endpoint_host = authority_host(&advertised.endpoint);
    let extras = [Some("localhost"), Some("127.0.0.1"), endpoint_host]
        .into_iter()
        .flatten()
        .map(str::to_string)
        .chain(advertised.lan_ip.clone());
    for extra in extras {
        if !names.contains(&extra) {
            names.push(extra);
        }
    }
    names
}

/// Host part of a `host:port` authority (IPv6 literals in brackets included).
fn authority_host(authority: &str) -> Option<&str> {
    if let Some(rest) = authority.strip_prefix('[') {
        return rest.split(']').next();
    }
    authority
        .rsplit_once(':')
        .map(|(host, _)| host)
        .or(Some(authority))
}

fn start_turn(
    args: &Args,
    advertised: &Advertised,
    cancel: &CancellationToken,
) -> Result<TurnConfig> {
    let secret = match &args.turn_secret {
        Some(secret) if !secret.is_empty() => secret.clone(),
        _ => instance::load_or_create_turn_secret(&args.data_dir)?,
    };
    let public_ip = args
        .turn_public_ip
        .clone()
        .or_else(|| advertised.lan_ip.clone())
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let ceiling = (args.relay_bitrate_ceiling > 0).then_some(args.relay_bitrate_ceiling);
    info!(
        "turn relay advertises {public_ip}, bitrate ceiling {}",
        ceiling
            .map(|value| format!("{value} bps"))
            .unwrap_or_else(|| "disabled".into())
    );

    let listen = args.turn_listen;
    let relay_ports = args.relay_ports.clone();
    let turn_secret = secret.clone();
    let cancel = cancel.clone();
    tokio::spawn(async move {
        if let Err(error) =
            waystoned::turnserver::run(listen, &public_ip, &turn_secret, relay_ports, cancel).await
        {
            warn!("turn server stopped: {error:#}");
        }
    });
    Ok(TurnConfig {
        server: advertised.turn_endpoint.clone(),
        secret,
        ttl: TURN_CREDENTIAL_TTL,
        relay_bitrate_ceiling: ceiling,
    })
}

/// Parses `low-high` into an inclusive port range.
fn parse_port_range(text: &str) -> Result<RangeInclusive<u16>, String> {
    let (low, high) = text
        .split_once('-')
        .ok_or_else(|| format!("expected LOW-HIGH, got {text:?}"))?;
    let low: u16 = low
        .trim()
        .parse()
        .map_err(|_| format!("bad port {low:?}"))?;
    let high: u16 = high
        .trim()
        .parse()
        .map_err(|_| format!("bad port {high:?}"))?;
    if low == 0 || low > high {
        return Err(format!("{text:?} is not a range of ports"));
    }
    Ok(low..=high)
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
