use log::{debug, error, info};
use protocol::TransmissionControlBlock;
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::io;
use std::net::Ipv4Addr;

#[allow(unused)]
mod protocol;
mod util;

#[derive(PartialEq, Eq, Debug, Clone, Copy, Hash)]
struct Quad {
    src: (Ipv4Addr, u16),
    dst: (Ipv4Addr, u16),
}

type TCB = TransmissionControlBlock;
fn main() -> io::Result<()> {
    util::logging();

    let mut nic = tun_tap::Iface::without_packet_info("mytcp", tun_tap::Mode::Tun)?;
    info!("created interface: {}", nic.name());

    let mut connection: HashMap<Quad, protocol::TransmissionControlBlock> = Default::default();

    let mut buf = [0u8; 1500]; // size == MTU
    loop {
        let buf_len = nic.recv(&mut buf[..])?;
        if !matches!(
            etherparse::SlicedPacket::from_ip(&buf[..buf_len])
                .unwrap()
                .ip,
            Some(etherparse::InternetSlice::Ipv4(_))
        ) {
            debug!("Not a Ipv4 packet");
            continue;
        }

        // let's ignore ipv6 packets
        match etherparse::Ipv4HeaderSlice::from_slice(&buf[..buf_len]) {
            Ok(ip_header) => {
                if ip_header.protocol() != 6 {
                    //https://en.wikipedia.org/wiki/List_of_IP_protocol_numbers
                    debug!("Protocol is {}, not a TCP packet...", ip_header.protocol());
                    continue;
                }

                info!("Got a TCP packet");
                debug!("Ipv4 Packet content: {:02x?}", &buf[..buf_len]);

                // ihl in ip header indicates the header's length, note the unit is 32bit or 4bytes.
                match etherparse::TcpHeaderSlice::from_slice(
                    &buf[ip_header.ihl() as usize * 4..buf_len],
                ) {
                    Ok(tcp_header) => {
                        debug!("TCP header: {:02x?}", &tcp_header);
                        match connection.entry(Quad {
                            src: (ip_header.source_addr(), tcp_header.source_port()),
                            dst: (ip_header.destination_addr(), tcp_header.destination_port()),
                        }) {
                            Entry::Vacant(v) => {
                                if let Some(c) = TCB::establish(&mut nic, ip_header, tcp_header)
                                    .expect("failed when establishing new connection")
                                {
                                    v.insert(c);
                                    info!("New connection established!");
                                }
                            }
                            Entry::Occupied(mut c) => {
                                c.get_mut()
                                    .unpack(
                                        &mut nic,
                                        tcp_header,
                                        &buf[ip_header.total_len() as usize..buf_len],
                                    )
                                    .expect("unpacking TCP packet failed");
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
