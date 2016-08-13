#![feature(fnbox)]
#![feature(box_syntax)]
#![feature(question_mark)]
#![feature(unboxed_closures)]
#![feature(stmt_expr_attributes)]

#[macro_use] extern crate log;
#[macro_use] extern crate bitflags;
#[macro_use] extern crate clap;
extern crate httparse;
extern crate libc;
extern crate nix;
extern crate xyio;

use std::boxed::FnBox;
use std::io::{Error, ErrorKind};
use std::net::TcpListener;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::ptr;
use std::thread;
use std::rc::Rc;
use std::collections::{VecDeque, HashMap};

use libc::{c_int};

use clap::{App, Arg};

use nix::errno::Errno;
use nix::fcntl;
use nix::sys::{epoll, socket};
// use nix::sys::uio::IoVec;
use nix::unistd::{close, pipe2, read, write};

use xyio::logging;

mod ffi {

use libc::{self, c_int, sockaddr, socklen_t};

#[repr(C)]
pub struct IoVec {
    iov_base: *mut libc::c_void,
    iov_len: libc::c_int,
}

extern {
    // Linux >= 2.6.27.
    pub fn epoll_create1(flags: c_int) -> c_int;

    // Linux >= 2.6.28.
    pub fn accept4(sockfd: c_int, addr: *mut sockaddr, addrlen: *mut socklen_t, flags: c_int) -> c_int;
}

} // mod ffi

bitflags! {
    flags EPollFlags: i32 {
        const EPOLL_CLOEXEC = 0x80000,
    }
}

fn epoll_create(flags: EPollFlags) -> Result<i32, Errno> {
    let fd = unsafe {
        ffi::epoll_create1(flags.bits())
    };

    if fd >= 0 {
        Ok(fd)
    } else {
        Err(Errno::last())
    }
}

fn accept4(sockfd: c_int, flags: c_int) -> Result<i32, Errno> {
    let fd = unsafe {
        ffi::accept4(sockfd, ptr::null_mut(), ptr::null_mut(), flags)
    };

    if fd >= 0 {
        Ok(fd)
    } else {
        Err(Errno::last())
    }
}

fn sendmsg(fd: RawFd, iov: &[&[u8]], flags: socket::MsgFlags) -> Result<usize, Error> {
    let mhdr = libc::msghdr {
        msg_name: 0 as *mut libc::c_void,
        msg_namelen: 0,
        msg_iov: iov.as_ptr() as *mut ffi::IoVec as *mut libc::iovec,
        msg_iovlen: iov.len(),
        msg_control: 0 as *mut libc::c_void,
        msg_controllen: 0,
        msg_flags: 0,
    };

    let rc = unsafe {
        libc::sendmsg(fd, &mhdr, flags.bits())
    };

    if rc < 0 {
        Err(Error::last_os_error())
    } else {
        Ok(rc as usize)
    }
}

pub struct EventData {
    pub events: epoll::EpollEventKind,
    pub handle_read: Option<Box<FnBox(&mut Context)>>,
    pub handle_write: Option<Box<FnBox(&mut Context)>>,
}

pub trait HandleRead {
    // type Item: Send + 'static;
    // type Error: Send + 'static;
    type Stream: StreamRead + 'static;

    fn complete(self: Box<Self>, result: Result<usize, Error>, ctx: &mut Context, rd: Self::Stream);
    /// Returns a reference to a read buffer where all byte will be read into while reading from
    /// the stream.
    fn buf(&mut self) -> &mut [u8];
}

pub trait HandleWrite {
    type Stream: StreamWrite;

    fn complete(self: Box<Self>, nw: Result<usize, Error>, ctx: &mut Context, wr: Self::Stream);
    fn with_iov<F: Fn(&[&[u8]]) -> Result<usize, Error>>(&self, f: F) -> Result<usize, Error>;
}

pub enum Operation {
    Poll,
    User(Box<FnBox(&mut Context)>),
}

// TODO: public elimination.
pub struct Context {
    pub reactor: RawFd,

    pub fdmap: HashMap<RawFd, EventData>,
    pub queue: VecDeque<Operation>,
}

impl Context {
    ///
    pub fn new() -> Result<Context, Error> {
        let fd = epoll_create(EPOLL_CLOEXEC)?;

        let ctx = Context {
            reactor: fd,
            fdmap: HashMap::new(),
            queue: VecDeque::new(),
        };

        Ok(ctx)
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        if let Err(err) = close(self.reactor) {
            error!("failed to close reactor fd: {:?}", err);
        }
    }
}

