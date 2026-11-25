//! Router port mappings for the host's transport port.
//!
//! A host behind a home router is unreachable for a peer whose NAT allocates a
//! new port per destination (most cellular carriers): hole punching needs one
//! side whose public port is predictable. Asking the router to forward a port
//! gives the host exactly that, and the forwarded address is advertised as a
//! server reflexive candidate.
//!
//! UPnP IGD is tried first because nearly every consumer router speaks it,
//! then PCP and its predecessor NAT-PMP (Apple routers, OpenWrt with
//! miniupnpd). UPnP is implemented here on purpose: the protocol subset is
//! small (SSDP, one HTTP GET, three SOAP calls) and the available crates pull
//! in a full HTTP and XML stack.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::num::NonZeroU16;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::oneshot;
use tokio::time::timeout;
use tracing::{debug, info};

/// Requested lease. Renewed at half of it while the session lives.
const LEASE_SECONDS: u32 = 3600;
/// How long routers get to answer the SSDP search.
const DISCOVERY_WINDOW: Duration = Duration::from_millis(1500);
/// Ceiling for one HTTP exchange with the router.
const HTTP_TIMEOUT: Duration = Duration::from_secs(3);
const DESCRIPTION: &str = "Waystone";

/// Maps `port` on the router, reports `(external, base)` through `result`,
/// keeps the mapping alive and deletes it once `released` fires or its
/// sender is dropped.
///
/// Each of `interfaces` is tried in order until a router answers; the first
/// should be the one the default route uses.
pub(crate) async fn hold(
    interfaces: Vec<Ipv4Addr>,
    port: u16,
    result: oneshot::Sender<Option<(SocketAddr, SocketAddr)>>,
    mut released: oneshot::Receiver<()>,
) {
    let mut opened = None;
    for local in interfaces {
        if let Some(mapping) = Mapping::open(local, port).await {
            opened = Some((mapping, local));
            break;
        }
    }
    let Some((mut mapping, local)) = opened else {
        debug!("no router port mapping available");
        let _ = result.send(None);
        return;
    };
    let base = SocketAddr::new(IpAddr::V4(local), port);
    info!("router forwards {} to {base}", mapping.external());
    if result.send(Some((mapping.external(), base))).is_err() {
        mapping.close().await;
        return;
    }
    loop {
        tokio::select! {
            _ = &mut released => break,
            _ = tokio::time::sleep(mapping.renew_after()) => {
                if !mapping.renew().await {
                    debug!("port mapping renewal failed");
                }
            }
        }
    }
    mapping.close().await;
}

enum Mapping {
    Upnp {
        gateway: Upnp,
        external: SocketAddr,
        local: Ipv4Addr,
        port: u16,
        lease: u32,
    },
    Pcp(crab_nat::PortMapping, Ipv4Addr),
}

impl Mapping {
    async fn open(local: Ipv4Addr, port: u16) -> Option<Mapping> {
        let gateway = match Upnp::discover(local).await {
            Some(upnp) => match upnp.map(local, port).await {
                Some((external, lease)) => {
                    return Some(Mapping::Upnp {
                        gateway: upnp,
                        external,
                        local,
                        port,
                        lease,
                    })
                }
                // The router answered SSDP, so it is the gateway; it may
                // still speak PCP.
                None => upnp.host,
            },
            None => default_gateway_guess(local)?,
        };
        let mapping = pcp_map(gateway, local, port).await?;
        Some(mapping)
    }

    fn external(&self) -> SocketAddr {
        match self {
            Mapping::Upnp { external, .. } => *external,
            Mapping::Pcp(mapping, external) => {
                SocketAddr::new(IpAddr::V4(*external), mapping.external_port().get())
            }
        }
    }

    fn renew_after(&self) -> Duration {
        let lease = match self {
            // A permanent lease (0) needs no renewal, but checking once an
            // hour costs nothing.
            Mapping::Upnp { lease: 0, .. } => LEASE_SECONDS,
            Mapping::Upnp { lease, .. } => *lease,
            Mapping::Pcp(mapping, _) => mapping.lifetime(),
        };
        Duration::from_secs(u64::from(lease.max(60) / 2))
    }

