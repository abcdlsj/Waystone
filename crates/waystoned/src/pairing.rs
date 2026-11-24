//! Pairing credentials: token issuance, the pairing URI and its QR rendering.

use anyhow::Result;

use waystone_proto::device::Role;
use waystone_proto::pairing::PairingUri;

use crate::store::{Store, PAIRING_TOKEN_TTL};

/// Creates a pairing URI for a device that is about to enroll.
///
/// `endpoint` is the authority the device will connect to (host and port),
/// `turn` the STUN/TURN authority and `fingerprint` the pinned certificate digest.
pub fn create(
    store: &Store,
    endpoint: &str,
    turn: &str,
    fingerprint: &str,
    role: Role,
) -> Result<PairingUri> {
    let token = store.create_pairing_token(role, token_ttl())?;
    Ok(PairingUri {
        endpoint: endpoint.to_string(),
        turn: turn.to_string(),
        fingerprint: fingerprint.to_string(),
        token: token.token,
    })
}

/// How long a freshly issued pairing token stays usable.
///
/// The default is short on purpose: the URI is displayed on screen and anybody
/// who photographs it can enroll. `WAYSTONE_PAIR_TTL` exists for development
/// (baking a URI into a device build, where build and install take longer than
/// the token lives).
fn token_ttl() -> std::time::Duration {
    match std::env::var("WAYSTONE_PAIR_TTL") {
        Ok(value) => value
            .trim()
            .parse::<u64>()
            .map(std::time::Duration::from_secs)
            .unwrap_or(PAIRING_TOKEN_TTL),
        Err(_) => PAIRING_TOKEN_TTL,
    }
}

/// Renders a pairing URI as a QR code, ready to be embedded in the panel.
///
/// The result is an SVG document whose modules are single paths, which keeps the
/// payload tiny and lets the panel scale it with CSS.
pub fn qr_svg(uri: &str) -> Result<String> {
    use qrcode::render::svg;
    use qrcode::QrCode;

    let code = QrCode::new(uri.as_bytes())?;
    Ok(code
        .render::<svg::Color>()
        .min_dimensions(240, 240)
        .dark_color(svg::Color("#0c0c0c"))
        .light_color(svg::Color("#ffffff"))
        .build())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        let mut store = Store::open(":memory:").unwrap();
        store.ensure_instance("server", &"f".repeat(64)).unwrap();
        store
    }

    #[test]
    fn creates_a_parsable_uri() {
        let store = store();
        let uri = create(
            &store,
            "192.168.1.10:8443",
            "192.168.1.10:3478",
            &"a".repeat(64),
            Role::Controller,
        )
        .unwrap();

        let text = uri.to_uri();
        assert!(text.starts_with("waystone://pair?"));
        let parsed: PairingUri = text.parse().unwrap();
        assert_eq!(parsed, uri);
        assert!(parsed.fingerprint_is_well_formed());

        // The token in the URI is the one stored and is immediately usable.
        assert_eq!(
            store.consume_pairing_token(&parsed.token).unwrap(),
            Some(Role::Controller)
        );
    }

    #[test]
    fn renders_svg() {
        let svg = qr_svg("waystone://pair?ep=a:1&turn=a:2&fp=00&tk=0123456789abcdef").unwrap();
        assert!(svg.contains("<svg"), "expected inline SVG");
        assert!(svg.contains("<path"));
    }
}
