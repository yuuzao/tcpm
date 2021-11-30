use crate::util;
use log::{debug, info};
use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::time;

pub struct TCB {
    state: State,
    send: SendSequenceSpace,
    recv: RecvSequenceSpace,
    ip_header: etherparse::Ipv4Header,
    tcp_header: etherparse::TcpHeader,
    closed: bool,
    closed_at: Option<u32>,
    timers: Timers,

    // stores received buffer which is waiting for reading
    pub(crate) incoming: VecDeque<u8>,

    // stores the buffer to be sent, including unacked buffer
    pub(crate) unacked: VecDeque<u8>,
}

pub struct Timers {
    send_times: BTreeMap<u32, time::Instant>,
    srtt: f64,
}
#[derive(Debug)]
enum State {
    Listen,
    Closing,
    Closed,
    LastAck,
    SynSent,
    SynRcvd,
    Estab,
    FinWait1,
    FinWait2,
    TimeWait,
    CloseWait,
}

struct SendSequenceSpace {
    ///Send Sequence Variables, see RFC793 page 19 and page 25

    /// unacknowledged
    una: u32,
    /// next sequence number for sending
    nxt: u32,
    /// send window
    wnd: u16,
    /// initial sequence number for sending
    iss: u32,
}
impl SendSequenceSpace {
    fn new(iss: u32, wnd: u16) -> Self {
        Self {
            iss,
            una: iss,
            nxt: iss,
            wnd,
        }
    }
}

struct RecvSequenceSpace {
    /// Recv Sequence Variables, see RFC793 page 19 and page 25

    /// receive next, which equals to received sequence number + 1
    nxt: u32,
    /// receive window
    wnd: u16,
    /// initial received sequence number
    irs: u32,
}
impl RecvSequenceSpace {
    fn new(irs: u32, nxt: u32, wnd: u16) -> Self {
        Self { nxt, wnd, irs }
    }
}

pub enum Available {
    READ,
    WRITE,
}
pub enum Action {
    Close,
    Continue,
    Read,
}
impl TCB {
    fn new(ip_header: etherparse::Ipv4Header, tcp_header: etherparse::TcpHeader) -> Self {
        let iss = 0;
        Self {
            state: State::SynRcvd,
            send: SendSequenceSpace::new(iss, tcp_header.window_size),
            recv: RecvSequenceSpace::new(
                tcp_header.sequence_number,
                tcp_header.sequence_number + 1,
                1024,
            ),
            ip_header: etherparse::Ipv4Header::new(
                0,
                64,
                etherparse::IpTrafficClass::Tcp,
                ip_header.destination,
                ip_header.source,
            ),
            tcp_header: etherparse::TcpHeader::new(
                tcp_header.destination_port,
                tcp_header.source_port,
                iss,
                1024,
            ),
            incoming: Default::default(),
            unacked: Default::default(),
            closed: false,
            closed_at: None,
            timers: Timers {
                send_times: Default::default(),
                srtt: time::Duration::from_secs(1 * 60).as_secs_f64(),
            },
        }
    }
    pub fn new_connection(
        nic: &mut tun_tap::Iface,
        ip_header: etherparse::Ipv4HeaderSlice,
        tcp_header: etherparse::TcpHeaderSlice,
    ) -> io::Result<Option<Self>> {
        if !tcp_header.syn() {
            return Ok(None);
        }
        let mut tcb = TCB::new(ip_header.to_header(), tcp_header.to_header());

        tcb.tcp_header.syn = true;
        tcb.tcp_header.ack = true;

        tcb.write(nic, tcb.send.nxt, 0).unwrap();

        Ok(Some(tcb))
    }

