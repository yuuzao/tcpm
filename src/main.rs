extern crate crossbeam;
extern crate crossbeam_channel;
use log::{debug, info};
use std::io;
use std::io::{BufRead, Read, Write};
use std::thread;
use std::time;
use tcpm::iface;
use tcpm::util;

fn main() -> io::Result<()> {
    // util::logging("debug");
    let (tx, rx) = crossbeam_channel::unbounded();
    thread::spawn(move || loop {
        let mut buffer = String::new();
        let stdin = io::stdin();
        let mut handle = stdin.lock();
        handle.read_line(&mut buffer).unwrap();
        tx.send(buffer).unwrap();
        thread::sleep(time::Duration::from_secs_f64(0.1));
    });

    let mut nic = iface::Interface::new("tcpm")?;
    info!("Main: created interface");
    let mut listener = nic.bind(8010)?;
    while let Ok(mut stream) = listener.try_new() {
        let mut s = stream.clone();
        let rx = rx.clone();
        info!("Main: Got connection!");
        thread::spawn(move || {
            stream.write(b"hello, world\n").unwrap();
            loop {
                let mut buf = [0; 1024];
                let n = stream.read(&mut buf[..]).unwrap();
                info!("read {}b data", n);
                if n == 0 {
                    info!("Main: No more incoming data");
                    break;
                } else if n > 1 {
                    let msg = String::from_utf8(buf[..n].to_vec()).unwrap();
                    println!(">>> {}", msg.trim());
                    let mut ech = String::from("echo > ");
                    ech.push_str(&msg);
                    stream.write(ech.as_bytes()).unwrap();
                }
            }
            stream.shutdown().unwrap();
        });
        thread::spawn(move || loop {
            if let Ok(msg) = rx.try_recv() {
                if let Err(_) = s.write(msg.as_bytes()) {
                    break;
                };
            }
            thread::sleep(time::Duration::from_secs_f64(0.1));
        });
    }
    return Ok(());
}
