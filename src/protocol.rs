use crate::util;
use log::debug;
use std::collections::VecDeque;
use std::io;

pub struct TCB {
    state: State,
    send: SendSequenceSpace,
    recv: RecvSequenceSpace,
    ip_header: etherparse::Ipv4Header,
    tcp_header: etherparse::TcpHeader,

    pub(crate) incoming: VecDeque<u8>,
    pub(crate) unacked: VecDeque<u8>,
}

enum State {
    SynRcvd,
    Estab,
    FinWait1,
    FinWait2,
    TimeWait,
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
        let buf = [0u8; 1500];
        if !tcp_header.syn() {
            return Ok(None);
        }

        let iss = 123;
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
        };
        tcb.tcp_header.syn = true;
        tcb.tcp_header.ack = true;

        tcb.write(nic, &[]).unwrap();

        Ok(Some(tcb))
    }

    fn write(&mut self, nic: &mut tun_tap::Iface, payload: &[u8]) -> io::Result<usize> {
        let mut buf = [0u8; 1500];
        self.tcp_header.sequence_number = self.send.nxt;
        self.tcp_header.acknowledgment_number = self.recv.nxt;
        debug!(
            "TCB::new: tcp header length: {:?}",
            self.tcp_header.header_len()
        );

        // the ip packet to be sent should not exceeds the MTU.
        let data_size = std::cmp::min(
            buf.len(),
            // self.ip_header.total_len() as usize + payload.len() as usize,
            self.tcp_header.header_len() as usize
                + self.ip_header.header_len() as usize
                + payload.len(),
        );
        self.ip_header
            .set_payload_len(data_size - self.ip_header.header_len())
            .expect("set payload failed");
        self.tcp_header.checksum = self
            .tcp_header
            .calc_checksum_ipv4(&self.ip_header, &[])
            .expect("failed to cal checksum");

        // write the header and payload into buf.
        // buf is an array which doesn't have write trait.
        // we can use a slice for it.
        // note: this slice references to the unwritten part for buf.
        use std::io::Write;
        let mut writer = &mut buf[..];

        self.ip_header
            .write(&mut writer)
            .expect("ip header write failed");
        self.tcp_header
            .write(&mut writer)
            .expect("tcp header write failed");

        let payload_bytes = writer
            .write(payload)
            .expect("failed write payload into buf");
        let unwritten = writer.len();

        // update connection's send sequence space
        self.send.nxt = self.send.nxt.wrapping_add(payload_bytes as u32);

        // SYN in tcp_header only occurs once in first or second handshake.
        // FIN also only occurs once.
        // they have to be removed later.
        if self.tcp_header.syn {
            // The SYN in header means this packet to be sent is for handshaking.
            // So that the payload is empty, we need add 1 to send.nxt
            self.send.nxt = self.send.nxt.wrapping_add(1);
            self.tcp_header.syn = false;
        }
        if self.tcp_header.fin {
            // If we initiate the close and set the FIN, we have to
            // to send a ACK later but not more data, so we need to add 1 to
            // send.nxt, and remove the FIN.
            // If we passively close the connection, when the FIN is set, we
            // don't have to send any other packet later, we don't care about
            // what the snd.nxt is. For convenient, we add 1 to it.
            self.send.nxt = self.send.nxt.wrapping_add(1);
            self.tcp_header.fin = false;
        }

        debug!(
            "TCB::write: writing data with length: {:02x?}",
            buf.len() - unwritten
        );
        nic.send(&buf[..buf.len() - unwritten])
            .expect("nic send failed");
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
        if tcp_header.syn() {
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
            } else if !util::is_between_wrapped(self.recv.nxt.wrapping_sub(1), seqn, wend)
                && !util::is_between_wrapped(
                    self.recv.nxt.wrapping_sub(1),
                    seqn.wrapping_add(data_len - 1),
                    wend,
                )
            {
                false
            } else {
                true
            }
        };

        if !acceptable {
            debug!("TCB::unpack: Packet not acceptable");
            self.write(nic, &[]).expect("TCB write failed");
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
            if util::is_between_wrapped(self.send.una, ackn, self.send.nxt.wrapping_add(1)) {
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

            if !util::is_between_wrapped(self.send.una, ackn, self.send.nxt.wrapping_add(1)) {
                debug!("TCB::unpack: Invalid sequence number");
                return Ok(());
            }

            if !self.unacked.is_empty() {
                let data_start = if self.send.una == self.send.iss {
                    self.send.una.wrapping_add(1)
                } else {
                    self.send.una
                };
                let acked_data_end =
                    std::cmp::min(ackn.wrapping_sub(data_start) as usize, self.unacked.len());
                self.unacked.drain(..acked_data_end);
                self.send.una = ackn;
            }
        }

        if let State::FinWait1 = self.state {
            if self.send.una == self.send.iss + 2 {
                debug!("TCB::unpack::Fin-Wait1: Got ACK for our FIN");
                self.state = State::FinWait2;
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
                self.write(nic, &[])?;
            }
        }

        if tcp_header.fin() {
            match self.state {
                State::FinWait2 => {
                    debug!("TCB::unpack: connection state turn into TimeWait");
                    self.write(nic, &[]);
                    self.state = State::TimeWait;
                }
                _ => unimplemented!(),
            }
        }

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
}

pub enum Available {
    READ,
    WRITE,
}
