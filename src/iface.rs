use crate::protocol::TCB;
use crate::stream::TcpListener;
use crate::stream::{Acm, SocketPair};
use log::{debug, error, info};
use std::collections::{hash_map::Entry, HashMap, VecDeque};
use std::io;
use std::thread;
use tun_tap;
pub struct Interface {
    jh: Option<thread::JoinHandle<io::Result<()>>>,
    m: Option<Acm>,
}

impl Interface {
    pub fn new(ifacename: &str) -> io::Result<Self> {
        debug!("Interface: created new interface");
        let nic = tun_tap::Iface::without_packet_info(ifacename, tun_tap::Mode::Tun)
            .expect("Failed to create interface");

        let acm = Acm::default();

        let jh = {
            let acm = acm.clone();
            thread::spawn(move || packet_loop(nic, acm))
        };
        Ok(Self {
            jh: Some(jh),
            m: Some(acm),
        })
    }
}
impl Drop for Interface {
    fn drop(&mut self) {
        self.jh
            .take()
            .expect("trying to drop interface more than once")
            .join()
            .expect("Join failed on interface")
            .unwrap();
    }
}
impl Interface {
    pub fn bind(&mut self, port: u16) -> io::Result<TcpListener> {
        // let mut cm = self.m.as_mut().unwrap();
        let mut cm = self.m.as_mut().unwrap().manager.lock().unwrap();
        match cm.pending.entry(port) {
            Entry::Vacant(v) => {
                v.insert(VecDeque::new());
            }
            Entry::Occupied(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    "port already bond",
                ))
            }
        }
        drop(cm);
        Ok(TcpListener {
            port,
            m: self.m.clone().unwrap(),
        })
    }
}

/// This function is initialized by the new() method of Interface. It will
/// never ends and watches the incoming tcp packets and then notify the
/// related threads to process on.
// TODO: ih.notify
fn packet_loop(mut nic: tun_tap::Iface, acm: Acm) -> io::Result<()> {
    let mut buf = [0u8; 1500];

    loop {
        let buf_len = nic.recv(&mut buf[..])?;
        // let's ignore non-IP packets
        if !matches!(
            etherparse::SlicedPacket::from_ip(&buf[..buf_len])
                .unwrap()
                .ip,
            Some(etherparse::InternetSlice::Ipv4(_))
        ) {
            // debug!("Interface: not a Ipv4 packet");
            continue;
        }

        match etherparse::Ipv4HeaderSlice::from_slice(&buf[..buf_len]) {
            Ok(ip_header) => {
                // let's ignore non-TCP packets
                //LINK https://en.wikipedia.org/wiki/List_of_IP_protocol_numbers
                if ip_header.protocol() != 6 {
                    // debug!(
                    //     "Interface: not a TCP packet, protocol number is {}",
                    //     ip_header.protocol()
                    // );
                    continue;
                }

                debug!(
                    "Interface: got a TCP packet, Ipv4 Packet content length: {}",
                    buf_len
                );

                // ihl in ip header stands for the ip header's length, note the unit is 4bytes.
                match etherparse::TcpHeaderSlice::from_slice(
                    &buf[ip_header.ihl() as usize * 4..buf_len],
                ) {
                    Ok(tcp_header) => {
                        // assign four vars for less confusing.
                        // FIXME: delete these vars to reduce memory allocation.
                        let remote_addr = ip_header.source_addr();
                        let remote_port = tcp_header.source_port();
                        let local_addr = ip_header.destination_addr();
                        let local_port = tcp_header.destination_port();
                        let sp = SocketPair {
                            src: (remote_addr, remote_port),
                            dst: (local_addr, local_port),
                        };

                        let mut cm_guard = acm
                            .manager
                            .lock()
                            .expect("failed to get lock in packet_loop");

                        let cm = &mut *cm_guard;
                        match cm.connections.entry(sp) {
                            //new connection comes as vacant
                            Entry::Vacant(con) => {
                                debug!("Interface: Got a packet from a new client");
                                if let Some(pending) = cm.pending.get_mut(&local_port) {
                                    debug!(
                                        "Listener: port is on, now trying to establish connection"
                                    );
                                    if let Some(c) = TCB::new(&mut nic, ip_header, tcp_header)? {
                                        debug!("Listener: connection established");
                                        con.insert(c);
                                        pending.push_back(sp);
                                        drop(cm);
                                        acm.estab_notifier.notify_all();
                                    }
                                } else {
                                    debug!("Listener: Port is off, ignoring...")
                                }
                            }

                            // existed connections comes as occupied
                            // TODO: complete this
                            Entry::Occupied(mut con) => {
                                debug!("Interface: got a packet from known a connection");

                                let data_start = ip_header.ihl() as usize * 4
                                    + tcp_header.slice().len() as usize;
                                con.get_mut()
                                    .unpack(&mut nic, tcp_header, &buf[data_start..buf_len])
                                    .expect("unpacking TCP packet failed");
                                drop(cm);
                                acm.reading_notifier.notify_all();
                            }
                        }
                    }
                    Err(e) => {
                        error!("parsed some weird tcp packet: {:?}", e);
                    }
                }
            }
            Err(e) => {
                error!("parse some weird ip packets: {:?}", e);
            }
        }
    }
}
