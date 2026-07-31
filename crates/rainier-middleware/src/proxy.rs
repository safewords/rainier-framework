//! [`TrustProxies`] — deciding which client address to believe.
//!
//! `X-Forwarded-For` is written by whoever is in front of you, and *appended
//! to* by every hop. It is therefore only worth anything if you know which
//! hops are yours — which is what this middleware is for.

use std::net::IpAddr;

use rainier_http::{ClientIp, Request, Response};

use crate::pipeline::{Middleware, Next};

/// One trusted address or network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cidr {
    address: IpAddr,
    prefix: u8,
}

impl Cidr {
    /// Parse `10.0.0.0/8`, `192.168.1.7`, `::1`, `fd00::/8`.
    ///
    /// A bare address is treated as a single-host network.
    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        let (address, prefix) = match value.split_once('/') {
            Some((address, prefix)) => (address, Some(prefix)),
            None => (value, None),
        };

        let address: IpAddr = address.parse().ok()?;
        let full = if address.is_ipv4() { 32 } else { 128 };

        let prefix = match prefix {
            Some(prefix) => {
                let prefix: u8 = prefix.parse().ok()?;
                if prefix > full {
                    return None;
                }
                prefix
            }
            None => full,
        };

        Some(Self { address, prefix })
    }

    /// Whether `candidate` falls inside this network.
    pub fn contains(&self, candidate: IpAddr) -> bool {
        match (self.address, candidate) {
            (IpAddr::V4(network), IpAddr::V4(candidate)) => {
                matches(&network.octets(), &candidate.octets(), self.prefix)
            }
            (IpAddr::V6(network), IpAddr::V6(candidate)) => {
                matches(&network.octets(), &candidate.octets(), self.prefix)
            }
            // A v4 address is not inside a v6 network, or the other way round.
            // Comparing them by mapping would let `::ffff:10.0.0.1` slip
            // through a `10.0.0.0/8` rule from an unexpected direction.
            _ => false,
        }
    }
}

/// Whether the first `prefix` bits of two addresses agree.
fn matches(network: &[u8], candidate: &[u8], prefix: u8) -> bool {
    let whole = (prefix / 8) as usize;
    if network[..whole] != candidate[..whole] {
        return false;
    }

    let remaining = prefix % 8;
    if remaining == 0 {
        return true;
    }

    let mask = 0xffu8 << (8 - remaining);
    network[whole] & mask == candidate[whole] & mask
}

/// Which proxies to believe.
#[derive(Debug, Clone, Default)]
pub enum Trusted {
    /// Believe nobody. `X-Forwarded-For` is ignored entirely.
    #[default]
    None,
    /// Believe whoever connected.
    ///
    /// Correct **only** when nothing but your proxy can reach the process. If
    /// the port is open to the internet, this lets any client name its own
    /// address — which defeats [rate limiting](crate::ThrottleRequests) and
    /// falsifies every access log you have.
    All,
    /// Believe these addresses and networks.
    These(Vec<Cidr>),
}

impl Trusted {
    /// Whether a connection from `peer` may set the forwarded header at all.
    pub fn accepts(&self, peer: IpAddr) -> bool {
        match self {
            Trusted::None => false,
            Trusted::All => true,
            Trusted::These(networks) => networks.iter().any(|network| network.contains(peer)),
        }
    }

    /// Whether `address` is one of *our* proxies, and so should be skipped
    /// when looking for the client in the chain.
    ///
    /// Distinct from [`accepts`](Self::accepts), and the difference matters:
    /// [`Trusted::All`] says "believe whoever connected" but names no
    /// addresses, so it cannot recognise a hop. Treating every entry as a
    /// known proxy would skip the whole chain and find no client at all.
    pub fn is_known_hop(&self, address: IpAddr) -> bool {
        match self {
            Trusted::None | Trusted::All => false,
            Trusted::These(networks) => networks.iter().any(|network| network.contains(address)),
        }
    }
}

/// Rewrites the client address from `X-Forwarded-For`, when the peer is a
/// proxy you trust.
///
/// ```
/// use rainier_middleware::TrustProxies;
///
/// // Behind a load balancer on a private network.
/// let middleware = TrustProxies::these(["10.0.0.0/8", "172.16.0.0/12"]);
///
/// // Behind a proxy on the same host, and nothing else can reach the port.
/// let middleware = TrustProxies::all();
/// ```
#[derive(Debug, Clone, Default)]
pub struct TrustProxies {
    trusted: Trusted,
    header: String,
}

impl TrustProxies {
    /// Trust nobody — the default, and a no-op.
    pub fn new() -> Self {
        Self { trusted: Trusted::None, header: "x-forwarded-for".to_string() }
    }

