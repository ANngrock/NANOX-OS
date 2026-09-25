//! Fixed-capacity IPv4-over-Ethernet neighbor cache.
//!
//! The caller drives time and sends the returned ARP requests. Learning is an
//! unauthenticated observation: conflicting live mappings are reported and
//! never silently replace the existing entry, but this is not anti-spoofing.

use net_wire::{emit_arp, ArpOp, ArpPacket, Ipv4Addr, MacAddr, WireError};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArpConfig {
    /// Lifetime of a learned mapping, in caller-supplied monotonic ticks.
    pub reachable_ticks: u64,
    /// Delay between an unanswered request and its retry.
    pub retry_ticks: u64,
    /// Total requests, including the first request.
    pub max_attempts: u8,
}

impl Default for ArpConfig {
    fn default() -> Self {
        Self {
            reachable_ticks: 60_000,
            retry_ticks: 1_000,
            max_attempts: 3,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArpError {
    InvalidConfig,
    InvalidAddress,
    ClockRegressed,
    Wire(WireError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolve {
    /// A live mapping is available.
    Resolved(MacAddr),
    /// The caller should emit one broadcast ARP request now.
    SendRequest { attempt: u8 },
    /// A request is already outstanding; call `poll` to drive its retry.
    Pending,
    /// The fixed table has no empty or expired slot. Pending entries are never
    /// evicted to make room for an unrelated lookup.
    TableFull,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LearnOutcome {
    Inserted,
    Updated,
    Refreshed,
    /// A different live MAC was already associated with this IP; it remains
    /// installed and the new unauthenticated observation is rejected.
    Conflict {
        existing: MacAddr,
    },
    /// No empty or expired slot was available; no mapping was changed.
    TableFull,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArpEvent {
    Retry { ip: Ipv4Addr, attempt: u8 },
    TimedOut { ip: Ipv4Addr },
}

#[derive(Clone, Copy)]
enum State {
    Empty,
    Pending { attempts: u8, sent_at: u64 },
    Reachable { mac: MacAddr, learned_at: u64 },
}

#[derive(Clone, Copy)]
struct Entry {
    ip: Ipv4Addr,
    state: State,
}

impl Entry {
    const EMPTY: Self = Self {
        ip: Ipv4Addr::UNSPECIFIED,
        state: State::Empty,
    };
}

pub struct ArpCache<const N: usize> {
    config: ArpConfig,
    entries: [Entry; N],
    last_now: Option<u64>,
}

impl<const N: usize> ArpCache<N> {
    pub fn new(config: ArpConfig) -> Result<Self, ArpError> {
        if config.reachable_ticks == 0 || config.retry_ticks == 0 || config.max_attempts == 0 {
            return Err(ArpError::InvalidConfig);
        }
        Ok(Self {
            config,
            entries: [Entry::EMPTY; N],
            last_now: None,
        })
    }

    /// Resolve an address or start an ARP request. `now` must be monotonic
    /// across every call to this cache.
    pub fn resolve(&mut self, ip: Ipv4Addr, now: u64) -> Result<Resolve, ArpError> {
        validate_ip(ip)?;
        self.observe_time(now)?;
        self.expire(now);
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.ip == ip && !matches!(entry.state, State::Empty))
        {
            return Ok(match entry.state {
                State::Reachable { mac, .. } => Resolve::Resolved(mac),
                State::Pending { .. } => Resolve::Pending,
                State::Empty => return Ok(Resolve::TableFull),
            });
        }
        let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| matches!(entry.state, State::Empty))
        else {
            return Ok(Resolve::TableFull);
        };
        entry.ip = ip;
        entry.state = State::Pending {
            attempts: 1,
            sent_at: now,
        };
        Ok(Resolve::SendRequest { attempt: 1 })
    }

    /// Learn a mapping from a validated ARP packet or other explicit caller
    /// policy. Unsolicited learning is allowed but remains unauthenticated.
    pub fn learn(
        &mut self,
        ip: Ipv4Addr,
        mac: MacAddr,
        now: u64,
    ) -> Result<LearnOutcome, ArpError> {
        validate_ip(ip)?;
        validate_mac(mac)?;
        self.observe_time(now)?;
        self.expire(now);
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.ip == ip && !matches!(entry.state, State::Empty))
        {
            return Ok(match entry.state {
                State::Pending { .. } => {
                    entry.state = State::Reachable {
                        mac,
                        learned_at: now,
                    };
                    LearnOutcome::Updated
                }
                State::Reachable { mac: old, .. } if old == mac => {
                    entry.state = State::Reachable {
                        mac,
                        learned_at: now,
                    };
                    LearnOutcome::Refreshed
                }
                State::Reachable { mac: existing, .. } => LearnOutcome::Conflict { existing },
                State::Empty => return Ok(LearnOutcome::TableFull),
            });
        }
        let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| matches!(entry.state, State::Empty))
        else {
            return Ok(LearnOutcome::TableFull);
        };
        entry.ip = ip;
        entry.state = State::Reachable {
            mac,
            learned_at: now,
        };
        Ok(LearnOutcome::Inserted)
    }

    /// Drive one due retry/timeout. The caller should invoke this from its
    /// own timer loop and emit `Retry` as a broadcast ARP request.
    pub fn poll(&mut self, now: u64) -> Result<Option<ArpEvent>, ArpError> {
        self.observe_time(now)?;
        self.expire(now);
        for entry in &mut self.entries {
            let State::Pending { attempts, sent_at } = entry.state else {
                continue;
            };
            if now.saturating_sub(sent_at) < self.config.retry_ticks {
                continue;
            }
            if attempts >= self.config.max_attempts {
                let ip = entry.ip;
                *entry = Entry::EMPTY;
                return Ok(Some(ArpEvent::TimedOut { ip }));
            }
            let attempt = attempts + 1;
            entry.state = State::Pending {
                attempts: attempt,
                sent_at: now,
            };
            return Ok(Some(ArpEvent::Retry {
                ip: entry.ip,
                attempt,
            }));
        }
        Ok(None)
    }

    /// Construct the ARP payload for a request. The caller wraps it in an
    /// Ethernet broadcast frame and uses its local MAC/IP as the sender.
    pub fn emit_request(
        out: &mut [u8],
        local_mac: MacAddr,
        local_ip: Ipv4Addr,
        target_ip: Ipv4Addr,
    ) -> Result<usize, ArpError> {
        validate_ip(local_ip)?;
        validate_ip(target_ip)?;
        emit_arp(
            out,
            &ArpPacket {
                op: ArpOp::Request,
                sender_mac: local_mac,
                sender_ip: local_ip,
                target_mac: MacAddr([0; 6]),
                target_ip,
            },
        )
        .map_err(ArpError::Wire)
    }

    fn observe_time(&mut self, now: u64) -> Result<(), ArpError> {
        if self.last_now.is_some_and(|last| now < last) {
            return Err(ArpError::ClockRegressed);
        }
        self.last_now = Some(now);
        Ok(())
    }

    fn expire(&mut self, now: u64) {
        for entry in &mut self.entries {
            if let State::Reachable { learned_at, .. } = entry.state {
                if now.saturating_sub(learned_at) >= self.config.reachable_ticks {
                    *entry = Entry::EMPTY;
                }
            }
        }
    }
}

fn validate_ip(ip: Ipv4Addr) -> Result<(), ArpError> {
    if ip == Ipv4Addr::UNSPECIFIED || ip.is_group() {
        Err(ArpError::InvalidAddress)
    } else {
        Ok(())
    }
}

fn validate_mac(mac: MacAddr) -> Result<(), ArpError> {
    if mac.is_multicast() || mac.0 == [0; 6] {
        Err(ArpError::InvalidAddress)
    } else {
        Ok(())
    }
}
