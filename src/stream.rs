use crate::protocol;
use log::debug;
use std::collections::{HashMap, VecDeque};
use std::fmt::Debug;
use std::io;
use std::io::{Read, Write};
use std::net::Ipv4Addr;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

pub type Acm = Arc<AtomicallyConnectionManager>;
#[derive(Default)]
pub struct AtomicallyConnectionManager {
    pub manager: Mutex<ConnectionManager>,
    pub estab_notifier: Condvar,
    pub reading_notifier: Condvar,
}
#[derive(Debug, Hash, PartialEq, Eq, Clone, Copy)]
pub struct SocketPair {
    pub src: (Ipv4Addr, u16),
    pub dst: (Ipv4Addr, u16),
}

#[derive(Default)]
pub struct ConnectionManager {
    pub connections: HashMap<SocketPair, protocol::TCB>,
    pub pending: HashMap<u16, VecDeque<SocketPair>>,
}

pub struct TcpListener {
    pub port: u16,
    pub m: Acm,
}

impl TcpListener {
    pub fn try_new(&mut self) -> io::Result<TcpStream> {
        let mut cm = self.m.manager.lock().unwrap();
        loop {
            cm = self.m.estab_notifier.wait(cm).unwrap();
            if let Some(sp) = cm
                .pending
                .get_mut(&self.port)
                .expect("port closed while listener still active")
                .pop_front()
            {
                debug!("Listener: Let's Streaming!!!");
                return Ok(TcpStream {
                    socketpair: sp,
                    m: self.m.clone(),
                });
            }
        }
    }
}

#[derive(Clone)]
pub struct TcpStream {
    socketpair: SocketPair,
    m: Arc<AtomicallyConnectionManager>,
}

impl Read for TcpStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut cm = self
            .m
            .manager
            .lock()
            .expect("failed to get lock in reading");
        loop {
            cm = self.m.reading_notifier.wait(cm).unwrap();
            debug!("Stream::Read: Reader awaked");

            let c = cm.connections.get_mut(&self.socketpair).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "stream terminated unexpectedly",
                )
            })?;

            if c.is_recv_closed() && c.incoming.is_empty() {
                debug!("Stream::Read: Recv closed and incoming empty, ending...");
                return Ok(0);
            }

            debug!("STREAM::READ: incoming empty? {}", c.incoming.is_empty());

            if !c.incoming.is_empty() {
                debug!("Stream::Read: start reading");
                //TODO: figure out vecdeque drain
                let mut nread = 0;
                let (head, tail) = c.incoming.as_slices();
                let head_size = std::cmp::min(buf.len(), head.len());
                buf[..head_size].copy_from_slice(&head[..head_size]);
                nread += head_size;

                let tail_size = std::cmp::min(buf.len() - nread, tail.len());
                buf[nread..(nread + tail_size)].copy_from_slice(&tail[..tail_size]);
                nread += tail_size;
                //remember drop
                drop(c.incoming.drain(..nread));
                return Ok(nread);
            }
        }
    }
}
impl Write for TcpStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        Ok(1234)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl TcpStream {
    pub fn shutdown<T>(&self, t: T) -> io::Result<()> {
        Ok(())
    }
}