impl FromRawFd for Context {
    unsafe fn from_raw_fd(fd: RawFd) -> Self {
        Context {
            reactor: fd,
            fdmap: HashMap::new(),
            queue: VecDeque::new(),
        }
    }
}

struct FileDesc {
    fd: RawFd,
}

impl Drop for FileDesc {
    fn drop(&mut self) {
        trace!("closing fd {}", self.fd);

        // Note that errors aren't handled when closing a file descriptor. The reason for this is
        // that if an error occurs we don't actually know if the file descriptor was closed or not,
        // and if we retried (for something like EINTR), we might close another valid file
        // descriptor opened after we closed ours.
        // Also note that this syscall may block, for example when there are unflushed buffers.
        if let Err(err) = close(self.fd) {
            error!("failed to close file descriptor: {:?}", err);
        }
    }
}

impl FromRawFd for FileDesc {
    unsafe fn from_raw_fd(fd: RawFd) -> Self {
        FileDesc {
            fd: fd,
        }
    }
}

impl AsRawFd for FileDesc {
    fn as_raw_fd(&self) -> RawFd {
        self.fd
    }
}

struct TcpSocket {
    fd: FileDesc,
}

impl TcpSocket {
    fn into_pair(self) -> (TcpSocketReader, TcpSocketWriter) {
        let fd = Rc::new(self.fd);

        let rd = TcpSocketReader {
            fd: fd.clone(),
        };

        let wr = TcpSocketWriter {
            fd: fd,
        };

        (rd, wr)
    }

    unsafe fn from_raw_fd(fd: RawFd, context: &mut Context) -> Result<TcpSocket, Error> {
        let ev = epoll::EpollEvent {
            events: epoll::EPOLLIN | epoll::EPOLLOUT | epoll::EPOLLET,
            data: fd as u64,
        };
        epoll::epoll_ctl(context.reactor, epoll::EpollOp::EpollCtlAdd, fd, &ev)?;

        let evd = EventData {
            events: ev.events,
            handle_read: None,
            handle_write: None,
        };
        context.fdmap.insert(fd, evd);

        let fd = FileDesc {
            fd: fd,
        };

        let sock = TcpSocket {
            fd: fd,
        };

        Ok(sock)
    }
}

impl StreamRead for TcpSocket {}
impl StreamWrite for TcpSocket {}