    /// This function does three things:
    /// calculates the proper length of sending buffer from the unacked queue and adjusts TCB
    /// sends the buffer with tcp header to the nic
    /// return the length of sent buffer.
    fn write(&mut self, nic: &mut tun_tap::Iface, seq: u32, mut limit: usize) -> io::Result<usize> {
        let mut buf = [0u8; 1500];
        self.tcp_header.sequence_number = seq;
        self.tcp_header.acknowledgment_number = self.recv.nxt;

        let mut offset = seq.wrapping_sub(self.send.una) as usize;

        if let Some(closed_at) = self.closed_at {
            if seq == closed_at.wrapping_add(1) {
                offset = 0;
                limit = 0;
            }
        }
        let (mut head, mut tail) = self.unacked.as_slices();
        if head.len() >= offset {
            head = &head[offset..];
        } else {
            let skipped = head.len();
            head = &[];
            tail = &tail[(offset - skipped)..];
        }

        let max_data = std::cmp::min(limit, tail.len() + head.len());
        // the ip packet to be sent should not exceeds the MTU.
        let data_size = std::cmp::min(
            buf.len(),
            self.tcp_header.header_len() as usize + self.ip_header.header_len() + max_data,
        );

        self.ip_header
            .set_payload_len(data_size - self.ip_header.header_len())
            .unwrap();

        // write the header and payload into buf.
        // buf is an array which doesn't have write trait.
        // we can use a slice for it.
        // note: this slice references to the unwritten part for buf.
        use std::io::Write;
        let buf_len = buf.len();
        let mut writer = &mut buf[..];

        self.ip_header.write(&mut writer).unwrap();
        let ip_header_ends_at = buf_len - writer.len();

        writer = &mut writer[self.tcp_header.header_len() as usize..];
        let tcp_header_ends_at = buf_len - writer.len();

        let payload_bytes = {
            let mut written = 0;
            let mut limit = max_data;

            let p1l = std::cmp::min(limit, head.len());
            written += writer.write(&head[..p1l])?;
            limit -= written;

            let p2l = std::cmp::min(limit, tail.len());
            written += writer.write(&tail[..p2l])?;

            written
        };
        let payload_ends_at = buf_len - writer.len();

        self.tcp_header.checksum = self
            .tcp_header
            .calc_checksum_ipv4(&self.ip_header, &buf[tcp_header_ends_at..payload_ends_at])
            .unwrap();

        let mut tcp_header_buf = &mut buf[ip_header_ends_at..tcp_header_ends_at];
        self.tcp_header.write(&mut tcp_header_buf)?;

        // update connection's send sequence space
        let next_seq = seq.wrapping_add(payload_bytes as u32);
        // SYN in tcp_header only occurs once in first or second handshake.
        // FIN also only occurs once.
        // they have to be removed after relative handshaking.
        if self.tcp_header.syn {
            // The SYN in header means this packet to be sent is for handshaking.
            // So that the payload is empty, we need add 1 to send.nxt
            self.send.nxt = self.send.nxt.wrapping_add(1);
            self.tcp_header.syn = false;
        }
        if self.tcp_header.fin {
            // If we initiate the close and set the FIN, we have to send a
            // ACK later with empty data, so we need to add 1 to send.nxt, and
            // remove the FIN.
            // If we passively close the connection, when the FIN is set, we
            // don't have to send any other packet later, we don't care about
            // what the snd.nxt is. For convenient, we add 1 to it in the
            // both scenarios.
            self.send.nxt = next_seq.wrapping_add(1);
            self.tcp_header.fin = false;
        }

        if util::lt(self.send.nxt, next_seq) {
            self.send.nxt = next_seq;
        }
        self.timers.send_times.insert(seq, time::Instant::now());

        nic.send(&buf[..payload_ends_at]).unwrap();
        Ok(payload_bytes)
    }