    async fn renew(&mut self) -> bool {
        match self {
            Mapping::Upnp {
                gateway,
                external,
                local,
                port,
                lease,
            } => gateway
                .add(external.port(), *local, *port, *lease)
                .await
                .is_ok(),
            Mapping::Pcp(mapping, _) => mapping.renew().await.is_ok(),
        }
    }

    async fn close(self) {
        match self {
            Mapping::Upnp {
                gateway, external, ..
            } => gateway.delete(external.port()).await,
            Mapping::Pcp(mapping, _) => {
                let _ = mapping.try_drop().await;
            }
        }
    }
}

/// PCP, falling back to NAT-PMP inside `crab_nat`.
async fn pcp_map(gateway: Ipv4Addr, local: Ipv4Addr, port: u16) -> Option<Mapping> {
    let timeouts = crab_nat::TimeoutConfig {
        initial_timeout: Duration::from_millis(250),
        max_retries: 2,
        max_retry_timeout: Some(Duration::from_secs(1)),
    };
    let mapping = crab_nat::PortMapping::new(
        gateway.into(),
        local.into(),
        crab_nat::InternetProtocol::Udp,
        NonZeroU16::new(port)?,
        crab_nat::PortMappingOptions {
            external_port: NonZeroU16::new(port),
            lifetime_seconds: Some(LEASE_SECONDS),
            timeout_config: Some(timeouts),
        },
    )
    .await
    .map_err(|error| debug!("pcp/nat-pmp via {gateway}: {error}"))
    .ok()?;
    let external = match mapping.mapping_type() {
        crab_nat::PortMappingType::Pcp {
            external_ip: IpAddr::V4(ip),
            ..
        } => ip,
        crab_nat::PortMappingType::Pcp { .. } => return None,
        crab_nat::PortMappingType::NatPmp => {
            crab_nat::natpmp::external_address(gateway.into(), Some(timeouts))
                .await
                .ok()?
        }
    };
    if !is_public(external) {
        debug!("pcp/nat-pmp external address {external} is not public");
        let _ = mapping.try_drop().await;
        return None;
    }
    Some(Mapping::Pcp(mapping, external))
}

/// Without SSDP there is no portable way to read the default route, so the
/// conventional first address of the local /24 is tried.
fn default_gateway_guess(local: Ipv4Addr) -> Option<Ipv4Addr> {
    let [a, b, c, d] = local.octets();
    (d != 1).then_some(Ipv4Addr::new(a, b, c, 1))
}

/// True for addresses reachable from the internet: a router behind another
/// NAT (carrier-grade, or a second router) maps a port nobody can reach.
fn is_public(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    let shared = a == 100 && (64..128).contains(&b);
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_documentation()
        || shared)
}

/// A UPnP internet gateway's WAN connection service.
struct Upnp {
    host: Ipv4Addr,
    port: u16,
    control_path: String,
    service: String,
}

#[derive(Debug)]
enum SoapError {
    Fault(u16),
    Transport,
}

impl Upnp {
    async fn discover(local: Ipv4Addr) -> Option<Upnp> {
        let socket = UdpSocket::bind((local, 0)).await.ok()?;
        for device in [
            "urn:schemas-upnp-org:device:InternetGatewayDevice:2",
            "urn:schemas-upnp-org:device:InternetGatewayDevice:1",
        ] {
            let search = format!(
                "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\n\
                 MAN: \"ssdp:discover\"\r\nMX: 1\r\nST: {device}\r\n\r\n"
            );
            socket
                .send_to(search.as_bytes(), (Ipv4Addr::new(239, 255, 255, 250), 1900))
                .await
                .ok()?;
        }
        let mut buffer = [0u8; 2048];
        let deadline = tokio::time::Instant::now() + DISCOVERY_WINDOW;
        loop {
            let (len, _) = tokio::time::timeout_at(deadline, socket.recv_from(&mut buffer))
                .await
                .ok()?
                .ok()?;
            let Some(location) = ssdp_location(&String::from_utf8_lossy(&buffer[..len])) else {
                continue;
            };
            if let Some(upnp) = Upnp::describe(&location).await {
                return Some(upnp);
            }
        }
    }

