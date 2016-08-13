use std::net::SocketAddr;

use nix;
use nix::errno;
use nix::errno::Errno;
use nix::sys::*;

use super::super::super::{Context, EventData, Operation, HandleWrite, HandleRead};

#[derive(Clone)]
pub struct TcpStream<'a> {
    fd: i32,
    context: Context<'a>,
}

impl<'a> TcpStream<'a> {
    pub fn connect<F>(addr: SocketAddr, callback: F, context: &Context<'a>) -> Result<(), Errno>
        where F: FnOnce(Result<TcpStream<'a>, Errno>) + 'a
    {
        let family = match addr {
            SocketAddr::V4(..) => { socket::AddressFamily::Inet }
            SocketAddr::V6(..) => { socket::AddressFamily::Inet6 }
        };

        let socktype = socket::SockType::Stream;
        let flags    = socket::SOCK_NONBLOCK;

        let fd = match socket::socket(family, socktype, flags) {
            Ok(fd) => fd,
            Err(err) => {
                error!(target: "Sock", "failed to init socket: {:?}", err.errno());
                return Err(err.errno());
            }
        };

        trace!(target: "Sock", "init TCP socket, fd: {}", fd);

        try!(TcpStream::register_descriptor(fd, context));

        // Try to connect immediately. Post poll operation only if unable to complete right now.
        trace!(target: "Sock", "connecting to {}", addr);
        match socket::connect(fd, &socket::SockAddr::new_inet(socket::InetAddr::from_std(&addr))) {
            Ok(()) => {
                trace!(target: "Sock", "connected");
                unimplemented!();
            }
            Err(nix::Error::Sys(errno::EINPROGRESS)) => {
                trace!(target: "Sock", "connection in progress");
                // TODO: Post watcher.

                // 4. Register descriptor.
                let on_write = {
                    let context = context.clone();

                    Box::new(move || {
                        let stream = TcpStream {
                            fd: fd,
                            context: context,
                        };

                        callback(Ok(stream));
                    })
                };

                let evd = EventData::new(None, Some(on_write));
                context.d.descriptors.insert(fd, evd);

                // 5. Push Poll operation.
                context.d.queue.push_back(Operation::Poll);

                Ok(())
            }
            Err(err) => {
                error!(target: "Sock", "failed to connect");
                unimplemented!();
            }
        }
    }

    pub unsafe fn from_connected(fd: i32, context: &Context<'a>) -> Result<TcpStream<'a>, Errno> {
        trace!(target: "Sock", "init TCP socket, fd: {}", fd);

        try!(TcpStream::register_descriptor(fd, context));

        context.d.descriptors.insert(fd, EventData::new(None, None));

        let stream = TcpStream {
            fd: fd,
            context: context.clone(),
        };

        Ok(stream)
    }

    fn register_descriptor(fd: i32, context: &Context<'a>) -> Result<(), Errno> {
        let wevent = event::KEvent {
            ident: fd as u64,
            filter: event::EventFilter::EVFILT_WRITE,
            flags: event::EV_ADD | event::EV_CLEAR,
            fflags: event::FilterFlag::empty(),
            data: 0,
            udata: 0,
        };

        let revent = event::KEvent {
            ident: fd as u64,
            filter: event::EventFilter::EVFILT_READ,
            flags: event::EV_ADD | event::EV_CLEAR,
            fflags: event::FilterFlag::empty(),
            data: 0,
            udata: 0,
        };

        // 3. Register write and read event.
        event::kevent(context.fd, &[wevent, revent], &mut[], 0).unwrap();

        Ok(())
    }

    pub fn async_write_some<H>(&mut self, handler: H)
        where H: HandleWrite + 'a
    {
        trace!(target: "Send", "sending {:?}", handler.buf());

        match socket::send(self.fd, handler.buf(), 0) {
            Ok(len) => {
                trace!(target: "Send", "written {} bytes", len);
                self.context.d.queue.push_back(Operation::User(Box::new(move || {
                    handler.complete(Ok(len))
                })));
            }
            Err(nix::Error::Sys(errno::EWOULDBLOCK)) => {
                trace!(target: "Send", "EWOULDBLOCK");
                // TODO: Ha-ha, I've never reached this while testing.
                unimplemented!();
            }
            // TODO: Retry on EINTR.
            Err(err) => {
                warn!(target: "Send", "failed to send bytes: {:?}", err);
                unimplemented!();
            }
        }
    }

    pub fn async_read_some<H>(&mut self, mut handler: H)
        where H: HandleRead + 'a
    {
        trace!(target: "Recv", "receiving bytes");
        match socket::recv(self.fd, handler.buf(), 0) {
            Ok(0) => {
                //TODO: EOF or just poll if handler.buf().len() == 0.
                trace!(target: "Recv", "read EOF (or null buffer)");
                self.context.d.queue.push_back(Operation::User(Box::new(move || {
                    handler.complete(Ok(0))
                })));
            }
            Ok(nread) => {
                trace!(target: "Recv", "read {} bytes", nread);
                self.context.d.queue.push_back(Operation::User(Box::new(move || {
                    handler.complete(Ok(nread))
                })));
            }
            Err(nix::Error::Sys(nix::errno::EWOULDBLOCK)) => {
                trace!(target: "Recv", "EWOULDBLOCK");

                // NOTE: 1. Register callback in fdset.
                let ev = self.context.d.descriptors.get_mut(&self.fd).unwrap();
                let mut stream = self.clone();
                ev.read_handler = Some(Box::new(move || {
                    stream.async_read_some(handler)
                }));

                // NOTE: 2. Add epoll_wait task to the queue.
                self.context.d.queue.push_back(Operation::Poll);
            }
            // TODO: EAGAIN, EINTR
            Err(err) => {
                warn!(target: "Recv", "failed to read: {:?}", err);
                unimplemented!();
            }
        }
    }
}
