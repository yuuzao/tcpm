use log::{debug, info};
use std::io;
use std::io::{Read, Write};
use std::thread;
use tcpm::iface;
use tcpm::util;

fn main() -> io::Result<()> {
    // util::logging("debug");
    util::logging("info");
    let mut nic = iface::Interface::new("tcpm")?;
    info!("Main: created interface");
    let mut listener = nic.bind(8005)?;
    while let Ok(mut stream) = listener.try_new() {
        info!("Main: Got connection!");

        // TODO: replace this with function above
        thread::spawn(move || {
            stream.write(b"hello world\n").unwrap();
            stream.write(b"foo bar\n").unwrap();
            stream.shutdown(std::net::Shutdown::Write).unwrap();
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
        });
    }
    Ok(())
}