    async fn describe(location: &str) -> Option<Upnp> {
        let (host, port, path) = parse_url(location)?;
        let request = format!("GET {path} HTTP/1.0\r\nHost: {host}:{port}\r\n\r\n");
        let (status, body) = http(host, port, request.as_bytes()).await?;
        if status != 200 {
            return None;
        }
        let (service, control) = wan_service(&body)?;
        let control_path = match parse_url(&control) {
            Some((_, _, path)) => path,
            None if control.starts_with('/') => control,
            None => format!("/{control}"),
        };
        debug!("upnp gateway {host}:{port}{control_path} ({service})");
        Some(Upnp {
            host,
            port,
            control_path,
            service,
        })
    }

    /// Maps `port`, preferring the same external port. Returns the external
    /// address and the granted lease.
    async fn map(&self, local: Ipv4Addr, port: u16) -> Option<(SocketAddr, u32)> {
        let external_ip = self.external_ip().await?;
        if !is_public(external_ip) {
            debug!("upnp external address {external_ip} is not public");
            return None;
        }
        let mut lease = LEASE_SECONDS;
        let mut external = port;
        for _ in 0..4 {
            match self.add(external, local, port, lease).await {
                Ok(()) => return Some((SocketAddr::new(external_ip.into(), external), lease)),
                // OnlyPermanentLeasesSupported
                Err(SoapError::Fault(725)) if lease != 0 => lease = 0,
                // ConflictInMappingEntry: someone else holds the port.
                Err(SoapError::Fault(718)) => {
                    external = rand::random_range(20_000..60_000);
                }
                Err(error) => {
                    debug!("upnp AddPortMapping failed: {error:?}");
                    return None;
                }
            }
        }
        None
    }

    async fn external_ip(&self) -> Option<Ipv4Addr> {
        let body = self.call("GetExternalIPAddress", &[]).await.ok()?;
        xml_text(&body, "NewExternalIPAddress")?.trim().parse().ok()
    }

    async fn add(
        &self,
        external: u16,
        local: Ipv4Addr,
        port: u16,
        lease: u32,
    ) -> Result<(), SoapError> {
        let (external, port, lease, local) = (
            external.to_string(),
            port.to_string(),
            lease.to_string(),
            local.to_string(),
        );
        self.call(
            "AddPortMapping",
            &[
                ("NewRemoteHost", ""),
                ("NewExternalPort", &external),
                ("NewProtocol", "UDP"),
                ("NewInternalPort", &port),
                ("NewInternalClient", &local),
                ("NewEnabled", "1"),
                ("NewPortMappingDescription", DESCRIPTION),
                ("NewLeaseDuration", &lease),
            ],
        )
        .await
        .map(drop)
    }

    async fn delete(&self, external: u16) {
        let external = external.to_string();
        let result = self
            .call(
                "DeletePortMapping",
                &[
                    ("NewRemoteHost", ""),
                    ("NewExternalPort", &external),
                    ("NewProtocol", "UDP"),
                ],
            )
            .await;
        debug!("upnp DeletePortMapping {external}: {result:?}");
    }

    async fn call(&self, action: &str, arguments: &[(&str, &str)]) -> Result<String, SoapError> {
        let body = soap_envelope(&self.service, action, arguments);
        let request = format!(
            "POST {path} HTTP/1.0\r\nHost: {host}:{port}\r\n\
             Content-Type: text/xml; charset=\"utf-8\"\r\n\
             SOAPAction: \"{service}#{action}\"\r\nContent-Length: {len}\r\n\r\n{body}",
            path = self.control_path,
            host = self.host,
            port = self.port,
            service = self.service,
            len = body.len(),
        );
        let (status, response) = http(self.host, self.port, request.as_bytes())
            .await
            .ok_or(SoapError::Transport)?;
        match status {
            200 => Ok(response),
            _ => Err(SoapError::Fault(
                xml_text(&response, "errorCode")
                    .and_then(|code| code.trim().parse().ok())
                    .unwrap_or(status),
            )),
        }
    }
}

/// One HTTP/1.0 exchange (so the body is never chunked). Returns the status
/// code and the body.
async fn http(host: Ipv4Addr, port: u16, request: &[u8]) -> Option<(u16, String)> {
    let exchange = async {
        let mut stream = TcpStream::connect((host, port)).await.ok()?;
        stream.write_all(request).await.ok()?;
        let mut response = Vec::new();
        stream.take(1 << 20).read_to_end(&mut response).await.ok()?;
        Some(response)
    };
    let response = timeout(HTTP_TIMEOUT, exchange).await.ok()??;
    let response = String::from_utf8_lossy(&response);
    let (head, body) = response.split_once("\r\n\r\n")?;
    let status = head.split_whitespace().nth(1)?.parse().ok()?;
    Some((status, body.to_string()))
}