    /// Trust whoever connected. See [`Trusted::All`] before reaching for this.
    pub fn all() -> Self {
        Self { trusted: Trusted::All, ..Self::new() }
    }

    /// Trust these addresses and networks. Unparseable entries are dropped.
    pub fn these(networks: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        let parsed = networks
            .into_iter()
            .filter_map(|network| {
                let network = network.as_ref();
                let parsed = Cidr::parse(network);
                if parsed.is_none() {
                    tracing::warn!(%network, "ignoring an unparseable trusted proxy");
                }
                parsed
            })
            .collect();

        Self { trusted: Trusted::These(parsed), ..Self::new() }
    }

    /// Read the chain from a different header.
    pub fn header(mut self, header: impl Into<String>) -> Self {
        self.header = header.into().to_ascii_lowercase();
        self
    }

    /// Whether anything is trusted.
    pub fn trusts_anything(&self) -> bool {
        !matches!(self.trusted, Trusted::None)
    }

    /// The client address `request` should be credited to.
    ///
    /// Walks the chain from the **right**, skipping addresses that are
    /// themselves trusted proxies; the first untrusted one is the client.
    ///
    /// Taking the leftmost entry instead — the obvious reading of "the
    /// original client" — is what makes this spoofable: the left of the chain
    /// is whatever the client sent, and a client can send anything. Only the
    /// entries your own proxies appended are worth believing, and those are on
    /// the right.
    pub fn resolve(&self, request: &Request, peer: IpAddr) -> IpAddr {
        if !self.trusted.accepts(peer) {
            return peer;
        }

        let Some(header) = request.header(&self.header) else {
            return peer;
        };

        header
            .split(',')
            .rev()
            .filter_map(|entry| entry.trim().parse::<IpAddr>().ok())
            .find(|address| !self.trusted.is_known_hop(*address))
            .unwrap_or(peer)
    }
}

#[async_trait::async_trait]
impl Middleware for TrustProxies {
    async fn handle(&self, mut request: Request, next: Next) -> Response {
        if let Some(peer) = request.ip() {
            let client = self.resolve(&request, peer);
            if client != peer {
                request.extensions_mut().insert(ClientIp(client));
            }
        }
        next.run(request).await
    }

    fn name(&self) -> &'static str {
        "TrustProxies"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(value: &str) -> IpAddr {
        value.parse().unwrap()
    }

    fn request(chain: &str) -> Request {
        Request::builder().header("x-forwarded-for", chain).build()
    }

    #[test]
    fn a_bare_address_is_a_single_host() {
        let cidr = Cidr::parse("192.168.1.7").unwrap();

        assert!(cidr.contains(ip("192.168.1.7")));
        assert!(!cidr.contains(ip("192.168.1.8")));
    }

    #[test]
    fn a_prefix_matches_its_network() {
        let cidr = Cidr::parse("10.0.0.0/8").unwrap();

        assert!(cidr.contains(ip("10.0.0.1")));
        assert!(cidr.contains(ip("10.255.255.255")));
        assert!(!cidr.contains(ip("11.0.0.1")));
    }

    #[test]
    fn a_prefix_that_is_not_a_whole_byte_still_works() {
        let cidr = Cidr::parse("172.16.0.0/12").unwrap();

        assert!(cidr.contains(ip("172.16.0.1")));
        assert!(cidr.contains(ip("172.31.255.255")));
        assert!(!cidr.contains(ip("172.32.0.1")), "just outside the /12");
        assert!(!cidr.contains(ip("172.15.255.255")));
    }

    #[test]
    fn ipv6_works_too() {
        assert!(Cidr::parse("::1").unwrap().contains(ip("::1")));
        assert!(Cidr::parse("fd00::/8").unwrap().contains(ip("fd12:3456::1")));
        assert!(!Cidr::parse("fd00::/8").unwrap().contains(ip("fe80::1")));
    }

    #[test]
    fn families_do_not_cross() {
        assert!(!Cidr::parse("10.0.0.0/8").unwrap().contains(ip("::ffff:10.0.0.1")));
        assert!(!Cidr::parse("::/0").unwrap().contains(ip("10.0.0.1")));
    }

