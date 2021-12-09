use log::{debug, info};
use std::io;
use std::io::{Read, Write};
use std::thread;
use std::time;
use tcpm::iface;
use tcpm::util;

fn main() -> io::Result<()> {
    // util::logging("debug");
    util::logging("debug");
    let mut nic = iface::Interface::new("tcpm")?;
    info!("Main: created interface");
    let mut listener = nic.bind(8005)?;
    while let Ok(mut stream) = listener.try_new() {
        info!("Main: Got connection!");

        thread::spawn(move || {
            stream.write(b"hello from tcpm, oh yes!\n").unwrap();
            // loop {
            let mut buf = [0; 24];
            let n = stream.read(&mut buf[..]).unwrap();
            info!("read {}b data", n);
            if n == 0 {
                info!("Main: No more incoming data");
                // break;
            } else {
                info!(">> {}", std::str::from_utf8(&buf[..]).unwrap());
            }
            // }
            stream.write(b"hello from tcpm, oh yes!\n").unwrap();

            stream.shutdown().unwrap();
        });
    }
    Ok(())
}