impl AsRawFd for TcpSocket {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

pub trait StreamRead: Sized + AsRawFd {
    fn async_read<H>(self, mut handler: Box<H>, context: &mut Context)
        where Self: 'static,
              H: HandleRead<Stream=Self> + 'static
    {
        trace!("receiving bytes");
        match socket::recv(self.as_raw_fd(), handler.buf(), socket::MsgFlags::empty()) {
            Ok(0) => {
                //TODO: poll if handler.buf().len() == 0.
                trace!("read EOF");
                context.queue.push_back(Operation::User(box move |context: &mut Context| {
                    handler.complete(Ok(0), context, self)
                }))
            }
            Ok(nread) => {
                trace!("read {} bytes", nread);
                context.queue.push_back(Operation::User(box move |context: &mut Context| {
                    handler.complete(Ok(nread), context, self)
                }))
            }
            Err(nix::Error::Sys(err)) if err == nix::errno::EWOULDBLOCK => {
                trace!("EWOULDBLOCK");

                // Register callback in the fdmap.
                let ev = context.fdmap.get_mut(&self.as_raw_fd()).unwrap();
                ev.handle_read = Some(box move |context: &mut Context| {
                    self.async_read(handler, context)
                });

                // Add epoll_wait task to the queue.
                context.queue.push_back(Operation::Poll);
            }
            // TODO: EAGAIN, EINTR
            Err(err) => {
                error!("failed to read: {:?}", err);
                context.queue.push_back(Operation::User(box move |context: &mut Context| {
                    handler.complete(Err(err.into()), context, self)
                }))
            }
        }
    }
}

pub trait StreamWrite: Sized + AsRawFd {
    fn async_write<H>(self, handler: Box<H>, context: &mut Context)
        where Self: 'static,
              H: HandleWrite<Stream=Self> + 'static
    {
        // trace!("sending {:?}", ::std::str::from_utf8(handler.buf()).unwrap());

        match handler.with_iov(|iov| sendmsg(self.as_raw_fd(), iov, socket::MsgFlags::empty())) {
        // match socket::send(self.as_raw_fd(), handler.buf(), socket::MsgFlags::empty()) {
            Ok(len) => {
                trace!("written {} bytes", len);
                context.queue.push_back(Operation::User(box move |context: &mut Context| {
                    handler.complete(Ok(len), context, self)
                }))
            }
            Err(ref err) if err.kind() == ErrorKind::WouldBlock => {
            // Err(nix::Error::Sys(err)) if err == nix::errno::EWOULDBLOCK => {
                trace!("EWOULDBLOCK");

                let ev = context.fdmap.get_mut(&self.as_raw_fd()).unwrap();
                ev.handle_write = Some(box move |context: &mut Context| {
                    self.async_write(handler, context)
                });

                // Add epoll_wait task to the queue.
                context.queue.push_back(Operation::Poll);
            }
            // TODO: EINTR.
            Err(err) => {
                error!("failed to send bytes: {:?}", err);
                context.queue.push_back(Operation::User(box move |context: &mut Context| {
                    handler.complete(Err(err.into()), context, self)
                }))
            }
        }
    }
}

pub struct TcpSocketReader {
    fd: Rc<FileDesc>,
}

impl StreamRead for TcpSocketReader {}

impl AsRawFd for TcpSocketReader {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

// Big difference between shutdown and close on a socket is the behavior when the socket is shared
// by other processes. A shutdown() affects all copies of the socket while close() affects only the
// file descriptor in one process.
// To force sending RST we can set SO_LINGER option before closing.
pub struct TcpSocketWriter {
    fd: Rc<FileDesc>,
}

impl StreamWrite for TcpSocketWriter {}

impl AsRawFd for TcpSocketWriter {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

const CODE_OK: &'static str = "HTTP/1.1 200 OK\r\n";

// struct Line(Cow<'static, str>);
//
// trait Header {
//     fn name(&self) -> Line;
//     fn format_value(&self, wr: &mut Write) -> Result<(), Error>;
// }
//
// struct ContentLength(u64);
//
// impl Header for ContentLength {
//     fn name(&self) -> Line {
//         Line::from("Content-Length")
//     }
//
//     fn format_value<W: ?Sized + Write>(&self, wr: &mut W) -> Result<(), Error> {
//         write!(wr, "{}", self.0)
//     }
// }

// enum WellKnownHeader {
//     ContentLength(ContentLength),
// }

// enum HeaderItem {
//     WellKnown(WellKnownHeader),
//     Custom(Box<Header>),
//
//     /// Header with custom name and value, represented as strings. Should end with \r\n.
//     ///
//     /// Think "X-Trace-Id-Y: 42\r\n".
//     ///
//     /// This is the fastest possible header representation, because there is no runtime formatting
//     /// and the result buffer will contain only a single slice.
//     Precompiled(Line),
//     PrecompiledWithValue(Line, Line),
// }

// Vec<Line> - headers?
// for head in headers {
//   match head {
//     WellKnown(h) => {
//

// trait ResponseState {}
//
// struct Prelude;
// struct Streaming;
//
// struct Response<S: ResponseState> {
//     _state: PhantomData<S>,
// }

/// As we don't support pipelining there can be exactly one Response object in a Connection at a
/// time which is cleaned up before each writing stage without memory deallocation.
struct Response {
    /// One of the preallocated HTTP statuses.
    code: &'static str,
    headers: Vec<&'static [u8]>,
    // header_values_buf: Vec<u8>,
    // header_values_pointers: Vec<(usize, usize)>,
    body: Vec<u8>, // TODO: Netbuf,
    size: usize,
    nwritten: usize,
}

impl Response {
    fn new() -> Response {
        Response {
            code: CODE_OK,
            headers: Vec::with_capacity(64),
            body: Vec::with_capacity(4096),
            size: 0,
            nwritten: 0,
        }
    }

    fn reset(&mut self) {
        self.code = CODE_OK;
        unsafe { self.headers.set_len(0); }
        unsafe { self.body.set_len(0); }
        self.size = 0;
        self.nwritten = 0;
    }

    fn set_status(&mut self, status: &'static str) {
        self.code = status;
        self.size += status.as_bytes().len();
    }