    #[test]
    fn rubbish_does_not_parse() {
        for bad in ["", "nonsense", "10.0.0.0/33", "10.0.0.0/-1", "::1/129", "10.0.0.0/x"] {
            assert!(Cidr::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn trusting_nobody_ignores_the_header() {
        let middleware = TrustProxies::new();

        assert!(!middleware.trusts_anything());
        assert_eq!(middleware.resolve(&request("1.2.3.4"), ip("10.0.0.9")), ip("10.0.0.9"));
    }

    #[test]
    fn an_untrusted_peer_is_believed_over_its_own_header() {
        // The client claims to be someone else. It is not.
        let middleware = TrustProxies::these(["10.0.0.0/8"]);

        assert_eq!(middleware.resolve(&request("1.2.3.4"), ip("203.0.113.9")), ip("203.0.113.9"));
    }

    #[test]
    fn a_trusted_peer_yields_the_client() {
        let middleware = TrustProxies::these(["10.0.0.0/8"]);

        assert_eq!(middleware.resolve(&request("203.0.113.9"), ip("10.0.0.1")), ip("203.0.113.9"));
    }

    #[test]
    fn trusted_hops_are_walked_past_from_the_right() {
        // client, then two of our own proxies appended themselves.
        let middleware = TrustProxies::these(["10.0.0.0/8"]);
        let chain = request("203.0.113.9, 10.0.0.5, 10.0.0.6");

        assert_eq!(middleware.resolve(&chain, ip("10.0.0.1")), ip("203.0.113.9"));
    }

    #[test]
    fn a_client_cannot_forge_an_address_by_prepending_one() {
        // The client sent `X-Forwarded-For: 1.2.3.4`; our proxy appended the
        // client's real address after it. Reading from the left would believe
        // the forgery.
        let middleware = TrustProxies::these(["10.0.0.0/8"]);
        let chain = request("1.2.3.4, 203.0.113.9, 10.0.0.5");

        assert_eq!(
            middleware.resolve(&chain, ip("10.0.0.1")),
            ip("203.0.113.9"),
            "the rightmost untrusted entry is the only one our own proxy vouched for"
        );
    }

    #[test]
    fn a_chain_of_only_trusted_hops_falls_back_to_the_peer() {
        let middleware = TrustProxies::these(["10.0.0.0/8"]);
        let chain = request("10.0.0.5, 10.0.0.6");

        assert_eq!(middleware.resolve(&chain, ip("10.0.0.1")), ip("10.0.0.1"));
    }

    #[test]
    fn rubbish_entries_are_skipped() {
        let middleware = TrustProxies::these(["10.0.0.0/8"]);
        let chain = request("203.0.113.9, unknown, 10.0.0.5");

        assert_eq!(middleware.resolve(&chain, ip("10.0.0.1")), ip("203.0.113.9"));
    }

    #[test]
    fn no_header_leaves_the_peer_alone() {
        let middleware = TrustProxies::these(["10.0.0.0/8"]);

        assert_eq!(middleware.resolve(&Request::builder().build(), ip("10.0.0.1")), ip("10.0.0.1"));
    }

    #[test]
    fn trusting_everything_believes_the_chain() {
        let middleware = TrustProxies::all();

        assert!(middleware.trusts_anything());
        assert_eq!(middleware.resolve(&request("203.0.113.9"), ip("10.0.0.1")), ip("203.0.113.9"));
    }

    #[test]
    fn trusting_everything_takes_the_rightmost_entry() {
        // `all()` names no addresses, so it cannot recognise a hop to skip.
        // The rightmost entry is the one the proxy appended, and the only one
        // worth anything.
        let middleware = TrustProxies::all();
        let chain = request("1.2.3.4, 203.0.113.9");

        assert_eq!(middleware.resolve(&chain, ip("10.0.0.1")), ip("203.0.113.9"));
    }

    #[test]
    fn an_alternative_header_can_be_read() {
        let middleware = TrustProxies::all().header("CF-Connecting-IP");
        let request = Request::builder().header("cf-connecting-ip", "203.0.113.9").build();

        assert_eq!(middleware.resolve(&request, ip("10.0.0.1")), ip("203.0.113.9"));
    }

    #[tokio::test]
    async fn the_middleware_rewrites_the_recorded_address() {
        use crate::pipeline::Pipeline;

        let request = Request::builder()
            .header("x-forwarded-for", "203.0.113.9, 10.0.0.5")
            .build()
            .with_extension(ClientIp(ip("10.0.0.1")));

        let response = Pipeline::new()
            .through(TrustProxies::these(["10.0.0.0/8"]))
            .then(|request: Request| async move {
                Response::text(request.ip().map(|ip| ip.to_string()).unwrap_or_default())
            })
            .run(request)
            .await;

        let body = response.into_http().into_body().collect().await.unwrap();
        assert_eq!(body, "203.0.113.9");
    }

    #[tokio::test]
    async fn a_request_with_no_recorded_address_passes_through() {
        use crate::pipeline::Pipeline;

        let response = Pipeline::new()
            .through(TrustProxies::all())
            .then(|request: Request| async move {
                Response::text(format!("{}", request.ip().is_none()))
            })
            .run(request("203.0.113.9"))
            .await;

        let body = response.into_http().into_body().collect().await.unwrap();
        assert_eq!(body, "true");
    }
}
