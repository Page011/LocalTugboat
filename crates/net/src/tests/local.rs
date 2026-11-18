//! N1, N2, N5.

use iroh::{
    address_lookup::{AddressLookup, EndpointInfo, Item},
    SecretKey, TransportAddr,
};
use n0_future::StreamExt;

use crate::local::*;

/// N1: the spike's 24 addresses plus the shapes the design adds.
#[test]
fn n1_local_address_filter() {
    for ok in [
        "127.0.0.1:1", "10.0.2.2:1", "10.255.255.255:1", "172.16.0.1:1", "172.31.255.1:1", "192.168.1.5:1",
        "169.254.3.4:1", "[::1]:1", "[fd12:3456::1]:1", "[fc00::1]:1", "[fe80::1]:1", "[::ffff:192.168.1.5]:1",
        "[::ffff:127.0.0.1]:1", "[::ffff:10.1.2.3]:1",
    ] {
        assert!(is_local_addr(ok.parse().unwrap()), "{ok} should be local");
    }
    for bad in [
        "8.8.8.8:1", "1.1.1.1:1", "172.15.0.1:1", "172.32.0.1:1", "192.169.0.1:1", "11.0.0.1:1", "100.64.0.1:1",
        "0.0.0.0:1", "224.0.0.1:1", "255.255.255.255:1", "[2001:4860:4860::8888]:1", "[::ffff:8.8.8.8]:1", "[::]:1",
        "[fec0::1]:1", "[ff02::1]:1", "[2001:db8::1]:1",
    ] {
        assert!(!is_local_addr(bad.parse().unwrap()), "{bad} must not be local");
    }
}

/// N2: only IP addresses that are local pass; relays never do.
#[test]
fn n2_transport_addr_allowed_rejects_relays() {
    let relay: iroh::RelayUrl = "https://relay.example.com".parse().unwrap();
    assert!(!transport_addr_allowed(&TransportAddr::Relay(relay)));
    assert!(!transport_addr_allowed(&TransportAddr::Ip("8.8.8.8:1".parse().unwrap())));
    assert!(transport_addr_allowed(&TransportAddr::Ip("192.168.1.5:1".parse().unwrap())));
    assert!(transport_addr_allowed(&TransportAddr::Ip("127.0.0.1:1".parse().unwrap())));
}

/// A lookup that returns fixed items, standing in for mDNS.
#[derive(Debug)]
struct FakeLookup(Vec<Item>);

impl AddressLookup for FakeLookup {
    fn resolve(&self, _id: iroh::EndpointId) -> Option<n0_future::boxed::BoxStream<Result<Item, iroh::address_lookup::Error>>> {
        let items: Vec<Result<Item, iroh::address_lookup::Error>> = self.0.iter().cloned().map(Ok).collect();
        Some(Box::pin(n0_future::stream::iter(items)))
    }
}

/// N5: a spoofed lookup answer with public addresses is stripped before iroh sees it.
#[tokio::test]
async fn n5_lookup_results_are_stripped_to_local_addresses() {
    let id = SecretKey::generate().public();
    let info = EndpointInfo::new(id).with_ip_addrs(vec![
        "192.168.1.9:5000".parse().unwrap(),
        "8.8.8.8:5000".parse().unwrap(),
        "[2001:db8::1]:5000".parse().unwrap(),
        "10.1.2.3:5000".parse().unwrap(),
    ]);
    let wrapped = LocalOnlyLookup(FakeLookup(vec![Item::new(info, "test", None)]));
    let mut stream = wrapped.resolve(id).expect("stream");
    let item = stream.next().await.expect("one item").expect("ok item");
    let mut got: Vec<String> = item.ip_addrs().map(|a| a.to_string()).collect();
    got.sort();
    assert_eq!(got, vec!["10.1.2.3:5000".to_string(), "192.168.1.9:5000".to_string()]);
    assert!(stream.next().await.is_none());

    // An item with only public addresses ends up with none, so the dial fails instead of going out.
    let only_public = EndpointInfo::new(id).with_ip_addrs(vec!["1.1.1.1:5000".parse().unwrap()]);
    let wrapped = LocalOnlyLookup(FakeLookup(vec![Item::new(only_public, "test", None)]));
    let item = wrapped.resolve(id).unwrap().next().await.unwrap().unwrap();
    assert_eq!(item.ip_addrs().count(), 0);
}
