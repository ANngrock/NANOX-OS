use net_stack::arp::{ArpCache, ArpConfig, ArpError, ArpEvent, LearnOutcome, Resolve};
use net_wire::{parse_arp, Ipv4Addr, MacAddr};

fn ip(last: u8) -> Ipv4Addr {
    Ipv4Addr([10, 0, 0, last])
}

fn mac(last: u8) -> MacAddr {
    MacAddr([2, 0, 0, 0, 0, last])
}

fn config() -> ArpConfig {
    ArpConfig {
        reachable_ticks: 20,
        retry_ticks: 5,
        max_attempts: 2,
    }
}

#[test]
fn resolve_learn_conflict_expiry_and_capacity() {
    let mut cache = ArpCache::<1>::new(config()).unwrap();
    assert_eq!(
        cache.resolve(ip(2), 1),
        Ok(Resolve::SendRequest { attempt: 1 })
    );
    assert_eq!(cache.resolve(ip(3), 1), Ok(Resolve::TableFull));
    assert_eq!(cache.learn(ip(2), mac(2), 2), Ok(LearnOutcome::Updated));
    assert_eq!(cache.resolve(ip(2), 3), Ok(Resolve::Resolved(mac(2))));
    assert_eq!(
        cache.learn(ip(2), mac(3), 4),
        Ok(LearnOutcome::Conflict { existing: mac(2) })
    );
    assert_eq!(cache.learn(ip(3), mac(3), 22), Ok(LearnOutcome::Inserted));
    assert_eq!(cache.resolve(ip(3), 22), Ok(Resolve::Resolved(mac(3))));
}

#[test]
fn full_learn_is_not_reported_as_a_fake_conflict() {
    let mut cache = ArpCache::<1>::new(config()).unwrap();
    assert_eq!(cache.learn(ip(2), mac(2), 1), Ok(LearnOutcome::Inserted));
    assert_eq!(cache.learn(ip(3), mac(3), 2), Ok(LearnOutcome::TableFull));
    assert_eq!(cache.resolve(ip(2), 3), Ok(Resolve::Resolved(mac(2))));
}

#[test]
fn retries_then_times_out_without_evicting_other_pending_entries() {
    let mut cache = ArpCache::<2>::new(config()).unwrap();
    assert_eq!(
        cache.resolve(ip(2), 0),
        Ok(Resolve::SendRequest { attempt: 1 })
    );
    assert_eq!(
        cache.resolve(ip(3), 0),
        Ok(Resolve::SendRequest { attempt: 1 })
    );
    assert_eq!(cache.poll(4), Ok(None));
    assert_eq!(
        cache.poll(5),
        Ok(Some(ArpEvent::Retry {
            ip: ip(2),
            attempt: 2
        }))
    );
    assert_eq!(cache.poll(10), Ok(Some(ArpEvent::TimedOut { ip: ip(2) })));
    assert_eq!(cache.resolve(ip(3), 10), Ok(Resolve::Pending));
}

#[test]
fn rejects_invalid_addresses_clock_regression_and_config() {
    assert!(matches!(
        ArpCache::<2>::new(ArpConfig {
            retry_ticks: 0,
            ..config()
        }),
        Err(ArpError::InvalidConfig)
    ));
    let mut cache = ArpCache::<2>::new(config()).unwrap();
    assert_eq!(
        cache.resolve(Ipv4Addr::UNSPECIFIED, 3),
        Err(ArpError::InvalidAddress)
    );
    assert_eq!(
        cache.learn(ip(2), MacAddr([1, 0, 0, 0, 0, 2]), 3),
        Err(ArpError::InvalidAddress)
    );
    cache.resolve(ip(2), 4).unwrap();
    assert_eq!(cache.poll(3), Err(ArpError::ClockRegressed));
}

#[test]
fn emitted_request_is_an_arp_query_for_the_target() {
    let mut bytes = [0u8; 28];
    let len = ArpCache::<2>::emit_request(&mut bytes, mac(1), ip(1), ip(2)).unwrap();
    let packet = parse_arp(&bytes[..len]).unwrap();
    assert_eq!(packet.op, net_wire::ArpOp::Request);
    assert_eq!(packet.sender_mac, mac(1));
    assert_eq!(packet.sender_ip, ip(1));
    assert_eq!(packet.target_ip, ip(2));
}
