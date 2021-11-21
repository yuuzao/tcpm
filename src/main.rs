use log::{debug, info};
use std::{io, io::Read, io::Write, thread};
use tcpm::iface;

fn main() -> io::Result<()> {
    tcpm::util::logging("debug");

    let mut nic = iface::Interface::new("tcpm")?;
    info!("created interface");
    let mut listener = nic.bind(8008)?;
    while let Ok(mut stream) = listener.try_new() {
        thread::spawn(move || {
            stream.write(b"hello from tcpm\n").unwrap();
            stream.shutdown(std::net::Shutdown::Write).unwrap();
            loop {
                let mut buf = [0; 1024];
                let n = stream.read(&mut buf[..]).unwrap();
                debug!("read {}b data", n);
                if n == 0 {
                    info!("No more incoming data");
                    break;
                } else {
                    info!("{}", std::str::from_utf8(&buf[..n]).unwrap());
                }
            }
        });
    }
    Ok(())
}