    fn add_header(&mut self, header: &'static str) {
        self.headers.push(header.as_bytes());
        self.size += header.as_bytes().len();
    }

    fn write(&mut self, data: &[u8]) {
        self.body.extend_from_slice(data);
        self.size += data.len();
    }

    fn with<R, F: Fn(&[&[u8]]) -> R>(&self, f: F) -> R {
        let default: &[u8] = unsafe { std::mem::uninitialized() };
        let mut buf = [default; 1024];

        let mut id = 0;
        let mut offset = self.nwritten;

        if offset < self.code.as_bytes().len() {
            buf[id] = &self.code.as_bytes()[offset..];
            id += 1;
        } else {
            offset -= self.code.as_bytes().len();
        }

        for header in &self.headers {
            if offset < header.len() {
                buf[id] = &header[offset..];
                id += 1;
            } else {
                offset -= header.len();
            }
        }

        if offset < self.body.len() {
            buf[id] = &self.body[offset..];
            id += 1;
        }

        f(&buf[..id])
    }
}

// TODO: construct response dynamically.
// TODO: apply filter.

// static: "HTTP/1.1 200 OK\r\n"
//         "Server: xyio/0.1.0\r\n"
//         "Content-Length: "
// dyn:    "15\r\n", but can be preallocated [u8; 64].
// static: "Connection: Keep-Alive\r\n"
//         "X-Powered-By: Cocaine\r\n"
//         "\r\n"
// dyn:    "Ты пидор", netbuf.

// TODO: Build the response.
//       Then replace send with sendmsg.
//       Measure.
/// HTTP protocol encoding/decoding and handling.

// >>>>> Parse request, extract headers: X-JSON-RPC-Y.
// >>>>> Parse body chunked.
// >>>>> Then get/create TCP socket to the Cocaine.
// >>>>> Request.
// >>>>> Response.
// >>>>> Write status, headers, body(chunked?).

struct Request;

struct HttpConnection<D> {
    /// Read buffer for status line, headers and body.
    ///
    /// A request line cannot exceed the size of the buffer, or the 414 (URI Too Long)
    /// error is returned to the client.
    /// A request header field cannot exceed the size of the buffer as well, or the 400 error is
    /// returned to the client.
    /// Buffer is allocated only on demand after TCP connection establishment.
    /// By default, the buffer size is equal to 4K bytes, which equals page size on most systems.
    /// If after the end of request processing a connection is transitioned into the keep-alive
    /// state, these buffer is not released.
    rdbuf: Vec<u8>,
    /// Left position of the buffer from where reading should be continued while reading HTTP status
    /// line with headers.
    /// Should be reset on state switch, i.e where there is transition between reading headers and
    /// body.
    nread: usize,

    keep_alive: bool,

    response: Response,

    /// Request dispatcher.
    dispatch: Option<D>,
}

impl<D: FnMut(&Request) + 'static> HttpConnection<D> {
    fn new(dispatch: D) -> Self {
        HttpConnection {
            rdbuf: [0u8; 4096].to_vec(),
            nread: 0,
            keep_alive: true,
            response: Response::new(),
            dispatch: Some(dispatch),
        }
    }
}

impl<F: FnMut(&Request) + 'static> HandleRead for HttpConnection<F> {
    type Stream = TcpSocket;

