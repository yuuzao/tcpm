use crate::util;
use log::debug;
use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::io::Write;
use std::time;
use tun_tap::Iface;

pub struct TCB {
    state: State,
    send: SendSequenceSpace,
    recv: RecvSequenceSpace,
    ip_header: etherparse::Ipv4Header,
    tcp_header: etherparse::TcpHeader,
    pub(crate) closed: bool,

    // the sequence number when FIN is sent.
    closed_at: Option<u32>,
    timers: Timers,

    // stores received buffer which is waiting for reading
    pub(crate) incoming: VecDeque<u8>,

    // stores the buffer to be sent, including unacked buffer
    pub(crate) outgoing: VecDeque<u8>,
}

pub struct Timers {
    send_times: BTreeMap<u32, time::Instant>,
    srtt: f64,
}
#[derive(Debug, PartialEq, Eq)]
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

    /// initial sequence number for sending
    iss: u32,
    /// unacknowledged
    una: u32,

    /// next sequence number for sending
    nxt: u32,
    /// send window
    wnd: u16,

    wl1: u32,
    wl2: u32,
}
impl SendSequenceSpace {
    fn new(irs: u32, wnd: u16) -> Self {
        Self {
            una: 0,
            nxt: 0,
            iss: 0,
            wnd,
            wl1: irs,
            wl2: irs,
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
    fn new(irs: u32) -> Self {
        Self {
            nxt: irs + 1,
            wnd: 1024,
            irs,
        }
    }
}

/// actions when segment arrives
#[derive(Debug)]
pub enum Action {
    // have to create a connection
    New,
    // have to close connection
    Close,
    // no more operation is needed, such as we have ACKed for a packet, received a ACK when we are
    // in SynRcvd state
    Continue,
    // we have to call stream to read for a packet. when we are in the states that have the ability
    // to receive payloads, we have to call stream to read no matter whether the payload is empty
    // or not.
    Read,
}

#[derive(Debug)]
pub enum Request {
    ReTransmit,
    RST,
    SYN,
    SYNACK,
    ACK,
    FIN,
}

impl TCB {
    fn new(ip_header: etherparse::Ipv4Header, tcp_header: etherparse::TcpHeader) -> Self {
        Self {
            state: State::SynRcvd,
            send: SendSequenceSpace::new(tcp_header.sequence_number, tcp_header.window_size),
            recv: RecvSequenceSpace::new(tcp_header.sequence_number),
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
                0,
                1024,
            ),
            incoming: VecDeque::default(),
            outgoing: Default::default(),
            closed: false,
            closed_at: None,
            timers: Timers {
                send_times: Default::default(),
                srtt: time::Duration::from_secs(1 * 60).as_secs_f64(),
            },
        }
    }
    pub fn new_connection(
        nic: &mut Iface,
        ip_header: etherparse::Ipv4HeaderSlice,
        tcp_header: etherparse::TcpHeaderSlice,
    ) -> io::Result<Option<Self>> {
        if !tcp_header.syn() {
            return Ok(None);
        }
        let mut tcb = TCB::new(ip_header.to_header(), tcp_header.to_header());

        tcb.tcp_header.syn = true;
        tcb.tcp_header.ack = true;

        // tcb.write(nic, tcb.send.nxt, 0).unwrap();
        tcb.write(nic, Request::SYNACK).unwrap();

        Ok(Some(tcb))
    }

    /// This function does three things:
    /// calculates the proper length of sending buffer from the unacked queue and adjusts TCB
    /// sends the buffer with tcp header to the nic
    /// return the length of sent buffer.
    fn write(&mut self, nic: &mut Iface, req: Request) -> io::Result<usize> {
        let mut buf = [0u8; 1500];
        let mut next_seqn = 0u32;

        self.tcp_header.sequence_number = match req {
            Request::SYN | Request::SYNACK => {
                self.tcp_header.syn = true;
                next_seqn += 1;
                self.send.nxt
            }
            Request::RST => {
                self.tcp_header.rst = true;
                self.send.nxt
            }
            Request::FIN => {
                // FIXME: Do we have to respect the zero receive window?
                assert!((self.state == State::FinWait1) | (self.state == State::LastAck));
                self.tcp_header.fin = true;
                next_seqn += 1;
                self.send.nxt
            }
            Request::ReTransmit => {
                //The sending TCP must regularly retransmit to the receiving TCP even when the window
                //is zero.
                if let State::FinWait1 | State::LastAck = self.state {
                    self.tcp_header.fin = true;
                };
                self.send.una
            }
            Request::ACK => self.send.nxt,
        };
        self.tcp_header.acknowledgment_number = self.recv.nxt;

        // length of the unacked data
        let offset = self.send.nxt.wrapping_sub(self.send.una) as usize;
        debug!(
            "send.una: {:?}, snd.nxt: {:?}",
            self.send.una, self.send.nxt
        );

        let payload: &[u8] = {
            if let Request::SYN | Request::SYNACK | Request::RST = req {
                &[]
            } else if self.outgoing.is_empty() {
                &[]
            } else {
                if let Request::ReTransmit = req {
                    &self.outgoing.as_slices().0[..offset]
                } else {
                    &self.outgoing.as_slices().0[offset..]
                }
            }
        };

        let data_size = std::cmp::min(
            buf.len(),
            self.tcp_header.header_len() as usize + self.ip_header.header_len() + payload.len(),
        );
        self.ip_header
            .set_payload_len(data_size - self.ip_header.header_len())
            .unwrap();

        // let's write the header and payload into buf.
        // buf is an array which doesn't have write trait, so we have to create a slice for it.
        // NOTE: this slice references to the unwritten part of buf.
        let buf_len = buf.len();
        let mut unwritten = &mut buf[..];

        // The write implementation of Ipv4header writes the ip header into it's parameter.
        self.ip_header.write(&mut unwritten).unwrap();
        let ip_header_ends_at = buf_len - unwritten.len();

        unwritten = &mut unwritten[self.tcp_header.header_len() as usize..];
        let tcp_header_ends_at = buf_len - unwritten.len();

        unwritten.write(payload).unwrap();
        let payload_ends_at = buf_len - unwritten.len();

        self.tcp_header.checksum = self
            .tcp_header
            .calc_checksum_ipv4(&self.ip_header, &buf[tcp_header_ends_at..payload_ends_at])
            .unwrap();

        let mut tcp_header_buf = &mut buf[ip_header_ends_at..tcp_header_ends_at];
        self.tcp_header.write(&mut tcp_header_buf).unwrap();

        nic.send(&buf[..payload_ends_at]).unwrap();

        next_seqn += payload.len() as u32;
        self.send.nxt = self.send.nxt.wrapping_add(next_seqn);
        self.tcp_header.fin = false;
        self.tcp_header.syn = false;
        self.timers
            .send_times
            .insert(self.send.una, time::Instant::now());

        Ok(payload.len())
    }

    pub fn close(&mut self) -> io::Result<()> {
        // if self.closed {
        //     return Err(io::Error::new(
        //         io::ErrorKind::BrokenPipe,
        //         "Connection already closed",
        //     ));
        // }
        self.closed = true;
        match self.state {
            State::Estab => {
                debug!("close called at ESTAB");
                self.state = State::FinWait1;
            }
            State::CloseWait => {
                debug!("should close from CLOSEWAIT");
                self.state = State::LastAck;
            }
            //FIXME: cannot do proper active close in some scenarios if there exist multiple
            //connections because of
            //reading_notifier will notify all
            _ => {
                debug!("close at improper state:{:?}", self.state);
                unreachable!()
            }
        };
        Ok(())
    }
    pub fn is_recv_closed(&self) -> bool {
        // TODO: completed, but not completely completed.
        if let State::TimeWait = self.state {
            true
        } else {
            false
        }
    }

    /// a tiemr for unacked queue
    // pub fn on_timer(&mut self, nic: &mut Iface) -> io::Result<Action> {
    //     if let State::FinWait2 | State::LastAck = self.state {
    //         return Ok(Action::Continue);
    //     }
    //
    //     Ok(Action::Continue)
    // }

    pub fn on_tick(&mut self, nic: &mut Iface) -> io::Result<Action> {
        //first, we figure out whether to retransmit
        let waited_for = self
            .timers
            .send_times
            .range(self.send.una..)
            .next()
            .map(|t| t.1.elapsed());

        let should_retransmit = waited_for.and_then(|x| {
            Some(x > time::Duration::from_secs(1) && x.as_secs_f64() > 1.5 * self.timers.srtt)
        });
        if should_retransmit.unwrap_or(false) {
            self.write(nic, Request::ReTransmit).unwrap();
            return Ok(Action::Continue);
        }

        // then, we send unsent data if there is any.
        let mut req = None;
        match self.state {
            State::FinWait2 => return Ok(Action::Continue),
            State::FinWait1 => {
                if self.closed_at.is_none() {
                    self.closed_at = Some(self.send.una);
                    req = Some(Request::FIN);
                }
            }
            State::TimeWait => {
                //FIXME: set correct MSL.
                if waited_for.expect("timer error in TimeWait") >= time::Duration::from_secs(2) {
                    debug!("timewait ends");
                    return Ok(Action::Close);
                } else {
                    return Ok(Action::Continue);
                }
            }
            _ => {}
        }

        let allowed = self.send.wnd as usize - self.outgoing.len();
        match allowed {
            // FIN wont' be sent if the allowed wnd is zero
            0 => {
                if let Some(Request::FIN) = req {
                    self.closed_at = None;
                };
                return Ok(Action::Continue);
            }
            _ => {
                let send = std::cmp::min(self.outgoing.len(), allowed);
                if send <= allowed && !self.closed && !self.outgoing.is_empty() {
                    req = Some(Request::ACK);
                }
            }
        };
        if req.is_some() {
            debug!("send for req type: {:?}", req);
            self.write(nic, req.unwrap())
                .expect("on_tick: sending failed");
        }
        Ok(Action::Continue)
    }

    /// RFC 793 page 36
    pub fn send_rst(&mut self, nic: &mut Iface) -> io::Result<()> {
        // TODO: completed, but not completely completed.
        match self.state {
            State::SynRcvd => {
                self.tcp_header.rst = true;
            }
            _ => {}
        }

        self.write(nic, Request::RST).unwrap();
        Ok(())
    }

    /// Operations on a received packet. See RFC 793 page 65 Segment Arrives
    /// NOTE: this method does not deal with the SYN for the first handshake when passive OPEN,
    /// which means the SYN occurs here is "illegal".
    pub fn on_segment(
        &mut self,
        nic: &mut Iface,
        tcp_header: etherparse::TcpHeaderSlice,
        data: &[u8],
    ) -> io::Result<Action> {
        let ackn = tcp_header.acknowledgment_number();
        let seqn = tcp_header.sequence_number();

        debug!(
            "on segmenting, self state: {:?} -> syn: {:?}, fin {:?}, ack: {:?}, rst: {:?}, seqn: {:?}",
            self.state,
            tcp_header.syn(),
            tcp_header.fin(),
            tcp_header.ack(),
            tcp_header.rst(),
            seqn
        );

        match self.state {
            State::Closed => {
                if !tcp_header.rst() {
                    if tcp_header.ack() {
                        ackn
                    } else {
                        seqn + data.len() as u32
                    };
                    // self.write(nic, c, 0).unwrap();
                    self.write(nic, Request::ACK).unwrap();
                    return Ok(Action::Close);
                }
            }
            State::SynSent => {
                // we didn't implement active open yet.
                unimplemented!();
            }
            State::Listen => {
                if tcp_header.ack() {
                    self.send_rst(nic).unwrap();
                }
                if tcp_header.syn() {
                    let tcb = TCB::new(self.ip_header.clone(), self.tcp_header.clone());
                    let _o = std::mem::replace(self, tcb);
                    self.tcp_header.syn = true;
                    self.tcp_header.ack = true;
                    self.write(nic, Request::SYNACK).unwrap();
                }
                return Ok(Action::Continue);
            }
            State::SynRcvd
            | State::Estab
            | State::FinWait1
            | State::FinWait2
            | State::CloseWait
            | State::Closing
            | State::LastAck
            | State::TimeWait => {
                // RFC793 page 69

                // first check sequence number
                if let false = self.check_seq(data, &tcp_header) {
                    debug!("seqn: {:?} -> sequence number invalid", seqn);
                    if tcp_header.rst() {
                        return Ok(Action::Close);
                    }
                    self.write(nic, Request::ACK).unwrap();
                    return Ok(Action::Continue);
                }

                // second check the RST bit
                if tcp_header.rst() {
                    // FIXME: Here we just close this connection when received RST, but RFC 793
                    // suggests TCP
                    // should be transfered to proper state.
                    debug!("seqn: {:?} -> recv RST, enter state::closed", seqn);
                    self.state = State::Closed;
                    return Ok(Action::Close);
                }

                // third check security and precedence, NOT DONE
                // fourth check the SYN bit
                if tcp_header.syn() {
                    debug!("seqn: {:?} -> recv dup SYN", seqn);
                    self.send_rst(nic).unwrap();
                    self.state = State::Closed;
                    return Ok(Action::Close);
                };

                debug!(
                    "Segment: {:?} is ok for reading. una: {:?}, ackn {:?}, nxt: {:?}, closed?: {:?}",
                    seqn,
                    self.send.una,
                    tcp_header.acknowledgment_number(),
                    self.send.nxt,
                    self.closed
                );
                // fifth check the Ack bit
                // DO NOT use match pattern
                {
                    if !tcp_header.ack() {
                        return Ok(Action::Continue);
                    }
                    if let State::SynRcvd = self.state {
                        if util::le(self.send.una, ackn) && util::le(ackn, self.send.nxt) {
                            self.state = State::Estab;
                            if tcp_header.fin() {
                                self.closed = true;
                                self.state = State::LastAck;
                                self.write(nic, Request::FIN).unwrap();
                            }
                            return Ok(Action::Continue);
                        } else {
                            self.send_rst(nic).unwrap();
                            return Ok(Action::Close);
                        }
                    }
                    if let State::Estab
                    | State::CloseWait
                    | State::FinWait1
                    | State::FinWait2
                    | State::Closing = self.state
                    {
                        // ackn too small, ignore
                        if util::lt(ackn, self.send.una) {
                            return Ok(Action::Continue);
                        }
                        // ackn too large, send ack and return
                        // FIXME: which ackn should be sent?
                        if util::lt(self.send.nxt, ackn) {
                            self.write(nic, Request::ACK).unwrap();
                            // self.write(nic, self.send.nxt, 0).unwrap();
                            return Ok(Action::Continue);
                        }

                        // ackn just fits
                        if util::lt(self.send.una, ackn) && util::le(ackn, self.send.nxt) {
                            // 1. update send.una to ackn
                            // 2. Any segments on the retransmission queue which are thereby
                            //    entirely acknowledged are removed
                            // NOTE: send.nxt will be updated in the next steps
                            // NOTE: only use clear() in ideal situation.So we do an assert.
                            // assert_eq!(self.unacked.len(), ackn.wrapping_sub(self.send.una));
                            if util::lt(self.send.wl1, seqn)
                                || (self.send.wl1 == seqn && util::le(self.send.wl2, ackn))
                            {
                                self.send.wnd = tcp_header.window_size();
                                self.send.wl1 = seqn;
                                self.send.wl2 = ackn;
                            }

                            self.outgoing.clear();

                            self.update_srtt(ackn).unwrap();

                            self.send.una = ackn;
                            self.timers.send_times.remove(&self.send.una);

                            // NOTE: do not return and do not wirte anything right now, because
                            // there may be a FIN.
                        }
                    }

                    // This ACK is for our FIN which was sent with payloads together.
                    if self.closed {
                        // if self.closed_at.expect("why closed_at not set?") == ackn {
                        if self.closed_at.is_some() {
                            match self.state {
                                State::FinWait1 => self.state = State::FinWait2,
                                State::FinWait2 => {}
                                State::Closing => self.state = State::TimeWait,
                                State::LastAck => {
                                    self.state = State::Closed;
                                    debug!("seqn: {:?}, got ack for our FIN, now perish", seqn);
                                    return Ok(Action::Close);
                                }
                                State::TimeWait => {
                                    // Here we received the retransmission queue of FIN, we should ACK this FIN
                                    self.write(nic, Request::ACK).unwrap();
                                    // self.write(nic, self.send.nxt, 0).unwrap();
                                    self.timers
                                        .send_times
                                        .insert(self.send.una, time::Instant::now());
                                    return Ok(Action::Continue);
                                }
                                _ => unreachable!(),
                            }
                        }
                    }
                };

                let mut req: Option<Request> = None;
                let mut act: Option<Action> = None;
                // sixth check the URG bit, NOT DONE
                // seventh, process the segment text
                if let State::Estab | State::FinWait1 | State::FinWait2 = self.state {
                    if !data.is_empty() {
                        // If incoming has no space for this data, an error should return but we
                        // still need to ack this data. (RFC 793 page 58). Since our incoming is
                        // growable, we will not handle it.
                        self.incoming.extend(&data[..]);
                        self.recv.nxt = seqn.wrapping_add(data.len() as u32);
                        req = Some(Request::ACK);
                    }
                }

                // eighth check the FIN bit
                // here we just only adjust the state.
                if tcp_header.fin() {
                    if let State::Closed | State::Listen | State::SynSent = self.state {
                        return Ok(Action::Continue);
                    }
                    self.recv.nxt = 1u32.wrapping_add(self.recv.nxt);
                    req = Some(Request::ACK);

                    match self.state {
                        State::SynRcvd | State::Estab => {
                            // send [FIN ACK] and send all outgoing
                            self.closed = true;
                            self.state = State::LastAck;
                            req = Some(Request::FIN);
                            act = Some(Action::Continue);
                            self.closed_at = Some(self.send.una);
                        }
                        State::FinWait1 => {
                            if self.closed_at.expect("why closed_at not set?")
                                == ackn.wrapping_sub(1)
                            {
                                self.state = State::TimeWait;
                                self.timers
                                    .send_times
                                    .insert(self.send.una, time::Instant::now());
                            } else {
                                self.state = State::Closing;
                            }
                        }
                        State::FinWait2 => {
                            self.state = State::TimeWait;
                            self.timers
                                .send_times
                                .insert(self.send.una, time::Instant::now());
                        }
                        State::TimeWait => {
                            self.timers
                                .send_times
                                .insert(self.send.una, time::Instant::now());
                        }
                        _ => {}
                    }
                }

                debug!("seqn: {:?} -> now state: {:?}", seqn, self.state);
                if req.is_some() {
                    self.write(nic, req.unwrap()).unwrap();
                }
                if act.is_some() {
                    return Ok(act.unwrap());
                }
            }
        }

        return Ok(Action::Read);
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

    /// reset timeout and update srtt
    fn update_srtt(&mut self, ackn: u32) -> io::Result<()> {
        let acked = std::mem::replace(&mut self.timers.send_times, BTreeMap::new());
        let una = self.send.una;
        self.timers
            .send_times
            .extend(acked.into_iter().filter_map(|(seq, sent)| {
                if util::segment_valid(una, seq, ackn) {
                    // SRTT = ( ALPHA * SRTT ) + ((1-ALPHA) * RTT)
                    self.timers.srtt =
                        0.8 * self.timers.srtt + (1.0 - 0.8) * sent.elapsed().as_secs_f64();
                    None
                } else {
                    Some((seq, sent))
                }
            }));
        Ok(())
    }
}
