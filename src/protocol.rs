use crate::util;
use log::debug;
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
enum State {
    SynRcvd,
    Estab,
    FinWait1,
    FinWait2,
    TimeWait,
    CloseWait,
}

enum TcpControl {
    None,
    RST,
    FIN,
    ACK,
    SYN,
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

impl TCB {
    /// listen on a port, return the
    pub fn new(
        nic: &mut tun_tap::Iface,
        ip_header: etherparse::Ipv4HeaderSlice,
        tcp_header: etherparse::TcpHeaderSlice,
    ) -> io::Result<Option<Self>> {
        if !tcp_header.syn() {
            return Ok(None);
        }

        let iss = 0;
        let mut tcb = Self {
            state: State::SynRcvd,
            send: SendSequenceSpace::new(iss, tcp_header.window_size()),
            recv: RecvSequenceSpace::new(
                tcp_header.sequence_number(),
                tcp_header.sequence_number() + 1,
                1024,
            ),
            ip_header: etherparse::Ipv4Header::new(
                0,
                64,
                etherparse::IpTrafficClass::Tcp,
                [
                    ip_header.destination()[0],
                    ip_header.destination()[1],
                    ip_header.destination()[2],
                    ip_header.destination()[3],
                ],
                [
                    ip_header.source()[0],
                    ip_header.source()[1],
                    ip_header.source()[2],
                    ip_header.source()[3],
                ],
            ),
            tcp_header: etherparse::TcpHeader::new(
                tcp_header.destination_port(),
                tcp_header.source_port(),
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
        };
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

        if util::wrapping_lt(self.send.nxt, next_seq) {
            self.send.nxt = next_seq;
        }
        self.timers.send_times.insert(seq, time::Instant::now());

        nic.send(&buf[..payload_ends_at]).unwrap();
        Ok(payload_bytes)
    }

    /// Operations on a received packet. See RFC 793 page 65 Segment Arrives
    pub fn unpack(
        &mut self,
        nic: &mut tun_tap::Iface,
        tcp_header: etherparse::TcpHeaderSlice,
        data: &[u8],
    ) -> io::Result<()> {
        // first, check if the sequence numbers are valid.
        // see RFC 793 page 24, it is complicated.

        debug!("TCB::unpack: data content length: {}", &data.len());

        let mut data_len = data.len() as u32; //check if segment is zero.
        let seqn = tcp_header.sequence_number();

        // then, figure out what is this packet used for. SYN, FIN or normal.
        if tcp_header.syn() {
            // After that, the connection is established.
            // we need to send ACK later for the third handshake.
            // After that, the connection is established.
            data_len += 1;
        }
        if tcp_header.fin() {
            data_len += 1;
        }

        // RFC 793 page 25, comparisons for sequence number
        let wend = self.recv.nxt.wrapping_add(self.recv.wnd as u32);
        let acceptable = if data_len == 0 {
            // segment is zero, see the comparisons in RFC 793 page 24
            if self.recv.wnd == 0 {
                if seqn != self.recv.nxt {
                    false
                } else {
                    true
                }
            } else if !util::is_between_wrapped(self.recv.nxt.wrapping_sub(1), seqn, wend) {
                false
            } else {
                true
            }
        } else {
            // normal segment which length is not zero
            if self.recv.wnd == 0 {
                false
            } else if !util::is_between_wrapped(self.recv.nxt.wrapping_sub(1), seqn, wend) {
                false
            } else {
                true
            }
        };

        if !acceptable {
            debug!("TCB::unpack: Packet not acceptable");
            self.write(nic, self.send.nxt, 0).expect("TCB write failed");
            return Ok(());
        }

        if !tcp_header.ack() {
            debug!("TCB::unpack: Packet does not have a ACK bit");
            return Ok(());
        }
        let ackn = tcp_header.acknowledgment_number();
        if let State::SynRcvd = self.state {
            // RFC 793 page 69, SEGMENT ARRIVES for Syn-Rcvd state
            // first check sequence number
            if util::is_between_wrapped(
                self.send.una.wrapping_sub(1),
                ackn,
                self.send.nxt.wrapping_add(1),
            ) {
                self.state = State::Estab;
            } else {
                // second check RST
                // third check security and precedence
                // TODO
            }
        }

        if let State::Estab | State::FinWait1 | State::FinWait2 = self.state {
            // RFC 793 page 69, "SEGMENT ARRIVES" handling for states above
            // first check sequence number
            // Wont' fix: second check RST
            // Wont' fix: third check security and precedence
            // Not here: fourth, check the SYN bit
            // fifth check the ACK field
            // sixth, check the URG bit
            // seventh, process the segment text
            // eighth, check the FIN bit

            debug!(
                "self.una {},ackn{}, self.nxt {}",
                self.send.una, ackn, self.send.nxt
            );

            if util::is_between_wrapped(self.send.una, ackn, self.send.nxt.wrapping_add(1)) {
                if !self.unacked.is_empty() {
                    let data_start = if self.send.una == self.send.iss {
                        self.send.una.wrapping_add(1)
                    } else {
                        self.send.una
                    };
                    let acked_data_end =
                        std::cmp::min(ackn.wrapping_sub(data_start) as usize, self.unacked.len());
                    self.unacked.drain(..acked_data_end);
                }
                self.send.una = ackn;
            }
        }

        if let State::FinWait1 = self.state {
            if let Some(closed_at) = self.closed_at {
                if self.send.una == closed_at.wrapping_add(1) {
                    self.state = State::FinWait2;
                }
            }
        }

        if !data.is_empty() {
            if let State::Estab | State::FinWait1 | State::FinWait2 = self.state {
                let mut unread_data_at = self.recv.nxt.wrapping_sub(seqn) as usize;
                debug!(
                    "TCB::unpack: seqn: {}, recv.nxt: {}, unread: {}",
                    seqn, self.recv.nxt, unread_data_at
                );
                if unread_data_at > data.len() {
                    unread_data_at = 0;
                }
                debug!(
                    "TCB::unpack: unread_data_at: {:02x?}",
                    &data[unread_data_at..]
                );
                self.incoming.extend(&data[unread_data_at..]);
                self.recv.nxt = seqn.wrapping_add(data_len);
                self.write(nic, self.send.nxt, 0)?;
            }
        }

        if tcp_header.fin() {
            match self.state {
                State::FinWait2 => {
                    debug!("TCB::unpack: connection state turn into TimeWait");
                    self.write(nic, self.send.nxt, 0);
                    self.state = State::TimeWait;
                }
                State::Estab => {
                    // fist, sending ack for this FIN.
                    self.write(nic, self.send.nxt, 0);

                    //TODO: second: send own pending data and CLOSE-WAIT
                    //TODO: third: send FIN(call close()) and LAST-ACK
                    unimplemented!();
                }
                _ => unimplemented!(),
            }
        }

        Ok(())
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
}
pub enum Available {
    READ,
    WRITE,
}
