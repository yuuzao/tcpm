use log::{debug, info};
use std::io;
use std::io::{Read, Write};
use std::thread;
use tcpm::iface;
use tcpm::util;

fn main() -> io::Result<()> {
    util::logging("debug");
    // util::logging("info");
    let mut nic = iface::Interface::new("tcpm")?;
    info!("Main: created interface");
    let mut listener = nic.bind(8009)?;
    while let Ok(mut stream) = listener.try_new() {
        info!("Main: Got connection!");
        // readandwrite(stream).unwrap();

        // TODO: replace this with function above
        thread::spawn(move || {
            stream.write(b"hello world\n").unwrap();
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

use tcpm::stream::TcpStream;
/// Just a test.
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
