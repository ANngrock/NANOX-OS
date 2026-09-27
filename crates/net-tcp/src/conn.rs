//! Active-open TCP client connection (profile: docs/specs/M5-TCP.md).

use crate::ring::Ring;
use crate::segment::{
    emit_in_place, emit_tcp, SegmentError, TcpHeader, TcpSegment, ACK, FIN, PSH, RST, SYN,
    TCP_HEADER_LEN,
};
use crate::seq;
use net_wire::Ipv4Addr;

/// MSS assumed when the peer's SYN carries no MSS option (RFC 9293 3.7.1).
const DEFAULT_PEER_MSS: u16 = 536;
/// Largest advertisable window without window scaling.
const MAX_WINDOW: usize = u16::MAX as usize;
/// Clock granularity G of RFC 6298, in milliseconds.
const CLOCK_GRANULARITY_MS: u64 = 1;
const MAX_CWND: u32 = 1 << 30;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Closed,
    SynSent,
    Established,
    FinWait1,
    FinWait2,
    CloseWait,
    Closing,
    LastAck,
    TimeWait,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TcpError {
    /// Rejected `Config`, empty buffer, port 0 or group remote address.
    InvalidConfig,
    /// `send` after `close`, or in a state that cannot send.
    SendClosed,
    /// RST answering our SYN.
    Refused,
    /// Acceptable RST in a synchronized state.
    Reset,
    /// Retransmission limit exceeded.
    TimedOut,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    /// MSS we announce and the upper bound for segments we send.
    pub mss: u16,
    /// Consecutive retransmission timeouts tolerated before `TimedOut`.
    /// Zero-window probes do not count.
    pub max_retries: u8,
    pub rto_initial_ms: u64,
    pub rto_min_ms: u64,
    pub rto_max_ms: u64,
    /// TIME-WAIT duration (2 * MSL).
    pub time_wait_ms: u64,
}

