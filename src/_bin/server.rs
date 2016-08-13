//! Simple single-threaded asynchronous http "Ты пидор" server using callbacks with kqueue backend.
#![feature(fnbox)]

#[macro_use]
extern crate log;

extern crate getopts;
extern crate libc;
extern crate vec_map;

extern crate http_muncher;
extern crate nix;

extern crate xyio;

use getopts::Options;

use xyio::logging;

use std::boxed::{FnBox};
use std::cell::{UnsafeCell};
use std::net::TcpListener;
use std::os::unix::io::{AsRawFd};
use std::ptr;
use std::rc::Rc;
use std::thread;

use vec_map::VecMap;

use http_muncher::{ParserHandler, Parser};
use nix::errno::Errno;
use nix::errno;
use nix::fcntl;
use nix::sys::{event, socket};
use nix::unistd::{close, pipe2, read, write};

// ////////////////////////////////////////////////////////////////////////////////////////////////////
// ////////////////////////////////////////////////////////////////////////////////////////////////////
// ////////////////////////////////////////////////////////////////////////////////////////////////////

struct Handler {
    complete: bool,
}

impl ParserHandler for Handler {
    fn on_message_complete(&mut self) -> bool {
        self.complete = true;
        true
    }
}

#[derive(Clone)]
struct SocketRef<'a> {
    pub fd: i32,
    pub context: Rc<Context_<'a>>
}

impl<'a> SocketRef<'a> {
    fn new(fd: i32, context: Rc<Context_<'a>>) -> SocketRef<'a> {
        SocketRef {
            fd: fd,
            context: context,
        }
    }

    // NOTE: From recv(2) man.
    // When a stream socket peer has performed an orderly shutdown, the return value will be 0 (the
    // traditional "end-of-file" return).
    // The value 0 may also be returned if the requested number of bytes to receive from a stream
    // socket was 0.
    fn async_read_some<H>(&mut self, mut handler: H)
        where H: ReadHandler + 'a // I don't give a fuck why this requires 'r if Context is borrowed.
    {
        trace!(target: "Recv", "receiving bytes");
        match socket::recv(self.fd, handler.buf(), 0) {
            Ok(0) => {
                //TODO: EOF or just poll if handler.buf().len() == 0.
                trace!(target: "Recv", "read EOF");
                unsafe {
                    (*self.context.queue.get()).push_back(OperationRef::User(Box::new(move || {
                        handler.on_read(Ok(0))
                    })));
                }
            }
            Ok(nread) => {
                trace!(target: "Recv", "read {} bytes", nread);
                unsafe {
                    (*self.context.queue.get()).push_back(OperationRef::User(Box::new(move || {
                        handler.on_read(Ok(nread))
                    })));
                }
            }
            Err(nix::Error::Sys(nix::errno::EWOULDBLOCK)) => {
                trace!(target: "Recv", "EWOULDBLOCK");

                // NOTE: 1. Register callback in fdset.
                let fd = self.fd as usize;
                let ev = self.context.descriptors.get_mut(&fd).unwrap();
                ev.read_handler = Some(Box::new(move || {
                    trace!(target: "Recv", "receiving bytes");
                    match socket::recv(fd as i32, handler.buf(), 0) {
                        Ok(0) => {
                            trace!(target: "Recv", "read EOF");
                            handler.on_read(Ok(0));
                        }
                        Ok(nread) => {
                            trace!(target: "Recv", "read {} bytes", nread);
                            handler.on_read(Ok(nread));
                        }
                        // TODO: EINTR
                        Err(err) => {
                            warn!(target: "Recv", "failed to read: {:?}", err);
                            unimplemented!();
                        }
                    }
                }));

                // NOTE: 2. Add epoll_wait task to the queue.
                unsafe {
                    (*self.context.queue.get()).push_back(OperationRef::Poll);
                }
            }
            // TODO: EAGAIN, EINTR
            Err(err) => {
                warn!(target: "Recv", "failed to read: {:?}", err);
                unimplemented!();
            }
        }
    }

    fn async_write_some<H>(&mut self, handler: H)
        where H: WriteHandler + 'a
    {
        trace!(target: "Send", "sending {:?}", std::str::from_utf8(handler.get_buf()).unwrap());

        match socket::send(self.fd, handler.get_buf(), 0) {
            Ok(len) => {
                trace!(target: "Send", "written {} bytes", len);
                unsafe {
                    (*self.context.queue.get()).push_back(OperationRef::User(Box::new(move || {
                        handler.on_write(Ok(len))
                    })));
                }
            }
            Err(nix::Error::Sys(errno::EWOULDBLOCK)) => {
                trace!(target: "Send", "EWOULDBLOCK");
                // TODO: Ha-ha, I've never reached this while testing.
                unimplemented!();
            }
            Err(err) => {
                warn!(target: "Send", "failed to send bytes: {:?}", err);
                unimplemented!();
            }
        }
    }
}

struct HttpReadHandlerRef<'a> {
    rdbuf: Vec<u8>,
    wrbuf: Vec<u8>,
    written: usize,
    parser: Parser<Handler>,
    socket: SocketRef<'a>,
}