/// The `LOCATION` header of an SSDP response.
fn ssdp_location(response: &str) -> Option<String> {
    response.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case("location")
            .then(|| value.trim().to_string())
    })
}

/// Splits `http://host:port/path`. Only IPv4 hosts are accepted: a router
/// on the local network announces itself by address.
fn parse_url(url: &str) -> Option<(Ipv4Addr, u16, String)> {
    let rest = url.strip_prefix("http://")?;
    let (authority, path) = match rest.find('/') {
        Some(index) => rest.split_at(index),
        None => (rest, "/"),
    };
    let (host, port) = match authority.split_once(':') {
        Some((host, port)) => (host, port.parse().ok()?),
        None => (authority, 80),
    };
    Some((host.parse().ok()?, port, path.to_string()))
}

/// The service type and control URL of the first WAN connection service in
/// a device description.
fn wan_service(description: &str) -> Option<(String, String)> {
    description.split("<service>").skip(1).find_map(|block| {
        let service = xml_text(block, "serviceType")?.trim();
        let wan = service.starts_with("urn:schemas-upnp-org:service:WANIPConnection:")
            || service.starts_with("urn:schemas-upnp-org:service:WANPPPConnection:");
        wan.then(|| {
            Some((
                service.to_string(),
                xml_text(block, "controlURL")?.trim().to_string(),
            ))
        })?
    })
}

/// The text of the first `<tag>` element, ignoring namespace prefixes.
fn xml_text<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let mut rest = xml;
    loop {
        let start = rest.find('<')?;
        rest = &rest[start + 1..];
        let end = rest.find('>')?;
        let name = rest[..end].split_whitespace().next()?;
        let local = name.rsplit(':').next()?;
        rest = &rest[end + 1..];
        if local == tag && !name.starts_with('/') {
            let close = rest.find("</")?;
            return Some(&rest[..close]);
        }
    }
}

fn soap_envelope(service: &str, action: &str, arguments: &[(&str, &str)]) -> String {
    let arguments: String = arguments
        .iter()
        .map(|(name, value)| format!("<{name}>{value}</{name}>"))
        .collect();
    format!(
        "<?xml version=\"1.0\"?>\
         <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
         <s:Body><u:{action} xmlns:u=\"{service}\">{arguments}</u:{action}></s:Body>\
         </s:Envelope>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_ssdp_location() {
        let response = "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=120\r\n\
                        Location: http://192.168.1.1:5000/rootDesc.xml\r\n\r\n";
        assert_eq!(
            ssdp_location(response).as_deref(),
            Some("http://192.168.1.1:5000/rootDesc.xml")
        );
    }

    #[test]
    fn splits_urls() {
        assert_eq!(
            parse_url("http://192.168.1.1:5000/ctl/IPConn"),
            Some((Ipv4Addr::new(192, 168, 1, 1), 5000, "/ctl/IPConn".into()))
        );
        assert_eq!(
            parse_url("http://10.0.0.1"),
            Some((Ipv4Addr::new(10, 0, 0, 1), 80, "/".into()))
        );
        assert_eq!(parse_url("https://router.local/x"), None);
    }

    #[test]
    fn finds_the_wan_service() {
        let description = r#"<root><device><serviceList>
            <service><serviceType>urn:schemas-upnp-org:service:Layer3Forwarding:1</serviceType>
            <controlURL>/ctl/L3F</controlURL></service>
            <service><serviceType>urn:schemas-upnp-org:service:WANIPConnection:2</serviceType>
            <controlURL>/ctl/IPConn</controlURL></service>
            </serviceList></device></root>"#;
        assert_eq!(
            wan_service(description),
            Some((
                "urn:schemas-upnp-org:service:WANIPConnection:2".into(),
                "/ctl/IPConn".into()
            ))
        );
    }

    #[test]
    fn reads_soap_values_and_faults() {
        let reply = r#"<s:Envelope><s:Body><u:GetExternalIPAddressResponse>
            <NewExternalIPAddress>203.0.113.7</NewExternalIPAddress>
            </u:GetExternalIPAddressResponse></s:Body></s:Envelope>"#;
        assert_eq!(xml_text(reply, "NewExternalIPAddress"), Some("203.0.113.7"));
        let fault = r#"<s:Fault><detail><UPnPError xmlns="urn:schemas-upnp-org:control-1-0">
            <errorCode>718</errorCode></UPnPError></detail></s:Fault>"#;
        assert_eq!(xml_text(fault, "errorCode"), Some("718"));
    }

    #[test]
    fn rejects_addresses_behind_another_nat() {
        assert!(is_public(Ipv4Addr::new(58, 34, 7, 100)));
        assert!(!is_public(Ipv4Addr::new(100, 72, 1, 2)));
        assert!(!is_public(Ipv4Addr::new(192, 168, 1, 2)));
        assert!(!is_public(Ipv4Addr::new(10, 0, 0, 1)));
    }

    #[test]
    fn guesses_the_gateway() {
        assert_eq!(
            default_gateway_guess(Ipv4Addr::new(192, 168, 3, 27)),
            Some(Ipv4Addr::new(192, 168, 3, 1))
        );
        assert_eq!(default_gateway_guess(Ipv4Addr::new(192, 168, 3, 1)), None);
    }
}

