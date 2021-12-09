use crate::protocol::Action;
use crate::protocol::TCB;
use crate::stream::TcpListener;
use crate::stream::{Acm, SocketPair};
use log::{error, info};
use nix;
use std::collections::{hash_map::Entry, VecDeque};
use std::io;
use std::thread;
use tun_tap;

pub struct Interface {
    jh: Option<thread::JoinHandle<io::Result<()>>>,
    m: Option<Acm>,
}

impl Interface {
    pub fn new(ifacename: &str) -> io::Result<Self> {
        info!("Interface: created new interface");
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

/// This function is initialized by the new() method of Interface. It is a loop
/// for writing and reading.
/// We use epoll for incoming data, reading will be waked up if the POLLIN fd is
/// positive. If there's no incoming data, on_tick will be waked up for writing.
fn packet_loop(mut nic: tun_tap::Iface, acm: Acm) -> io::Result<()> {
    info!("packet loop begins!");
    let mut buf = [0u8; 1500];
    let mut pending_remove: Vec<SocketPair> = vec![];
    loop {
        use std::os::unix::io::AsRawFd;
        let mut pfd = [nix::poll::PollFd::new(
            nic.as_raw_fd(),
            nix::poll::PollFlags::POLLIN,
        )];
        let n = nix::poll::poll(&mut pfd[..], 10).unwrap();

        let mut cm_guard = acm.manager.lock().unwrap();
        while let Some(k) = pending_remove.pop() {
            cm_guard.connections.remove(&k);
            info!("connection {:?} removed", &k);
        }
        drop(cm_guard);

        if n == 0 {
            let mut cm_guard = acm.manager.lock().unwrap();

            for (k, v) in cm_guard.connections.iter_mut() {
                let act = v.on_tick(&mut nic).unwrap();
                if let Action::Close = act {
                    pending_remove.push(k.clone());
                };
            }
            continue;
        }

        assert_eq!(n, 1);

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
                // LINK https://en.wikipedia.org/wiki/List_of_IP_protocol_numbers
                if ip_header.protocol() != 6 {
                    continue;
                }

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
                        let act = match cm.connections.entry(sp) {
                            //new connection comes as vacant
                            Entry::Vacant(con) => {
                                if let Some(pending) = cm.pending.get_mut(&local_port) {
                                    if let Some(c) =
                                        TCB::new_connection(&mut nic, ip_header, tcp_header)
                                            .unwrap()
                                    {
                                        info!("new connection into pending");
                                        con.insert(c);
                                        pending.push_back(sp);
                                        Action::New
                                    } else {
                                        // TODO: recovery from old connection
                                        info!("Old Connection exists");
                                        Action::Close
                                    }
                                } else {
                                    info!("Listener: Port is off, ignoring...");
                                    Action::Close
                                }
                            }

                            // Existed connections comes into occupied
                            Entry::Occupied(mut con) => {
                                let data_start = ip_header.ihl() as usize * 4
                                    + tcp_header.slice().len() as usize;
                                let act = con
                                    .get_mut()
                                    .on_segment(&mut nic, tcp_header, &buf[data_start..buf_len])
                                    .unwrap();
                                act
                            }
                        };
                        // cm must be dropped before notify_all.
                        match act {
                            Action::New => {
                                drop(cm_guard);
                                acm.estab_notifier.notify_all()
                            }
                            Action::Read => {
                                drop(cm_guard);
                                acm.reading_notifier.notify_all()
                            }
                            Action::Continue => continue,
                            Action::Close => {
                                cm.connections.remove(&sp);
                            }
                        }
                    }
                    Err(e) => {
                        error!("parsed some weird TCP packet: {:?}", e);
                    }
                }
            }
            Err(e) => {
                error!("parse some weird IP packets: {:?}", e);
            }
        }
    }
}