    fn complete(mut self: Box<Self>, nread: Result<usize, Error>, context: &mut Context, sock: TcpSocket) {
        match nread {
            Ok(0) => {
                trace!("EOF");
            }
            Ok(nread) => {
                trace!("buffer read: {:?}", ::std::str::from_utf8(&self.rdbuf[..nread]).unwrap());

                let mut should_keep_alive = true;
                let mut dispatch = std::mem::replace(&mut self.dispatch, None).unwrap();

                let complete = {
                    let mut headers = [httparse::EMPTY_HEADER; 64];
                    let mut request = httparse::Request::new(&mut headers);

                    match request.parse(&self.rdbuf[..nread]) {
                        Ok(status) => {
                            for header in &request.headers[..] {
                                if let &httparse::Header { name: "Connection", value: b"Close" } = header {
                                    should_keep_alive = false;
                                    break;
                                }
                            }

                            (dispatch)(&Request);

                            status.is_complete()
                        }
                        Err(err) => {
                            error!("failed to parse HTTP request: {:?}", err);
                            // TODO: Write 400 and close connection.
                            return;
                        }
                    }
                };

                std::mem::replace(&mut self.dispatch, Some(dispatch));

                // TODO: We should close the connection on any 4xx or 5xx.
                if complete {
                    trace!("complete");

                    self.nread = 0;

                    self.response.reset();
                    self.response.set_status(CODE_OK);
                    // TODO: Date automatically.
                    // TODO: Content-Length automatically.
                    // TODO: Transfer-Encoding automatically.
                    // TODO: Connection automatically.
                    self.response.add_header("Server: xyio/0.1.0\r\n");
                    self.response.add_header("Content-Length: 15\r\n");
                    if should_keep_alive {
                        self.response.add_header("Connection: Keep-Alive\r\n");
                    } else {
                        self.keep_alive = false;
                        self.response.add_header("Connection: Close\r\n");
                    }
                    self.response.add_header("X-Powered-By: Cocaine\r\n");
                    self.response.write(b"\r\n");
                    self.response.write("Ты пидор".as_bytes());

                    sock.async_write(self, context);
                } else {
                    sock.async_read(self, context);
                }
            }
            Err(err) => {
                error!("failed to read HTTP stream: {:?}", err);
            }
        }
    }

    fn buf(&mut self) -> &mut [u8] {
        &mut self.rdbuf[self.nread..]
    }
}

impl<F: FnMut(&Request) + 'static> HandleWrite for HttpConnection<F> {
    type Stream = TcpSocket;

    fn complete(mut self: Box<Self>, result: Result<usize, Error>, context: &mut Context, sock: TcpSocket) {
        match result {
            Ok(len) => {
                // trace!("buffer write: {:?}", std::str::from_utf8(&self.wrbuf[..len]).unwrap());
                self.response.nwritten += len;
                if self.response.nwritten == self.response.size {
                    trace!("completed write");

                    if self.keep_alive {
                        sock.async_read(self, context);
                    } else {
                        trace!("Connection: close");
                    }
                } else {
                    sock.async_write(self, context)
                }
            }
            Err(err) => {
                error!("failed to write into HTTP stream: {:?}", err);
            }
        }
    }

    fn with_iov<U: Fn(&[&[u8]]) -> Result<usize, Error>>(&self, f: U) -> Result<usize, Error> {
        self.response.with(f)
    }
}

fn run(reactor: i32, rd: i32) {
    let mut context = unsafe {
        Context::from_raw_fd(reactor)
    };

    let default: epoll::EpollEvent = unsafe {
        std::mem::uninitialized()
    };

    let mut events = [default; 1024];

    loop {
        // Process queue.
        let timeout = {
            let mut size = context.queue.len();
            trace!("processing {} operations", size);

            while size > 0 {
                match context.queue.pop_front() {
                    Some(Operation::User(op)) => {
                        trace!("user op");
                        op.call_box((&mut context,))
                    }
                    Some(Operation::Poll) => {
                        trace!("poll op");
                    }
                    None => {
                        error!("operation queue has been unexpectedly exhausted");
                        break;
                    }
                }

                size -= 1;
            }

            // It is possible that a user puts new pending operations while processing the previous
            // ones. In that case we should wake up immediately instead of sleeping.
            if context.queue.is_empty() {
                60000
            } else {
                0
            }
        };

        trace!("epoll_wait ...");
        match epoll::epoll_wait(reactor, &mut events, timeout) {
            Ok(0) => {
                trace!("epoll tick: timeout");
            }
            Ok(size) => {
                trace!("epoll tick, size: {}", size);

                for event in &events[..size] {
                    if event.data == rd as u64 {
                        trace!("control event: new connection");

                        let mut buf = [0u8; 4];
                        read(rd, &mut buf[..])
                            .expect("failed to read from the control channel");
                        let fd: i32 = unsafe { std::mem::transmute(buf) };

                        trace!("scheduled new connection, fd: {}", fd);

                        let sock = unsafe {
                            TcpSocket::from_raw_fd(fd, &mut context).unwrap()
                        };
                        sock.async_read(box HttpConnection::new(|rq| {}), &mut context);
                    } else {
                        let fd = event.data as i32;
                        trace!("processing event, fd: {}, events: {:?}", fd, event.events);

                        let mut ev = context.fdmap.remove(&fd).unwrap();

                        // TODO: 1. Out of band events.
                        if event.events.contains(epoll::EPOLLOUT) {
                            if let Some(callback) = std::mem::replace(&mut ev.handle_write, None) {
                                callback.call_box((&mut context,));
                            }
                        }

                        if event.events.contains(epoll::EPOLLIN) {
                            if let Some(callback) = std::mem::replace(&mut ev.handle_read, None) {
                                callback.call_box((&mut context,));
                            }
                        }

                        context.fdmap.insert(fd, ev);
                    }
                }
            }
            Err(..) => break,
        }
    }
}