#[cfg(test)]
mod fake_gateway {
    use super::*;
    use tokio::net::TcpListener;

    /// Serves the description and answers SOAP calls like a router whose
    /// external port 40123 is taken; returns the requests it saw.
    async fn serve(listener: TcpListener, requests: usize) -> Vec<String> {
        let mut seen = Vec::new();
        for _ in 0..requests {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0u8; 8192];
            let mut len = 0;
            // HTTP/1.0 with a body: read until the declared length arrived.
            loop {
                len += stream.read(&mut buffer[len..]).await.unwrap();
                let text = String::from_utf8_lossy(&buffer[..len]);
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let declared = head
                        .lines()
                        .find_map(|line| line.strip_prefix("Content-Length: "))
                        .map_or(0, |value| value.parse().unwrap());
                    if body.len() >= declared {
                        break;
                    }
                }
            }
            let request = String::from_utf8_lossy(&buffer[..len]).to_string();
            let (status, body) = if request.starts_with("GET /desc.xml") {
                (200, "<root><service><serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType><controlURL>/ctl</controlURL></service></root>".to_string())
            } else if request.contains("#GetExternalIPAddress") {
                (
                    200,
                    "<NewExternalIPAddress>58.34.7.100</NewExternalIPAddress>".to_string(),
                )
            } else if request.contains("<NewExternalPort>40123</NewExternalPort>")
                && request.contains("#AddPortMapping")
            {
                (
                    500,
                    "<UPnPError><errorCode>718</errorCode></UPnPError>".to_string(),
                )
            } else {
                (200, String::new())
            };
            let response = format!("HTTP/1.0 {status} X\r\n\r\n{body}");
            stream.write_all(response.as_bytes()).await.unwrap();
            seen.push(request);
        }
        seen
    }

    #[tokio::test]
    async fn maps_around_a_conflict_and_deletes() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(serve(listener, 5));

        let location = format!("http://127.0.0.1:{port}/desc.xml");
        let upnp = Upnp::describe(&location).await.unwrap();
        let local = Ipv4Addr::new(192, 168, 1, 20);
        let (external, lease) = upnp.map(local, 40_123).await.unwrap();
        assert_eq!(external.ip(), IpAddr::from([58, 34, 7, 100]));
        assert_ne!(external.port(), 40_123);
        assert_eq!(lease, LEASE_SECONDS);
        upnp.delete(external.port()).await;

        let seen = server.await.unwrap();
        assert!(seen[3].contains("<NewInternalClient>192.168.1.20</NewInternalClient>"));
        assert!(seen[4].contains("#DeletePortMapping"));
    }
}

#[cfg(test)]
mod live {
    /// Talks to the real router: `cargo test -p waystone-rtc --features
    /// port-mapping live -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore]
    async fn maps_a_port_on_this_network() {
        let interfaces = super::super::engine::mapping_interfaces(None);
        println!("interfaces: {interfaces:?}");
        let (result, mapped) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(super::hold(interfaces, 40_123, result, released));
        println!("mapping: {:?}", mapped.await);
        drop(release);
        task.await.unwrap();
    }
}
