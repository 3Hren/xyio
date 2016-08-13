#![feature(ip_addr)]
#![feature(into_raw_os)]

#[macro_use] extern crate log;
extern crate nix;
extern crate xyio;

use std::net::{IpAddr, SocketAddr};
use std::net;
use std::os::unix::io::IntoRawFd;
use std::str::FromStr;

use log::LogLevel;

use nix::errno::Errno;

use xyio::io::{Context, HandleWrite, HandleRead};
use xyio::io::net::ip::tcp::TcpStream;
use xyio::logging;

struct ConnectionHandler<'a> {
    wrbuf: Box<[u8]>,
    rdbuf: Box<[u8]>,
    stream: TcpStream<'a>,
}

impl<'a> HandleWrite for ConnectionHandler<'a> {
    fn complete(self, result: Result<usize, Errno>) {
        debug!("write complete: {}", result.unwrap());
        self.stream.clone().async_read_some(self);
    }

    fn buf(&self) -> &[u8] {
        &self.wrbuf[..]
    }
}

impl<'a> HandleRead for ConnectionHandler<'a> {
    fn complete(self, result: Result<usize, Errno>) {
        debug!("read complete: {}", result.unwrap());
    }

    fn buf(&mut self) -> &mut [u8] {
        &mut self.rdbuf[..]
    }
}

fn on_connect(stream: Result<TcpStream, Errno>) {
    match stream {
        Ok(mut stream) => {
            debug!("connected");
            let handler = ConnectionHandler {
                wrbuf: vec![147u8, 1, 0, 145, 167, 115, 116, 111, 114, 97, 103, 101].into_boxed_slice(),
                rdbuf: vec![0; 4096].into_boxed_slice(),
                stream: stream.clone()
            };
            // TODO: I don't like this API - cloning sockets seems to be not the best idea.
            stream.async_write_some(handler);
        }
        Err(err) => {
            error!("failed to connect: {:?}", err);
        }
    }
}

fn main() {
    logging::init(LogLevel::Trace).unwrap();

    let mut context = Context::new().unwrap();

    let (ip, port) = ("::", 10053);

    let ipaddr   = IpAddr::from_str(ip).ok().expect("unable to initialize destination address");
    let endpoint = SocketAddr::new(ipaddr, port);

    TcpStream::connect(endpoint, on_connect, &context).unwrap();

    let mut stream = unsafe {
        TcpStream::from_connected(
            net::TcpStream::connect((ip, port)).unwrap().into_raw_fd(),
            &context
        ).unwrap()
    };

    let handler = ConnectionHandler {
        wrbuf: vec![147u8, 1, 0, 145, 167, 115, 116, 111, 114, 97, 103, 101].into_boxed_slice(),
        rdbuf: vec![0; 4096].into_boxed_slice(),
        stream: stream.clone()
    };
    stream.async_write_some(handler);

    context.run();
}