impl Default for Config {
    fn default() -> Self {
        // RFC 6298: initial RTO 1 s, minimum 1 s; RFC 9293: MSL 2 minutes is
        // traditional, 30 s here keeps TIME-WAIT reasonable for a client.
        Self {
            mss: 1460,
            max_retries: 8,
            rto_initial_ms: 1000,
            rto_min_ms: 1000,
            rto_max_ms: 60_000,
            time_wait_ms: 60_000,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Endpoint {
    pub addr: Ipv4Addr,
    pub port: u16,
}

pub struct Connection<'a> {
    config: Config,
    local: Endpoint,
    remote: Endpoint,
    state: State,
    error: Option<TcpError>,
    tx: Ring<'a>,
    rx: Ring<'a>,
    iss: u32,
    snd_una: u32,
    snd_nxt: u32,
    /// Highest sequence number sent so far; `snd_nxt` falls back to
    /// `snd_una` after a timeout while `snd_max` keeps the true edge.
    snd_max: u32,
    snd_wnd: u32,
    snd_wl1: u32,
    snd_wl2: u32,
    snd_mss: u32,
    fin_queued: bool,
    fin_seq: Option<u32>,
    peer_fin: bool,
    rcv_nxt: u32,
    rcv_adv: u32,
    cwnd: u32,
    ssthresh: u32,
    rto: u64,
    srtt: Option<u64>,
    rttvar: u64,
    /// (sequence that must be acknowledged, send time) of the timed segment.
    rtt_sample: Option<(u32, u64)>,
    retx_deadline: Option<u64>,
    retries: u8,
    persist_deadline: Option<u64>,
    persist_interval: u64,
    probe_pending: bool,
    time_wait_deadline: Option<u64>,
    syn_pending: bool,
    ack_pending: bool,
    rst_pending: Option<u32>,
}

impl<'a> Connection<'a> {
    /// Starts an active open. The SYN is produced by the first
    /// `poll_transmit`. `iss` must come from the caller's CSPRNG-based
    /// generator (RFC 6528); this crate does not choose it.
    pub fn connect(
        config: Config,
        local: Endpoint,
        remote: Endpoint,
        iss: u32,
        tx_buf: &'a mut [u8],
        rx_buf: &'a mut [u8],
    ) -> Result<Self, TcpError> {
        let valid = config.mss >= 64
            && config.max_retries > 0
            && config.rto_min_ms > 0
            && config.rto_min_ms <= config.rto_initial_ms
            && config.rto_initial_ms <= config.rto_max_ms
            && config.time_wait_ms > 0
            && !tx_buf.is_empty()
            && !rx_buf.is_empty()
            && local.port != 0
            && remote.port != 0
            && !remote.addr.is_group()
            && remote.addr != Ipv4Addr::UNSPECIFIED;
        if !valid {
            return Err(TcpError::InvalidConfig);
        }
        Ok(Self {
            config,
            local,
            remote,
            state: State::SynSent,
            error: None,
            tx: Ring::new(tx_buf),
            rx: Ring::new(rx_buf),
            iss,
            snd_una: iss,
            snd_nxt: iss,
            snd_max: iss,
            snd_wnd: 0,
            snd_wl1: 0,
            snd_wl2: 0,
            snd_mss: DEFAULT_PEER_MSS.min(config.mss) as u32,
            fin_queued: false,
            fin_seq: None,
            peer_fin: false,
            rcv_nxt: 0,
            rcv_adv: 0,
            cwnd: 0,
            ssthresh: u32::MAX / 2,
            rto: config.rto_initial_ms,
            srtt: None,
            rttvar: 0,
            rtt_sample: None,
            retx_deadline: None,
            retries: 0,
            persist_deadline: None,
            persist_interval: 0,
            probe_pending: false,
            time_wait_deadline: None,
            syn_pending: true,
            ack_pending: false,
            rst_pending: None,
        })
    }

    pub fn state(&self) -> State {
        self.state
    }

    /// Why the connection closed, if it closed abnormally.
    pub fn error(&self) -> Option<TcpError> {
        self.error
    }

    pub fn local(&self) -> Endpoint {
        self.local
    }

    pub fn remote(&self) -> Endpoint {
        self.remote
    }

    /// Bytes received and not yet read by `recv`.
    pub fn recv_buffered(&self) -> usize {
        self.rx.len()
    }

    /// Bytes accepted by `send` and not yet acknowledged.
    pub fn send_buffered(&self) -> usize {
        self.tx.len()
    }

    /// The peer sent FIN and every byte before it has been read.
    pub fn peer_closed(&self) -> bool {
        self.peer_fin && self.rx.len() == 0
    }

    /// Diagnostics for tests and telemetry.
    pub fn rto_ms(&self) -> u64 {
        self.rto
    }

    pub fn congestion_window(&self) -> u32 {
        self.cwnd
    }

    pub fn bytes_in_flight(&self) -> u32 {
        self.snd_nxt.wrapping_sub(self.snd_una)
    }

    /// Earliest time at which `poll_transmit` has timer work to do.
    pub fn next_deadline(&self) -> Option<u64> {
        [
            self.retx_deadline,
            self.persist_deadline,
            self.time_wait_deadline,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// Queues data; returns how many bytes fit into the send buffer.
    pub fn send(&mut self, data: &[u8]) -> Result<usize, TcpError> {
        match self.state {
            State::SynSent | State::Established | State::CloseWait if !self.fin_queued => {
                Ok(self.tx.push(data))
            }
            _ => Err(self.error.unwrap_or(TcpError::SendClosed)),
        }
    }

    /// Reads received data. A window update is scheduled when reading
    /// opens the window by at least min(MSS, buffer/2) since the last
    /// advertisement (receiver-side SWS avoidance, RFC 9293 3.8.6.2.2).
    pub fn recv(&mut self, out: &mut [u8]) -> usize {
        let count = self.rx.pop(out);
        if count > 0 && self.synchronized() {
            let threshold = (self.config.mss as usize)
                .min(self.rx.capacity() / 2)
                .max(1) as u32;
            if self.rcv_window().saturating_sub(self.rcv_adv) >= threshold {
                self.ack_pending = true;
            }
        }
        count
    }

    /// Graceful close: FIN follows all queued data.
    pub fn close(&mut self) {
        match self.state {
            State::SynSent => self.close_with(None),
            State::Established => {
                self.fin_queued = true;
                self.state = State::FinWait1;
            }
            State::CloseWait => {
                self.fin_queued = true;
                self.state = State::LastAck;
            }
            _ => {}
        }
    }

    /// Abortive close: sends RST in synchronized states and discards data.
    pub fn abort(&mut self) {
        if self.synchronized() {
            self.rst_pending = Some(self.snd_nxt);
        }
        self.close_with(None);
    }

    fn synchronized(&self) -> bool {
        !matches!(self.state, State::Closed | State::SynSent)
    }

    fn close_with(&mut self, error: Option<TcpError>) {
        self.state = State::Closed;
        self.error = error;
        self.retx_deadline = None;
        self.persist_deadline = None;
        self.time_wait_deadline = None;
        self.syn_pending = false;
        self.ack_pending = false;
        self.probe_pending = false;
    }

    fn rcv_window(&self) -> u32 {
        self.rx.free().min(MAX_WINDOW) as u32
    }

    /// Sequence number of the first byte in the send buffer.
    fn data_start(&self) -> u32 {
        if self.snd_una == self.iss {
            self.iss.wrapping_add(1)
        } else {
            self.snd_una
        }
    }

    fn fin_acked(&self) -> bool {
        self.fin_seq.is_some_and(|fin| seq::gt(self.snd_una, fin))
    }

    fn enter_time_wait(&mut self, now: u64) {
        self.state = State::TimeWait;
        self.retx_deadline = None;
        self.persist_deadline = None;
        self.time_wait_deadline = Some(now.saturating_add(self.config.time_wait_ms));
    }

    /// RFC 6298 section 2 with integer milliseconds.
    fn rtt_update(&mut self, sample: u64) {
        match self.srtt {
            None => {
                self.srtt = Some(sample);
                self.rttvar = sample / 2;
            }
            Some(srtt) => {
                self.rttvar = (3 * self.rttvar + srtt.abs_diff(sample)) / 4;
                self.srtt = Some((7 * srtt + sample) / 8);
            }
        }
        let srtt = self.srtt.unwrap_or(sample);
        self.rto = (srtt + CLOCK_GRANULARITY_MS.max(4 * self.rttvar))
            .clamp(self.config.rto_min_ms, self.config.rto_max_ms);
    }

    /// RFC 5681 slow start and congestion avoidance.
    fn on_new_ack(&mut self, acked: u32) {
        if acked == 0 {
            return;
        }
        let increase = if self.cwnd < self.ssthresh {
            acked.min(self.snd_mss)
        } else {
            (self.snd_mss * self.snd_mss / self.cwnd).max(1)
        };
        self.cwnd = self.cwnd.saturating_add(increase).min(MAX_CWND);
    }

    /// Feeds a parsed segment addressed to this connection's IPv4 endpoints.
    /// Port mismatches are ignored; address demultiplexing is the caller's.
    pub fn on_segment(&mut self, segment: &TcpSegment<'_>, now: u64) {
        let header = &segment.header;
        if header.src_port != self.remote.port || header.dst_port != self.local.port {
            return;
        }
        match self.state {
            State::Closed => return,
            State::SynSent => self.on_syn_sent(segment, now),
            _ => self.on_synchronized(segment, now),
        }
        self.update_persist(now);
    }

    fn on_syn_sent(&mut self, segment: &TcpSegment<'_>, now: u64) {
        let header = &segment.header;
        let has_ack = segment.has(ACK);
        let ack_ok = has_ack && seq::gt(header.ack, self.iss) && seq::le(header.ack, self.snd_max);
        if has_ack && !ack_ok {
            if !segment.has(RST) {
                self.rst_pending = Some(header.ack);
            }
            return;
        }
        if segment.has(RST) {
            if ack_ok {
                self.close_with(Some(TcpError::Refused));
            }
            return;
        }
        // Simultaneous open (SYN without ACK) is outside the profile.
        if !segment.has(SYN) || !ack_ok {
            return;
        }
        self.rcv_nxt = header.seq.wrapping_add(1);
        self.snd_una = header.ack;
        if seq::lt(self.snd_nxt, self.snd_una) {
            self.snd_nxt = self.snd_una;
        }
        self.snd_mss = self.config.mss.min(header.mss.unwrap_or(DEFAULT_PEER_MSS)) as u32;
        // RFC 5681 section 3.1 initial window.
        self.cwnd = (4 * self.snd_mss).min((2 * self.snd_mss).max(4380));
        self.snd_wnd = header.window as u32;
        self.snd_wl1 = header.seq;
        self.snd_wl2 = header.ack;
        if let Some((end, sent)) = self.rtt_sample {
            if seq::ge(header.ack, end) {
                self.rtt_update(now.saturating_sub(sent));
                self.rtt_sample = None;
            }
        }
        self.retries = 0;
        self.retx_deadline = None;
        self.state = State::Established;
        self.ack_pending = true;
        let data_seq = header.seq.wrapping_add(1);
        self.receive_data(data_seq, segment.payload, segment.has(FIN), now);
    }

    fn on_synchronized(&mut self, segment: &TcpSegment<'_>, now: u64) {
        let header = &segment.header;
        let len = segment.seq_len();
        let window = self.rcv_window();
        // RFC 9293 section 3.10.7.4, first check. With a zero window a
        // segment at RCV.NXT stays acceptable so its ACK, RST and window
        // fields are processed; its data is not buffered.
        let acceptable = if len == 0 || window == 0 {
            if window == 0 {
                header.seq == self.rcv_nxt
            } else {
                seq::in_window(header.seq, self.rcv_nxt, window)
            }
        } else {
            seq::in_window(header.seq, self.rcv_nxt, window)
                || seq::in_window(header.seq.wrapping_add(len - 1), self.rcv_nxt, window)
        };
        if !acceptable {
            if !segment.has(RST) {
                self.ack_pending = true;
                if self.state == State::TimeWait && segment.has(FIN) {
                    self.time_wait_deadline = Some(now.saturating_add(self.config.time_wait_ms));
                }
            }
            return;
        }
        // RFC 5961 section 3.2: only an exact RST resets; others in the
        // window get a challenge ACK.
        if segment.has(RST) {
            if header.seq == self.rcv_nxt {
                self.close_with(Some(TcpError::Reset));
            } else {
                self.ack_pending = true;
            }
            return;
        }
        // RFC 5961 section 4.2: SYN in a synchronized state.
        if segment.has(SYN) {
            self.ack_pending = true;
            return;
        }
        if !segment.has(ACK) {
            return;
        }
        if !self.process_ack(segment, now) || self.state == State::Closed {
            return;
        }
        match self.state {
            State::Established | State::FinWait1 | State::FinWait2 => {
                self.receive_data(header.seq, segment.payload, segment.has(FIN), now)
            }
            _ => {
                if !segment.payload.is_empty() || segment.has(FIN) {
                    self.ack_pending = true;
                }
            }
        }
    }

    /// Returns false when the segment must be dropped (ACK of unsent data).
    fn process_ack(&mut self, segment: &TcpSegment<'_>, now: u64) -> bool {
        let header = &segment.header;
        let ack = header.ack;
        if seq::gt(ack, self.snd_max) {
            self.ack_pending = true;
            return false;
        }
        if seq::lt(ack, self.snd_una) {
            return true; // old duplicate: ignore ACK and window fields
        }
        if seq::gt(ack, self.snd_una) {
            let data_acked = (ack.wrapping_sub(self.data_start()) as usize).min(self.tx.len());
            self.tx.consume(data_acked);
            self.snd_una = ack;
            if seq::lt(self.snd_nxt, self.snd_una) {
                self.snd_nxt = self.snd_una;
            }
            if let Some((end, sent)) = self.rtt_sample {
                if seq::ge(ack, end) {
                    self.rtt_update(now.saturating_sub(sent));
                    self.rtt_sample = None;
                }
            }
            self.retries = 0;
            self.on_new_ack(data_acked as u32);
            self.retx_deadline = if self.snd_una == self.snd_max {
                None
            } else {
                Some(now.saturating_add(self.rto))
            };
            if self.fin_acked() {
                match self.state {
                    State::FinWait1 => self.state = State::FinWait2,
                    State::Closing => self.enter_time_wait(now),
                    State::LastAck => {
                        self.close_with(None);
                        return true;
                    }
                    _ => {}
                }
            }
        }
        if seq::lt(self.snd_wl1, header.seq)
            || (self.snd_wl1 == header.seq && seq::le(self.snd_wl2, ack))
        {
            self.snd_wnd = header.window as u32;
            self.snd_wl1 = header.seq;
            self.snd_wl2 = ack;
        }
        true
    }

    /// In-order data only: segments starting beyond RCV.NXT are dropped and
    /// answered with a duplicate ACK (no reassembly queue in this profile).
    fn receive_data(&mut self, seg_seq: u32, payload: &[u8], fin: bool, now: u64) {
        let mut start = seg_seq;
        let mut data = payload;
        if seq::lt(start, self.rcv_nxt) {
            let skip = self.rcv_nxt.wrapping_sub(start) as usize;
            if skip > data.len() {
                self.ack_pending = true;
                return;
            }
            data = &data[skip..];
            start = self.rcv_nxt;
        }
        if start != self.rcv_nxt {
            self.ack_pending = true;
            return;
        }
        let taken = self.rx.push(data);
        self.rcv_nxt = self.rcv_nxt.wrapping_add(taken as u32);
        if !data.is_empty() || fin {
            self.ack_pending = true;
        }
        if fin && taken == data.len() {
            self.rcv_nxt = self.rcv_nxt.wrapping_add(1);
            self.peer_fin = true;
            match self.state {
                State::Established => self.state = State::CloseWait,
                State::FinWait1 if self.fin_acked() => self.enter_time_wait(now),
                State::FinWait1 => self.state = State::Closing,
                State::FinWait2 => self.enter_time_wait(now),
                _ => {}
            }
        }
    }

    /// Zero-window persist (RFC 9293 3.8.6.1): while the peer advertises a
    /// zero window and data waits, the retransmission timer is replaced by a
    /// probe timer with exponential backoff that never times out.
    fn update_persist(&mut self, now: u64) {
        if !self.synchronized() || self.state == State::TimeWait {
            return;
        }
        if self.snd_wnd == 0 && self.tx.len() > 0 {
            self.retx_deadline = None;
            if self.persist_deadline.is_none() {
                self.persist_interval = self.rto;
                self.persist_deadline = Some(now.saturating_add(self.rto));
            }
        } else if self.persist_deadline.is_some() {
            self.persist_deadline = None;
            self.probe_pending = false;
            if self.snd_max != self.snd_una && self.retx_deadline.is_none() {
                self.retx_deadline = Some(now.saturating_add(self.rto));
            }
        }
    }

    fn run_timers(&mut self, now: u64) {
        if self.time_wait_deadline.is_some_and(|at| now >= at) {
            self.close_with(None);
            return;
        }
        if self.persist_deadline.is_some_and(|at| now >= at) {
            self.probe_pending = true;
            self.persist_interval = (self.persist_interval * 2).min(self.config.rto_max_ms);
            self.persist_deadline = Some(now.saturating_add(self.persist_interval));
        }
        if self.retx_deadline.is_some_and(|at| now >= at) {
            self.retries += 1;
            if self.retries > self.config.max_retries {
                self.close_with(Some(TcpError::TimedOut));
                return;
            }
            // RFC 6298 5.5-5.7 and Karn's algorithm.
            self.rto = (self.rto * 2).min(self.config.rto_max_ms);
            self.rtt_sample = None;
            if self.state == State::SynSent {
                self.syn_pending = true;
            } else {
                // RFC 5681 (4): halve the flight into ssthresh, one-segment
                // loss window, go back to SND.UNA.
                let flight = self.snd_max.wrapping_sub(self.snd_una);
                self.ssthresh = (flight / 2).max(2 * self.snd_mss);
                self.cwnd = self.snd_mss;
                self.snd_nxt = self.snd_una;
            }
            self.retx_deadline = Some(now.saturating_add(self.rto));
        }
    }

    /// Produces at most one segment (the IPv4 payload) into `out`. Call
    /// repeatedly until it returns `Ok(None)`. `BufferTooSmall` leaves the
    /// connection unchanged; `out` must hold at least 24 bytes.
    pub fn poll_transmit(
        &mut self,
        now: u64,
        out: &mut [u8],
    ) -> Result<Option<usize>, SegmentError> {
        if out.len() < TCP_HEADER_LEN + 4 {
            return Err(SegmentError::BufferTooSmall);
        }
        self.run_timers(now);
        let (src, dst) = (self.local.addr, self.remote.addr);
        if let Some(rst_seq) = self.rst_pending {
            let header = TcpHeader {
                src_port: self.local.port,
                dst_port: self.remote.port,
                seq: rst_seq,
                flags: RST,
                ..TcpHeader::default()
            };
            let len = emit_tcp(out, src, dst, &header, &[])?;
            self.rst_pending = None;
            return Ok(Some(len));
        }
        match self.state {
            State::Closed => return Ok(None),
            State::SynSent => return self.transmit_syn(now, out),
            _ => {}
        }
        self.update_persist(now);

        let data_start = self.data_start();
        let room = out.len() - TCP_HEADER_LEN;
        let probe = self.probe_pending && self.tx.len() > 0;
        let (seg_seq, count) = if probe {
            (self.snd_una, 1)
        } else {
            let sent = self.snd_nxt.wrapping_sub(data_start) as usize;
            let unsent = self.tx.len().saturating_sub(sent);
            let flight = self.snd_nxt.wrapping_sub(self.snd_una);
            let usable = self.cwnd.min(self.snd_wnd).saturating_sub(flight) as usize;
            let count = unsent.min(self.snd_mss as usize).min(usable).min(room);
            (self.snd_nxt, count)
        };
        let data_end = data_start.wrapping_add(self.tx.len() as u32);
        let send_fin = !probe
            && self.fin_queued
            && !self.fin_acked()
            && seg_seq.wrapping_add(count as u32) == data_end;
        if count == 0 && !send_fin && !self.ack_pending {
            return Ok(None);
        }

        let window = self.rcv_window();
        let mut flags = ACK;
        if count > 0 {
            flags |= PSH;
        }
        if send_fin {
            flags |= FIN;
        }
        let header = TcpHeader {
            src_port: self.local.port,
            dst_port: self.remote.port,
            seq: seg_seq,
            ack: self.rcv_nxt,
            flags,
            window: window as u16,
            mss: None,
        };
        let offset = seg_seq.wrapping_sub(data_start) as usize;
        self.tx
            .peek(offset, &mut out[TCP_HEADER_LEN..TCP_HEADER_LEN + count]);
        let len = emit_in_place(out, src, dst, &header, count)?;

        let end = seg_seq.wrapping_add(count as u32 + send_fin as u32);
        if probe {
            self.probe_pending = false;
            if seq::gt(end, self.snd_nxt) {
                self.snd_nxt = end;
            }
        } else {
            self.snd_nxt = end;
        }
        if seq::gt(end, self.snd_max) {
            // Karn: only a segment of entirely new data is timed.
            if self.rtt_sample.is_none() && seg_seq == self.snd_max && !probe {
                self.rtt_sample = Some((end, now));
            }
            self.snd_max = end;
        }
        if send_fin && self.fin_seq.is_none() {
            self.fin_seq = Some(data_end);
        }
        if (count > 0 || send_fin)
            && !probe
            && self.retx_deadline.is_none()
            && self.persist_deadline.is_none()
        {
            self.retx_deadline = Some(now.saturating_add(self.rto));
        }
        self.ack_pending = false;
        self.rcv_adv = window;
        Ok(Some(len))
    }

    fn transmit_syn(&mut self, now: u64, out: &mut [u8]) -> Result<Option<usize>, SegmentError> {
        if !self.syn_pending {
            return Ok(None);
        }
        let header = TcpHeader {
            src_port: self.local.port,
            dst_port: self.remote.port,
            seq: self.iss,
            ack: 0,
            flags: SYN,
            window: self.rcv_window() as u16,
            mss: Some(self.config.mss),
        };
        let len = emit_tcp(out, self.local.addr, self.remote.addr, &header, &[])?;
        self.syn_pending = false;
        self.snd_nxt = self.iss.wrapping_add(1);
        self.snd_max = self.snd_nxt;
        if self.retries == 0 && self.rtt_sample.is_none() {
            self.rtt_sample = Some((self.snd_nxt, now));
        }
        if self.retx_deadline.is_none() {
            self.retx_deadline = Some(now.saturating_add(self.rto));
        }
        self.rcv_adv = self.rcv_window();
        Ok(Some(len))
    }
}