// pub struct PipeRead {
//     fd: FileDesc,
// }
//
// impl FromRawFd for PipeRead {
//     unsafe fn from_raw_fd(fd: RawFd) -> PipeRead {
//         PipeRead {
//             fd: FileDesc::from_raw_fd(fd),
//         }
//     }
// }
//
// pub struct PipeWrite {
//     fd: FileDesc,
// }
//
// impl FromRawFd for PipeWrite {
//     unsafe fn from_raw_fd(fd: RawFd) -> PipeWrite {
//         PipeWrite {
//             fd: FileDesc::from_raw_fd(fd),
//         }
//     }
// }
//
// fn pipe_pair() -> Result<(PipeWrite, PipeRead), Error> {
//     let (rd, wr) = pipe2(fcntl::O_NONBLOCK | fcntl::O_CLOEXEC)?;
//
//     let rd = unsafe { PipeRead::from_raw_fd(rd) };
//     let wr = unsafe { PipeWrite::from_raw_fd(wr) };
//
//     Ok((wr, rd))
// }

fn start(nthreads: usize) {
    let mut workers = Vec::with_capacity(nthreads);
    for tid in 0..nthreads {
        let epollfd = epoll_create(EPOLL_CLOEXEC)
            .expect("failed to initialize epollfd");

        let (rd, wr) = pipe2(fcntl::O_NONBLOCK | fcntl::O_CLOEXEC)
            .expect("failed to create the control channel");

        let event = epoll::EpollEvent {
            events: epoll::EPOLLIN,
            data: rd as u64,
        };

        epoll::epoll_ctl(epollfd, epoll::EpollOp::EpollCtlAdd, rd, &event)
            .expect("unable to register control channel in the reactor");

        let thread = thread::Builder::new().name(format!("work#{:02}", tid)).spawn(move || {
            debug!("worker thread has been started");
            run(epollfd, rd);
            debug!("worker thread has been stopped");
        }).expect("failed to spawn worker thread");

        workers.push((thread, wr, rd));
    }

    // Listen.
    let listener = TcpListener::bind(("::", 8080)).unwrap();
    let fd = listener.as_raw_fd();

    info!("ready to serve");
    debug!("listening on fd {}", fd);

    let mut sched = workers.len() - 1;
    loop {
        match accept4(fd, (socket::SOCK_NONBLOCK | socket::SOCK_CLOEXEC).bits()) {
            Ok(fd) => {
                debug!("accepted new connection on fd {}", fd);

                // Schedule the client.
                let buf: [u8; 4] = unsafe { std::mem::transmute(fd) };

                sched = (sched + 1) % workers.len();
                write(workers[sched].1, &buf[..])
                    .expect("failed to write to the control channel");

                debug!("scheduled into {} worker", sched);
            }
            Err(err) => {
                error!("failed to accept: {:?}", err);
                break;
            }
        }
    }

    for (thread, wr, rd) in workers.drain(..) {
        close(rd).unwrap();
        close(wr).unwrap();

        thread.join().unwrap();
    }
}

fn main() {
    let matches = App::new("Proof of concept of asynchronous HTTP 1.1 server")
        .author(crate_authors!())
        .version(crate_version!())
        .arg(Arg::with_name("threads")
           .short("t")
           .long("threads")
           .value_name("THREADS")
           .help("number of worker threads")
           .takes_value(true))
        .arg(Arg::with_name("v")
           .short("v")
           .multiple(true)
           .help("verbosity level"))
        .get_matches();

    logging::init(logging::from_usize(matches.occurrences_of("v") as usize))
        .expect("failed to initialize logging system");

    let nthreads = matches.value_of("threads")
        .map_or(Ok(4), |v| v.parse())
        .expect("failed to parse \"number of worker threads\" argument");

    start(nthreads);
}