impl<'a> HttpReadHandlerRef<'a> {
    fn new(socket: SocketRef<'a>) -> HttpReadHandlerRef<'a> {
        let handler = Handler { complete: false };

        let buf = "HTTP/1.1 200 OK\r\nServer: xyio/0.1.0\r\nContent-Length: 15\r\nConnection: Keep-Alive\r\nX-Powered-By: Cocaine\r\n\r\nТы пидор".as_bytes();

        HttpReadHandlerRef {
            rdbuf: [0u8; 4096].to_vec(),
            wrbuf: buf.to_vec(),
            written: 0,
            parser: Parser::request(handler),
            socket: socket,
        }
    }
}

impl<'a> ReadHandler for HttpReadHandlerRef<'a> {
    fn on_read(mut self, nread: Result<usize, Errno>) {
        match nread {
            Ok(0) => {
                close(self.socket.fd).unwrap();
            }
            Ok(nread) => {
                {
                trace!(target: "Hand", "buffer read: {:?}", std::str::from_utf8(&self.rdbuf[..nread]).unwrap());

                let parsed = self.parser.parse(&self.rdbuf[..nread]);
                if parsed != nread {
                    trace!(target: "Hand", "parser error");
                    return;
                }

                if self.parser.has_error() {
                    trace!(target: "Hand", "parser error: {}", self.parser.error());
                    return;
                }

                if nread == 0 {
                    trace!(target: "Hand", "EOF");
                    return;
                } else {
                    if self.parser.get().complete {
                        trace!(target: "Hand", "complete");

                        let mut sock = self.socket.clone();
                        sock.async_write_some(self);
                        return;
                    }
                }
                }

                let mut sock = self.socket.clone();
                sock.async_read_some(self);
            }
            Err(err) => {
                unimplemented!();
            }
        }
    }

    fn buf(&mut self) -> &mut [u8] {
        &mut self.rdbuf[..]
    }
}

impl<'a> WriteHandler for HttpReadHandlerRef<'a> {
    fn on_write(mut self, len: Result<usize, Errno>) {
        match len {
            Ok(len) => {
                trace!(target: "Hand", "buffer write: {:?}", std::str::from_utf8(&self.wrbuf[..len]).unwrap());
                self.written += len;
                if self.written == self.wrbuf.len() {
                    self.written = 0;
                    // NOTE: close the socket or keep-alive.
                    trace!(target: "Hand", "completed write");
                    // close(self.socket.fd);

                    let mut sock = self.socket.clone();
                    sock.async_read_some(self);
                }
            }
            Err(err) => {
                unimplemented!();
            }
        }
    }