    pub fn close(&mut self) -> io::Result<()> {
        self.closed = true;
        match self.state {
            State::SynRcvd | State::Estab => {
                self.state = State::FinWait1;
            }
            State::FinWait1 | State::FinWait2 => {}
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    "Already closing",
                ))
            }
        };
        Ok(())
    }
    pub fn availability(&self) -> Available {
        Available::READ
    }

    pub fn is_recv_closed(&self) -> bool {
        if let State::TimeWait = self.state {
            true
        } else {
            false
        }
    }

    pub fn on_tick(&mut self, nic: &mut tun_tap::Iface) -> io::Result<()> {
        if let State::FinWait2 | State::TimeWait = self.state {
            return Ok(());
        }

        let unacked_data = self
            .closed_at
            .unwrap_or(self.send.nxt)
            .wrapping_sub(self.send.una);

        let unsent_data_len = self.unacked.len() as u32 - unacked_data;

        let waited_for = self
            .timers
            .send_times
            .range(self.send.una..)
            .next()
            .map(|t| t.1.elapsed());

        let should_retransmit = if let Some(waited_for) = waited_for {
            waited_for > time::Duration::from_secs(1)
                && waited_for.as_secs_f64() > 1.5 * self.timers.srtt
        } else {
            false
        };

        if should_retransmit {
            let resend = std::cmp::min(self.unacked.len() as u32, self.send.wnd as u32);
            if resend < self.send.wnd as u32 && self.closed {
                self.tcp_header.fin = true;
                self.closed_at = Some(self.send.una.wrapping_add(self.unacked.len() as u32))
            }
        } else {
            if unsent_data_len == 0 && self.closed_at.is_some() {
                return Ok(());
            }

            let allowed = self.send.wnd as u32 - unacked_data;
            if allowed == 0 {
                return Ok(());
            }

            let send = std::cmp::min(unsent_data_len, allowed);
            if send < allowed && self.closed && self.closed_at.is_none() {
                self.tcp_header.fin = true;
                self.closed_at = Some(self.send.una.wrapping_add(self.unacked.len() as u32));
            }

            self.write(nic, self.send.nxt, send as usize)?;
        }

        Ok(())
    }
    /// RFC 793 page 36
    pub fn send_rst(&mut self, nic: &mut tun_tap::Iface) -> io::Result<()> {
        match self.state {
            State::SynRcvd => {
                self.tcp_header.rst = true;
            }
            _ => {}
        }

        self.write(nic, self.send.nxt, 0).unwrap();
        Ok(())
    }

    /// Operations on a received packet. See RFC 793 page 65 Segment Arrives
    /// NOTE: this method does not deal with the SYN for the first handshake when passive OPEN,
    /// which means the SYN occurs here is "illegal".
    pub fn on_segment(
        &mut self,
        nic: &mut tun_tap::Iface,
        tcp_header: etherparse::TcpHeaderSlice,
        data: &[u8],
    ) -> io::Result<Action> {
        debug!("on segmenting, self state: {:?}", self.state);
        match self.state {
            State::SynSent | State::Listen => {
                // we didn't implement active open yet.
                unimplemented!()
            }
            State::SynRcvd
            | State::Estab
            | State::FinWait1
            | State::FinWait2
            | State::CloseWait
            | State::LastAck
            | State::TimeWait => {
                // RFC793 page 69

                // first check sequence number
                if let false = self.check_seq(data, &tcp_header) {
                    if tcp_header.rst() {
                        return Ok(Action::Close);
                    }
                    self.write(nic, self.send.nxt, 0).unwrap();
                    return Ok(Action::Continue);
                }

                // second check the RST bit
                if tcp_header.rst() {
                    // NOTE: operation on passive open are different to active open, here is
                    // passive open
                    if let State::SynRcvd = self.state {
                        self.state = State::Listen;
                        return Ok(Action::Continue);
                    } else {
                        self.state = State::Closed;
                        return Ok(Action::Close);
                    }
                }

                // third check security and precedence, NOT DONE
                // fourth check the SYN bit
                if tcp_header.syn() {
                    self.send_rst(nic).unwrap();
                    self.state = State::Closed;
                    return Ok(Action::Close);
                };

                // fifth check the Ack bit
                // DO NOT use match pattern
                {
                    if !tcp_header.ack() {
                        return Ok(Action::Continue);
                    }
                    let ackn = tcp_header.acknowledgment_number();
                    if let State::SynRcvd = self.state {
                        if util::le(self.send.una, ackn) && util::le(ackn, self.send.nxt) {
                            self.state = State::Estab;
                            // cannot return yet, there may be a FIN
                        } else {
                            self.send_rst(nic).unwrap();
                        }
                    }
                    if let State::Estab | State::CloseWait = self.state {
                        // ackn too small, ignore
                        if util::lt(ackn, self.send.una) {
                            return Ok(Action::Continue);
                        }
                        // ackn just fits
                        if util::lt(self.send.una, ackn) && util::le(ackn, self.send.nxt) {
                            self.send.una = ackn;
                            // do not return right now
                            // TODO: update send wnd, NOT IMPLEMENTED, not necessary right now.
                        }
                        // ackn too large
                        if util::lt(self.send.nxt, ackn) {
                            self.write(nic, self.send.nxt, 0).unwrap();
                            return Ok(Action::Continue);
                        }
                    }

                    // this ACK is for our FIN, and we now turn into FinWait2
                    if let State::FinWait1 = self.state {
                        self.state = State::FinWait2
                    }
                    if let State::FinWait2 = self.state {
                        // This ACK must comes with FIN
                    }
                    if let State::LastAck = self.state {
                        self.state = State::Closed;
                        return Ok(Action::Close);
                    }
                    if let State::TimeWait = self.state {
                        // Here we received the retransmission queue of FIN, we should ACK this FIN
                        if !tcp_header.fin() {
                            return Err(io::Error::new(
                                io::ErrorKind::Other,
                                "ack without FIN but in timewait???",
                            ));
                        }
                        self.write(nic, self.send.nxt, 0).unwrap();
                        return Ok(Action::Continue);
                    }
                };

                // sixth check the URG bit, NOT DONE
                // seventh, process the segment text
                let seqn = tcp_header.sequence_number();
                match self.state {
                    State::Estab | State::FinWait1 | State::FinWait2 => {
                        if !data.is_empty() {
                            self.incoming.extend(&data[..]);
                            self.recv.nxt = seqn.wrapping_add(data.len() as u32);
                        }
                        if tcp_header.fin() {
                            self.recv.nxt = 1u32.wrapping_add(self.recv.nxt);
                        }
                        self.write(nic, self.send.nxt, 0).unwrap();
                    }
                    _ => {}
                }

                // eighth check the FIN bit
                if tcp_header.fin() {
                    match self.state {
                        State::Closed | State::Listen | State::SynSent => {
                            return Ok(Action::Continue)
                        }
                        State::SynRcvd | State::Estab => self.state = State::CloseWait,
                        State::FinWait1 => {
                            if tcp_header.ack() {
                                // NOTE: This ACK is not always for our sent FIN in reality, here is the
                                // ideal situation.
                                self.state = State::TimeWait;
                            } else {
                                self.state = State::Closing;
                            }
                        }
                        State::FinWait2 => {
                            self.state = State::TimeWait;
                        }

                        _ => {}
                    }
                }
            }

            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "Unsupported connection state",
                ))
            }
        }

        Ok(Action::Read)
    }

    fn check_seq(&mut self, data: &[u8], tcp_header: &etherparse::TcpHeaderSlice) -> bool {
        let in_wnd = util::segment_valid(
            self.recv.nxt,
            tcp_header.sequence_number(),
            self.recv.nxt.wrapping_add(self.recv.wnd as u32),
        );
        if data.len() == 0 {
            if self.recv.wnd == 0 {
                return tcp_header.sequence_number() == self.recv.nxt;
            } else {
                return in_wnd;
            }
        } else {
            if self.recv.wnd > 0 {
                return in_wnd;
            } else {
                return false;
            }
        }
    }
}
