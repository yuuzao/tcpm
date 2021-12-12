use log::{debug, info};
use std::io;
use std::io::{Read, Write};
use std::thread;
use std::time;
use tcpm::iface;
use tcpm::util;

fn main() -> io::Result<()> {
    // util::logging("debug");
    let mut nic = iface::Interface::new("tcpm")?;
    info!("Main: created interface");
    let mut listener = nic.bind(8012)?;
    while let Ok(mut stream) = listener.try_new() {
        info!("Main: Got connection!");

        thread::spawn(move || loop {
            let mut buf = [0; 1024];
            let n = stream.read(&mut buf[..]).unwrap();
            info!("read {}b data", n);
            if n == 0 {
                info!("Main: No more incoming data");
                break;
            } else {
                if n >= 3 && std::str::from_utf8(&buf[..3]).unwrap() == "GET" {
                    stream
                        .write(
                            b"HTTP/1.0 200 OK
Content-Type: text/html\n\n",
                        )
                        .unwrap();
                } else {
                    stream.write(b"hello, world").unwrap();
                    stream.shutdown().unwrap();
                }
            }
        });
    }
    Ok(())
}
