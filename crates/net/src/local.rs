//! Local-network address filter. Applied to: every dial target (`node::Node::dial`),
//! every mDNS result (`LocalOnly` wrapper), the addresses this endpoint publishes
//! (`local_addr_filter` on the builder), the QR payload (`pairing::PairPayload::parse`),
//! and every live connection path (`guard`).
//!
//! Rule: local = loopback, RFC1918 (10/8, 172.16/12, 192.168/16),
//! IPv4 link-local (169.254/16), IPv6 link-local (fe80::/10), IPv6 ULA (fc00::/7).
//! IPv4-mapped IPv6 (::ffff:a.b.c.d) is unwrapped first and judged as IPv4.
//! Everything else, including relay and custom transport addresses, is not local.

use std::{
    borrow::Cow,
    net::{IpAddr, SocketAddr},
};

use iroh::{
    address_lookup::{
        AddrFilter, AddressLookup, AddressLookupBuilder, AddressLookupBuilderError, EndpointData, Error as LookupError,
        Item,
    },
    Endpoint, EndpointId, TransportAddr,
};
use n0_future::{boxed::BoxStream, StreamExt};
use tracing::debug;

/// True only for loopback, RFC1918, link-local (v4 and v6) and IPv6 ULA; IPv4-mapped unwrapped.
pub fn is_local_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => v.is_loopback() || v.is_private() || v.is_link_local(),
        IpAddr::V6(v) => match v.to_ipv4_mapped() {
            Some(m) => is_local_ip(IpAddr::V4(m)),
            None => v.is_loopback() || v.is_unique_local() || v.is_unicast_link_local(),
        },
    }
}

/// `is_local_ip` on the socket address's IP.
pub fn is_local_addr(a: SocketAddr) -> bool {
    is_local_ip(a.ip())
}

/// True only for `TransportAddr::Ip(a)` with `is_local_addr(a)`. Relay and any other
/// transport address kinds are never allowed (relays are off; seeing one is a bug).
pub fn transport_addr_allowed(t: &TransportAddr) -> bool {
    match t {
        TransportAddr::Ip(a) => is_local_addr(*a),
        _ => false,
    }
}

/// Keeps only the allowed entries of an address set; logs what it drops.
fn keep_local(addrs: &[TransportAddr]) -> Vec<TransportAddr> {
    addrs
        .iter()
        .filter(|t| {
            let ok = transport_addr_allowed(t);
            if !ok {
                debug!(addr = %t, "dropped non-local address");
            }
            ok
        })
        .cloned()
        .collect()
}

/// AddrFilter that keeps only `transport_addr_allowed` entries. Installed with
/// `Endpoint::builder(..).addr_filter(..)` (what this endpoint publishes to address lookup)
/// and with `MdnsAddressLookupBuilder::addr_filter(..)` (what mDNS advertises), so the PC
/// never advertises a public address over mDNS.
pub fn local_addr_filter() -> AddrFilter {
    AddrFilter::new(|addrs| Cow::Owned(keep_local(addrs)))
}

/// Strips non-local addresses from one lookup result.
pub fn local_only_item(item: Item) -> Item {
    let f = local_addr_filter();
    let mut info = item.endpoint_info().clone();
    info.data = info.data.apply_filter(&f).into_owned();
    Item::new(info, item.provenance(), item.last_updated())
}

/// Wraps an address lookup so everything it resolves is stripped of non-local IPs and relays
/// before iroh sees it (a spoofed mDNS answer cannot point a dial at the internet).
#[derive(Debug)]
pub struct LocalOnly<B>(pub B);

impl<B: AddressLookupBuilder> AddressLookupBuilder for LocalOnly<B> {
    fn into_address_lookup(self, endpoint: &Endpoint) -> Result<impl AddressLookup, AddressLookupBuilderError> {
        Ok(LocalOnlyLookup(self.0.into_address_lookup(endpoint)?))
    }
}

/// The resolved-lookup half of `LocalOnly`.
#[derive(Debug)]
pub struct LocalOnlyLookup<T>(pub T);

impl<T: AddressLookup> AddressLookup for LocalOnlyLookup<T> {
    fn publish(&self, data: &EndpointData) {
        self.0.publish(data)
    }

    fn resolve(&self, endpoint_id: EndpointId) -> Option<BoxStream<Result<Item, LookupError>>> {
        let inner = self.0.resolve(endpoint_id)?;
        Some(Box::pin(inner.map(|r| r.map(local_only_item))))
    }
}