    fn get_buf(&self) -> &[u8] {
        &self.wrbuf[..]
    }
}

pub trait ReadHandler {
    fn on_read(self, nread: Result<usize, Errno>);
    fn buf(&mut self) -> &mut [u8];
}

pub trait WriteHandler {
    fn on_write(self, nread: Result<usize, Errno>);
    fn get_buf(&self) -> &[u8];
}

mod io {
    use std::cell::UnsafeCell;
    use std::collections::VecDeque;

    use super::{OperationRef, DescriptorSetRef};

    pub struct Context<'a> {
        pub reactor: i32,
        pub queue: UnsafeCell<VecDeque<OperationRef<'a>>>,
        pub descriptors: DescriptorSetRef<'a>,
    }

    impl<'a> Context<'a> {
        pub fn new(reactor: i32) -> Context<'a> {
            Context {
                reactor: reactor,
                queue: UnsafeCell::new(VecDeque::new()),
                descriptors: DescriptorSetRef::new(),
            }
        }
    }
}

pub struct EventDataRef<'a> {
    pub events: event::EventFilter,
    pub read_handler: Option<Box<FnBox() -> () + 'a>>,
}

impl<'a> EventDataRef<'a> {
    pub fn new(read_handler: Option<Box<FnBox() -> () + 'a>>) -> EventDataRef<'a> {
        EventDataRef {
            events: event::EventFilter::EVFILT_SYSCOUNT,
            read_handler: read_handler,
        }
    }
}

pub struct DescriptorSetRef<'a>(UnsafeCell<VecMap<EventDataRef<'a>>>);

impl<'a> DescriptorSetRef<'a> {
    pub fn new() -> DescriptorSetRef<'a> {
        DescriptorSetRef(UnsafeCell::new(VecMap::with_capacity(1024)))
    }

    pub fn insert(&self, key: usize, val: EventDataRef<'a>) -> Option<EventDataRef<'a>> {
        unsafe {
            (*self.0.get()).insert(key, val)
        }
    }

    pub fn get_mut(&self, fd: &usize) -> Option<&mut EventDataRef<'a>> {
        unsafe {
            (*self.0.get()).get_mut(fd)
        }
    }
}

pub enum OperationRef<'a> {
    Poll,
    User(Box<FnBox() -> () + 'a>),
}

use io::Context as Context_;

use xyio::io::{Context, HandleWrite, HandleRead};

fn do_work(reactor: i32, pipefd: i32) {
    let context = Rc::new(Context_::new(reactor));
    // let context = Context::new(epollfd);

    let default: event::KEvent = unsafe { std::mem::uninitialized() };

    let mut events = [default; 1024];

    loop {
        // Process queue.
        let timeout = {
            let queue = unsafe { &mut *context.queue.get() };
            let mut size = queue.len();
            trace!(target: "Work", "processing {} operations", size);

            while size > 0 {
                let operation = queue.pop_front().unwrap();

                match operation {
                    OperationRef::User(operation) => {
                        trace!(target: "Work", "user op");
                        operation()
                    },
                    OperationRef::Poll => {
                        trace!(target: "Work", "poll op");
                        // poll = true;
                    }
                }

                size -= 1;
            }

            if queue.is_empty() {
                60000
            } else {
                0
            }
        };

        // if !poll {
        //     continue;
        // }

        trace!(target: "Work", "kevent");
        match event::kevent(reactor, &[], &mut events, timeout) {
            Ok(0) => {
                trace!(target: "Work", "kqueue tick: timeout");
            }
            Ok(size) => {
                trace!(target: "Work", "kqueue tick, size: {}", size);

                for event in &events[..size] {
                    if event.ident == pipefd as u64 {
                        trace!(target: "Work", "control event: new connection");

                        let mut buf = [0u8; 4];
                        read(pipefd, &mut buf[..])
                            .ok().expect("failed to read from the control channel");
                        let fd: i32 = unsafe { std::mem::transmute(buf) };

                        trace!(target: "Work", "scheduled new connection, fd: {}", fd);

                        // Process connection; read request; write response.
                        // Register socket on all operations except write.
                        let ev = event::KEvent {
                            ident: fd as u64,
                            filter: event::EventFilter::EVFILT_READ,
                            flags: event::EV_ADD | event::EV_CLEAR,
                            fflags: event::FilterFlag::empty(),
                            data: 0,
                            udata: 0,
                        };
                        event::kevent(reactor, &[ev], &mut[], 0).unwrap();

                        let mut evd = EventDataRef::new(None);
                        evd.events = ev.filter;
                        context.descriptors.insert(fd as usize, evd);

                        let mut socket = SocketRef::new(fd, context.clone());
                        let handler = HttpReadHandlerRef::new(socket.clone());
                        socket.async_read_some(handler);
                    } else {
                        let fd = event.ident as usize;
                        trace!(target: "Work", "processing event, fd: {}, events: {:?}", fd, event.filter);

                        {
                            let mut ev = EventDataRef::new(None);
                            ev.events = event.filter; //TODO: I can cache.

                            let ev = context.descriptors.insert(fd, ev).unwrap();
                            // TODO: 1. Out of band events.
                            // TODO: 2. Write events.
                            // Read events.
                            if let Some(handler) = ev.read_handler {
                                //&& event.filter == event::EventFilter::EVFILT_READ - assert.
                                handler();
                            }
                        }
                    }
                }
            }
            Err(..) => break,
        }
    }
}

