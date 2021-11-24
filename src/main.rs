use log::{debug, info};
use std::process::Command;
use std::time;
use std::{io, io::Read, io::Write, thread};
use tcpm::iface;
use tcpm::util;

fn main() -> io::Result<()> {
    // util::logging("debug");
    util::logging("info");
    let mut nic = iface::Interface::new("tcpm")?;
    info!("Main: created interface");
    let mut listener = nic.bind(8004)?;
    while let Ok(stream) = listener.try_new() {
        readandwrite(stream).unwrap();
    }
    Ok(())
}

use tcpm::stream::TcpStream;
fn readandwrite(mut stream: TcpStream) -> io::Result<()> {
    let mut w = stream.clone();
    let th = thread::spawn(move || loop {
        let mut hello = String::new();
        // TODO: catch terminate signal: ctrl-c or ctrl-d
        io::stdin().read_line(&mut hello).unwrap();
        w.write(&hello.as_bytes()).unwrap();
    });
    loop {
        let mut buf = [0; 1024];
        let n = stream.read(&mut buf[..]).unwrap();
        debug!("read {}b data", n);
        if n == 0 {
            info!("Main: No more incoming data");
            break;
        } else {
            info!("{}", std::str::from_utf8(&buf[..]).unwrap());
        }
    }
    info!("Main: Connection shutdown");
    th.join().unwrap();
    stream.shutdown(std::net::Shutdown::Write).unwrap();

    Ok(())
}
