use net_stack::dns::{DnsClient, DnsClientError, DnsConfig, DnsPoll, DnsResponse};
use net_wire::{dns, Ipv4Addr};

fn server() -> Ipv4Addr {
    Ipv4Addr([192, 0, 2, 53])
}

fn make_a_response(query: &[u8], id: u16, answer: [u8; 4]) -> Vec<u8> {
    let mut response = Vec::from(query);
    response[0..2].copy_from_slice(&id.to_be_bytes());
    response[2..4].copy_from_slice(&0x8180u16.to_be_bytes());
    response[6..8].copy_from_slice(&1u16.to_be_bytes());
    response.extend_from_slice(&[
        0xc0, 0x0c, // owner name points to QNAME
        0, 1, // A
        0, 1, // IN
        0, 0, 0, 60, // TTL
        0, 4, // RDLENGTH
    ]);
    response.extend_from_slice(&answer);
    response
}

fn client() -> DnsClient {
    DnsClient::new(
        DnsConfig {
            initial_timeout: 10,
            max_timeout: 40,
            max_attempts: 3,
        },
        0x1200,
    )
    .unwrap()
}

#[test]
fn query_retries_rotate_id_and_stale_reply_is_ignored() {
    let mut client = client();
    let name = dns::DnsName::from_text("example.test").unwrap();
    client.start(name, server(), 53000, 1).unwrap();
    let mut query = [0u8; 300];
    let DnsPoll::Send(first) = client.poll(1, &mut query).unwrap() else {
        panic!("initial query was not emitted");
    };
    assert_eq!(first.id, 0x1200);
    assert_eq!(first.attempt, 1);
    assert_eq!(first.destination_port, 53);
    assert_eq!(client.poll(10, &mut query).unwrap(), DnsPoll::Waiting);
    let DnsPoll::Send(second) = client.poll(11, &mut query).unwrap() else {
        panic!("first retry was not emitted");
    };
    assert_ne!(first.id, second.id);
    assert_eq!(second.attempt, 2);
    let stale = make_a_response(&query[..second.len], first.id, [203, 0, 113, 1]);
    let mut addrs = [Ipv4Addr([9, 9, 9, 9]); 2];
    assert_eq!(
        client
            .on_response(server(), 53, 53000, &stale, 12, &mut addrs)
            .unwrap(),
        DnsResponse::Ignored
    );
    assert!(client.is_pending());
}

#[test]
fn validates_endpoint_and_completes_only_a_valid_answer() {
    let mut client = client();
    let name = dns::DnsName::from_text("example.test").unwrap();
    client.start(name, server(), 53000, 0).unwrap();
    let mut query = [0u8; 300];
    let DnsPoll::Send(sent) = client.poll(0, &mut query).unwrap() else {
        panic!("query was not emitted");
    };
    let response = make_a_response(&query[..sent.len], sent.id, [203, 0, 113, 7]);
    let mut addrs = [Ipv4Addr::UNSPECIFIED; 2];
    assert_eq!(
        client
            .on_response(
                Ipv4Addr([192, 0, 2, 54]),
                53,
                53000,
                &response,
                1,
                &mut addrs
            )
            .unwrap(),
        DnsResponse::Ignored
    );
    assert_eq!(
        client
            .on_response(server(), 53000, 53000, &response, 2, &mut addrs)
            .unwrap(),
        DnsResponse::Ignored
    );
    assert_eq!(
        client
            .on_response(server(), 53, 53000, &response, 3, &mut addrs)
            .unwrap(),
        DnsResponse::Completed(Ok(dns::AResult { count: 1, ttl: 60 }))
    );
    assert_eq!(addrs[0], Ipv4Addr([203, 0, 113, 7]));
    assert!(!client.is_pending());
}

#[test]
fn malformed_packet_keeps_transaction_and_rcode_is_terminal() {
    let mut client = client();
    let name = dns::DnsName::from_text("bad.example").unwrap();
    client.start(name, server(), 53000, 0).unwrap();
    let mut query = [0u8; 300];
    let DnsPoll::Send(sent) = client.poll(0, &mut query).unwrap() else {
        panic!("query was not emitted");
    };
    let mut addrs = [Ipv4Addr::UNSPECIFIED; 1];
    assert_eq!(
        client
            .on_response(server(), 53, 53000, &sent.id.to_be_bytes(), 1, &mut addrs)
            .unwrap(),
        DnsResponse::Rejected(dns::DnsError::Truncated)
    );
    assert!(client.is_pending());
    let mut nxdomain = Vec::from(&query[..sent.len]);
    nxdomain[0..2].copy_from_slice(&sent.id.to_be_bytes());
    nxdomain[2..4].copy_from_slice(&0x8183u16.to_be_bytes());
    assert_eq!(
        client
            .on_response(server(), 53, 53000, &nxdomain, 2, &mut addrs)
            .unwrap(),
        DnsResponse::Completed(Err(dns::DnsError::NameError))
    );
    assert!(!client.is_pending());
}

#[test]
fn bounded_exponential_retry_sequence_times_out() {
    let mut client = client();
    client
        .start(
            dns::DnsName::from_text("retry.test").unwrap(),
            server(),
            53000,
            0,
        )
        .unwrap();
    let mut query = [0u8; 300];
    assert!(matches!(client.poll(0, &mut query), Ok(DnsPoll::Send(_))));
    assert!(matches!(client.poll(10, &mut query), Ok(DnsPoll::Send(_))));
    assert_eq!(client.poll(29, &mut query), Ok(DnsPoll::Waiting));
    assert!(matches!(client.poll(30, &mut query), Ok(DnsPoll::Send(_))));
    assert_eq!(client.poll(69, &mut query), Ok(DnsPoll::Waiting));
    assert_eq!(client.poll(70, &mut query), Ok(DnsPoll::TimedOut));
    assert!(!client.is_pending());
}

#[test]
fn rejects_bad_config_busy_transaction_and_time_regression() {
    assert!(matches!(
        DnsClient::new(
            DnsConfig {
                initial_timeout: 0,
                ..DnsConfig::default()
            },
            1
        ),
        Err(DnsClientError::InvalidConfig)
    ));
    let mut client = client();
    let name = dns::DnsName::from_text("busy.test").unwrap();
    client.start(name, server(), 53000, 5).unwrap();
    assert_eq!(
        client.start(name, server(), 53000, 5),
        Err(DnsClientError::Busy)
    );
    assert_eq!(
        client.poll(4, &mut [0u8; 64]),
        Err(DnsClientError::ClockRegressed)
    );
}