fn main() {
    // Parse command-line arguments.
    let args: Vec<String> = std::env::args().collect();

    let mut opts = Options::new();
    opts.optflagmulti("v", "", "verbose mode");

    let matches = match opts.parse(&args[1..]) {
        Ok(v) => v,
        Err(err) => {
            println!("unable to parse command-line arguments: {}", err);
            std::process::exit(1);
        }
    };

    logging::from_usize(matches.opt_count("v"))
        .map(logging::init)
        .unwrap().ok().expect("unable to initialize logging system");

    // TODO: Investigate how to properly close the fd (using scoped API).
    let kqueue = event::kqueue().ok().expect("unable to initialize kqueue");

    let (rdpipe, wrpipe) = pipe2(fcntl::O_NONBLOCK | fcntl::O_CLOEXEC)
        .ok()
        .expect("unable to create control channel");

    let event = event::KEvent {
        ident: rdpipe as u64,
        filter: event::EventFilter::EVFILT_READ,
        flags: event::EV_ADD | event::EV_CLEAR,
        fflags: event::FilterFlag::empty(),
        data: 0,
        udata: 0,
    };

    event::kevent(kqueue, &[event], &mut[], 0)
        .ok()
        .expect("unable to register control channel in epoll");

    // Initialize worker threads.
    let thread = thread::Builder::new().name("W".to_string()).spawn(move || {
        debug!(target: "Work", "worker thread has been started");
        do_work(kqueue, rdpipe);

        debug!(target: "Work", "worker thread has been stopped");
    }).ok().expect("unable to spawn worker thread");

    let listener = TcpListener::bind(("::", 8080)).unwrap();
    let fd = listener.as_raw_fd();

    info!(target: "Main", "ready to serve");
    debug!(target: "Main", "listening on fd {}", fd);

    loop {
        match socket::accept4(fd, socket::SOCK_NONBLOCK | socket::SOCK_CLOEXEC) {
            Ok(fd) => {
                debug!(target: "Main", "accepted new connection on fd {}", fd);

                // Schedule client.
                let buf: [u8; 4] = unsafe { std::mem::transmute(fd) };
                write(wrpipe, &buf[..])
                    .ok().expect("failed to write to the control channel");
            }
            Err(err) => {
                error!(target: "Main", "failed to accept: {:?}", err);
                break;
            }
        }
    }

    thread.join().unwrap();

    close(wrpipe).unwrap();
    close(rdpipe).unwrap();

    close(kqueue).unwrap();
}
