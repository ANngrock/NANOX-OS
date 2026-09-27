//! Single-flight DNS-over-UDP transport driver.
//!
//! Time and packet I/O are provided by the caller. Query IDs are rotated for
//! retries, but this deterministic allocator is not entropy and must not be
//! treated as a substitute for a CSPRNG or source-port randomization.

use net_wire::{dns, Ipv4Addr};

const DNS_PORT: u16 = 53;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DnsConfig {
    /// Initial retry delay in caller-supplied monotonic ticks.
    pub initial_timeout: u64,
    /// Maximum exponential-backoff delay.
    pub max_timeout: u64,
    /// Total UDP queries, including the initial query.
    pub max_attempts: u8,
}

impl Default for DnsConfig {
    fn default() -> Self {
        Self {
            initial_timeout: 1_000,
            max_timeout: 8_000,
            max_attempts: 4,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DnsClientError {
    InvalidConfig,
    InvalidEndpoint,
    Busy,
    ClockRegressed,
    Wire(dns::DnsError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DnsQuery {
    pub id: u16,
    pub server: Ipv4Addr,
    pub source_port: u16,
    pub destination_port: u16,
    pub attempt: u8,
    pub len: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DnsPoll {
    Idle,
    Waiting,
    Send(DnsQuery),
    TimedOut,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DnsResponse {
    /// Packet source/ports do not belong to the active transaction, or it has
    /// a stale query ID from an earlier retry.
    Ignored,
    /// Malformed or mismatched packet. The transaction remains pending so a
    /// valid response or timeout can still complete it.
    Rejected(dns::DnsError),
    /// A terminal DNS result; server RCODEs and TC are surfaced to the caller.
    Completed(core::result::Result<dns::AResult, dns::DnsError>),
}

#[derive(Clone, Copy)]
struct Transaction {
    name: dns::DnsName,
    server: Ipv4Addr,
    local_port: u16,
    id: Option<u16>,
    attempts: u8,
    sent_at: u64,
    timeout: u64,
}

pub struct DnsClient {
    config: DnsConfig,
    next_id: u16,
    transaction: Option<Transaction>,
    last_now: Option<u64>,
}

impl DnsClient {
    pub fn new(config: DnsConfig, id_seed: u16) -> Result<Self, DnsClientError> {
        if config.initial_timeout == 0
            || config.max_timeout < config.initial_timeout
            || config.max_attempts == 0
        {
            return Err(DnsClientError::InvalidConfig);
        }
        Ok(Self {
            config,
            next_id: id_seed,
            transaction: None,
            last_now: None,
        })
    }

    /// Begin one lookup. The caller must supply a nonzero local UDP port and
    /// a unicast IPv4 DNS server. Only one request may be active at a time.
    pub fn start(
        &mut self,
        name: dns::DnsName,
        server: Ipv4Addr,
        local_port: u16,
        now: u64,
    ) -> Result<(), DnsClientError> {
        self.observe_time(now)?;
        if self.transaction.is_some() {
            return Err(DnsClientError::Busy);
        }
        if local_port == 0 || server == Ipv4Addr::UNSPECIFIED || server.is_group() {
            return Err(DnsClientError::InvalidEndpoint);
        }
        self.transaction = Some(Transaction {
            name,
            server,
            local_port,
            id: None,
            attempts: 0,
            sent_at: now,
            timeout: self.config.initial_timeout,
        });
        Ok(())
    }

    /// Emit the initial query or a due retry into caller storage.
    pub fn poll(&mut self, now: u64, out: &mut [u8]) -> Result<DnsPoll, DnsClientError> {
        self.observe_time(now)?;
        let Some(mut tx) = self.transaction else {
            return Ok(DnsPoll::Idle);
        };
        if tx.attempts != 0 && now.saturating_sub(tx.sent_at) < tx.timeout {
            return Ok(DnsPoll::Waiting);
        }
        if tx.attempts >= self.config.max_attempts {
            self.transaction = None;
            return Ok(DnsPoll::TimedOut);
        }

        let id = match tx.id {
            None => self.next_id,
            Some(_) => self.next_id.wrapping_add(0x9e37),
        };
        let len = dns::emit_a_query(out, id, &tx.name).map_err(DnsClientError::Wire)?;
        self.next_id = id;
        tx.id = Some(id);
        tx.attempts += 1;
        tx.sent_at = now;
        if tx.attempts > 1 {
            tx.timeout = tx.timeout.saturating_mul(2).min(self.config.max_timeout);
        }
        let query = DnsQuery {
            id,
            server: tx.server,
            source_port: tx.local_port,
            destination_port: DNS_PORT,
            attempt: tx.attempts,
            len,
        };
        self.transaction = Some(tx);
        Ok(DnsPoll::Send(query))
    }

    /// Validate a UDP reply from the active DNS endpoint. Invalid or stale
    /// traffic cannot complete the lookup. RCODE/TC results are terminal and
    /// are returned distinctly to the caller for policy (e.g. TCP fallback).
    pub fn on_response(
        &mut self,
        source: Ipv4Addr,
        source_port: u16,
        destination_port: u16,
        message: &[u8],
        now: u64,
        out: &mut [Ipv4Addr],
    ) -> Result<DnsResponse, DnsClientError> {
        self.observe_time(now)?;
        let Some(tx) = self.transaction else {
            return Ok(DnsResponse::Ignored);
        };
        if source != tx.server || source_port != DNS_PORT || destination_port != tx.local_port {
            return Ok(DnsResponse::Ignored);
        }
        let Some(id) = tx.id else {
            return Ok(DnsResponse::Ignored);
        };
        if message.len() < 2 {
            return Ok(DnsResponse::Rejected(dns::DnsError::Truncated));
        }
        if u16::from_be_bytes([message[0], message[1]]) != id {
            return Ok(DnsResponse::Ignored);
        }
        match dns::parse_a_response(message, id, &tx.name, out) {
            Ok(answer) => {
                self.transaction = None;
                Ok(DnsResponse::Completed(Ok(answer)))
            }
            Err(error) if is_terminal(error) => {
                self.transaction = None;
                Ok(DnsResponse::Completed(Err(error)))
            }
            Err(error) => Ok(DnsResponse::Rejected(error)),
        }
    }

    pub fn active_id(&self) -> Option<u16> {
        self.transaction.and_then(|tx| tx.id)
    }

    pub fn is_pending(&self) -> bool {
        self.transaction.is_some()
    }

    fn observe_time(&mut self, now: u64) -> Result<(), DnsClientError> {
        if self.last_now.is_some_and(|last| now < last) {
            return Err(DnsClientError::ClockRegressed);
        }
        self.last_now = Some(now);
        Ok(())
    }
}

fn is_terminal(error: dns::DnsError) -> bool {
    matches!(
        error,
        dns::DnsError::TruncatedResponse
            | dns::DnsError::NameError
            | dns::DnsError::ServerFailure
            | dns::DnsError::Refused
            | dns::DnsError::OtherRcode(_)
    )
}
