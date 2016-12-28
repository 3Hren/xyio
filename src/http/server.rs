use std;
use std::boxed::FnBox;
use std::cell::RefCell;
use std::collections::{VecDeque, HashMap};
use std::io::{Cursor, Error, ErrorKind};
use std::io::Write;
use std::net::TcpListener;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::rc::Rc;
use std::str;
use std::thread;

use http_muncher::{Parser, ParserHandler};

use nix::fcntl::{self, O_NONBLOCK, O_CLOEXEC};
use nix::sys::{epoll, socket};
use nix::unistd::{close, pipe2, read, write};

use io::FileDesc;
use sys::{epoll_create1, accept4, sendmsg, EPOLL_CLOEXEC};

const PIPELINE_NUMBER: usize = 64;

pub type UserOperation = Box<FnBox()>;

pub enum Operation {
    Poll,
    User(UserOperation),
}

pub struct EventData {
    pub events: epoll::EpollEventKind,
    pub handle_read: Option<UserOperation>,
    pub handle_write: Option<UserOperation>,
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
        let fd = epoll_create1(EPOLL_CLOEXEC)?;

        let ctx = Context {
            reactor: fd,
            fdmap: HashMap::new(),
            queue: VecDeque::new(),
        };

        Ok(ctx)
    }

    pub fn post<F: FnOnce() + 'static>(&mut self, f: F) {
        self.queue.push_back(Operation::User(box move || {
            f()
        }));
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

type SharedContext = Rc<RefCell<Context>>;

struct TcpSocket {
    fd: FileDesc,
    ctx: SharedContext,
}

impl TcpSocket {
    unsafe fn from_raw_fd(fd: RawFd, ctx: &SharedContext) -> Result<TcpSocket, Error> {
        let ev = epoll::EpollEvent {
            events: epoll::EPOLLIN | epoll::EPOLLOUT | epoll::EPOLLET,
            data: fd as u64,
        };
        epoll::epoll_ctl(ctx.borrow_mut().reactor, epoll::EpollOp::EpollCtlAdd, fd, &ev)?;

        let evd = EventData {
            events: ev.events,
            handle_read: None,
            handle_write: None,
        };
        ctx.borrow_mut().fdmap.insert(fd, evd);

        let fd = unsafe { FileDesc::from_raw_fd(fd) };

        let sock = TcpSocket {
            fd: fd,
            ctx: ctx.clone(),
        };

        Ok(sock)
    }

    fn into_pair(self) -> (TcpSocketReader, TcpSocketWriter) {
        let fd = Rc::new(self.fd);

        let rd = TcpSocketReader {
            fd: fd.clone(),
            ctx: self.ctx.clone(),
        };

        let wr = TcpSocketWriter {
            fd: fd,
            ctx: self.ctx,
        };

        (rd, wr)
    }
}

impl StreamRead for TcpSocket {}
impl StreamWrite for TcpSocket {}

impl AsRawFd for TcpSocket {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

// struct RingBuf { // TODO: Not so ring.
//     // [pppppxxxxxxxxxxxxxx000000]
//     //       ^             ^
//     //       consumed      |
//     //                     position
//     // AsMut<[u8]> -> &mut [consumed..position]
//     data: Vec<u8>,
// }

pub trait PacketRecv {
    fn recv(&mut self, fd: RawFd, flags: socket::MsgFlags) -> Result<usize, Error>;
}

impl<T: AsMut<[u8]>> PacketRecv for Cursor<T> {
    fn recv(&mut self, fd: RawFd, flags: socket::MsgFlags) -> Result<usize, Error> {
        let id = self.position() as usize;
        let mut buf = self.get_mut();
        socket::recv(fd, &mut buf.as_mut()[id..], flags).map_err(|_| Error::last_os_error())
    }
}

pub trait PacketRead {
    fn read(&mut self, fd: RawFd) -> Result<usize, Error>;
}

impl<T: AsMut<[u8]>> PacketRead for Cursor<T> {
    fn read(&mut self, fd: RawFd) -> Result<usize, Error> {
        let id = self.position() as usize;
        let mut buf = self.get_mut();
        read(fd, &mut buf.as_mut()[id..]).map_err(|_| Error::last_os_error())
    }
}

pub trait PacketSend {
    fn send(&self, fd: RawFd, flags: socket::MsgFlags) -> Result<usize, Error>;
}

impl PacketSend for Vec<u8> {
    fn send(&self, fd: RawFd, flags: socket::MsgFlags) -> Result<usize, Error> {
        socket::send(fd, &self[..], flags).map_err(|_| Error::last_os_error())
    }
}

pub trait Bound {
    fn context(&self) -> &SharedContext;
}

impl Bound for TcpSocket {
    fn context(&self) -> &SharedContext {
        &self.ctx
    }
}

impl Bound for TcpSocketReader {
    fn context(&self) -> &SharedContext {
        &self.ctx
    }
}

impl Bound for TcpSocketWriter {
    fn context(&self) -> &SharedContext {
        &self.ctx
    }
}

pub trait StreamRead: Sized + AsRawFd + Bound {
    fn async_recv<T, F>(self, buf: T, f: F)
        where Self: 'static,
              T: PacketRecv + 'static,
              F: FnOnce(Result<usize, Error>, T, Self) + 'static
    {
        let mut buf = buf;

        trace!("receiving bytes");
        let mut ctx = self.context().clone();
        match buf.recv(self.as_raw_fd(), socket::MsgFlags::empty()) {
            Ok(0) => {
                trace!("read EOF [fd: {}]", self.as_raw_fd());
                ctx.borrow_mut().queue.push_back(Operation::User(box move || {
                    f(Ok(0), buf, self)
                }))
            }
            Ok(nread) => {
                trace!("read {} bytes [fd: {}]", nread, self.as_raw_fd());
                ctx.borrow_mut().queue.push_back(Operation::User(box move || {
                    f(Ok(nread), buf, self)
                }))
            }
            Err(ref err) if err.kind() == ErrorKind::WouldBlock => {
                trace!("EWOULDBLOCK");

                // Register callback in the fdmap.
                {
                let mut c = ctx.borrow_mut();
                let ev = c.fdmap.get_mut(&self.as_raw_fd()).unwrap();
                ev.handle_read = Some(box move || {
                    self.async_recv(buf, f)
                });
                }

                // Add epoll_wait task to the queue.
                ctx.borrow_mut().queue.push_back(Operation::Poll);
            }
            // TODO: EAGAIN, EINTR
            Err(err) => {
                error!("failed to read: {:?}", err);
                ctx.borrow_mut().queue.push_back(Operation::User(box move || {
                    f(Err(err.into()), buf, self)
                }))
            }
        }
    }

    fn async_read<T, F>(self, buf: T, f: F)
        where Self: 'static,
              T: PacketRead + 'static,
              F: FnOnce(Result<usize, Error>, T, Self) + 'static
    {
        let mut buf = buf;

        trace!("receiving bytes");
        let mut ctx = self.context().clone();
        match buf.read(self.as_raw_fd()) {
            Ok(0) => {
                trace!("read EOF [fd: {}]", self.as_raw_fd());
                ctx.borrow_mut().queue.push_back(Operation::User(box move || {
                    f(Ok(0), buf, self)
                }))
            }
            Ok(nread) => {
                trace!("read {} bytes [fd: {}]", nread, self.as_raw_fd());
                ctx.borrow_mut().queue.push_back(Operation::User(box move || {
                    f(Ok(nread), buf, self)
                }))
            }
            Err(ref err) if err.kind() == ErrorKind::WouldBlock => {
                trace!("EWOULDBLOCK");

                // Register callback in the fdmap.
                {
                    let mut c = ctx.borrow_mut();
                    let ev = c.fdmap.get_mut(&self.as_raw_fd()).unwrap();
                    ev.handle_read = Some(box move || {
                        self.async_read(buf, f)
                    });
                }

                // Add epoll_wait task to the queue.
                ctx.borrow_mut().queue.push_back(Operation::Poll);
            }
            // TODO: EAGAIN, EINTR
            Err(err) => {
                error!("failed to read: {:?}", err);
                ctx.borrow_mut().queue.push_back(Operation::User(box move || {
                    f(Err(err.into()), buf, self)
                }))
            }
        }
    }
}

pub trait StreamWrite: Sized + AsRawFd + Bound {
    fn send<T: AsRef<[u8]>>(&mut self, buf: T) -> Result<usize, Error> {
        Ok(socket::send(self.as_raw_fd(), buf.as_ref(), socket::MsgFlags::empty())?)
    }

    fn async_send<T, F>(self, buf: T, f: F)
        where Self: 'static,
              T: PacketSend + 'static,
              F: FnOnce(Result<usize, Error>, T, Self) + 'static
    {
        // trace!("sending {:?}", ::std::str::from_utf8(handler.buf()).unwrap());

        let mut ctx = self.context().clone();
        match buf.send(self.as_raw_fd(), socket::MsgFlags::empty()) {
            Ok(len) => {
                trace!("written {} bytes", len);
                ctx.borrow_mut().queue.push_back(Operation::User(box move || {
                    f(Ok(len), buf, self)
                }));
            }
            Err(ref err) if err.kind() == ErrorKind::WouldBlock => {
                trace!("EWOULDBLOCK");

                {
                let mut c = ctx.borrow_mut();
                let ev = c.fdmap.get_mut(&self.as_raw_fd()).unwrap();
                ev.handle_write = Some(box move || {
                    self.async_send(buf, f)
                });
                }

                // Add epoll_wait task to the queue.
                ctx.borrow_mut().queue.push_back(Operation::Poll);
            }
            // TODO: EINTR.
            Err(err) => {
                error!("failed to send bytes: {:?}", err);
                ctx.borrow_mut().queue.push_back(Operation::User(box move || {
                    f(Err(err.into()), buf, self)
                }))
            }
        }
    }
}

pub struct TcpSocketReader {
    fd: Rc<FileDesc>,
    ctx: SharedContext,
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
    ctx: SharedContext,
}

impl StreamWrite for TcpSocketWriter {}

impl AsRawFd for TcpSocketWriter {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

/// HTTP protocol encoding/decoding and handling.

#[derive(Debug)]
pub struct Request {
    url: Vec<u8>,
    headers: Vec<u8>,
}

impl Request {
    pub fn new() -> Self {
        Request {
            url: Vec::new(),
            headers: Vec::new(),
        }
    }

    pub fn clear(&mut self) {
        unsafe {
            self.url.set_len(0);
            self.headers.set_len(0);
        }
    }
}

struct Response<D> {
    /// Request id.
    id: usize,
    /// Buffer for status line and headers (and body).
    buf: Vec<u8>,

    wr: Rc<RefCell<Writer<TcpSocketWriter>>>,
    state: Rc<RefCell<ConnectionState<D>>>,
    version: (u16, u16),
    keepalive: bool,
}

impl<D: Dispatch + 'static> Response<D> {
    pub fn new(id: usize, buf: Vec<u8>, wr: Rc<RefCell<Writer<TcpSocketWriter>>>, state: Rc<RefCell<ConnectionState<D>>>, version: (u16, u16), keepalive: bool) -> Self {
        Response {
            id: id,
            buf: buf,
            wr: wr,
            state: state,
            version: version,
            keepalive: keepalive,
        }
    }

    pub fn finish(mut self) {
        let body = "Ты пидор".as_bytes();

        let mut buf = std::mem::replace(&mut self.buf, Vec::new());
        write!(buf, "HTTP/{}.{} 200 OK\r\n", self.version.0, self.version.1);
        // TODO: Date automatically.
        // TODO: Transfer-Encoding automatically.
        buf.extend_from_slice(b"Server: xyio/0.1.0\r\n");
        write!(buf, "Content-Length: {}\r\n", body.len());
        if self.keepalive {
            buf.extend_from_slice(b"Connection: Keep-Alive\r\n");
        } else {
            buf.extend_from_slice(b"Connection: Close\r\n");
        }
        buf.extend_from_slice(b"X-Powered-By: Cocaine\r\n");
        buf.extend_from_slice(b"\r\n");
        buf.extend_from_slice(body);
        // TODO: Move `state` inside writer?
        let state = self.state.clone();
        trace!("buffer write: {:?}", str::from_utf8(&buf[..]).unwrap());
        self.wr.borrow_mut().write(self.id, buf, move |result, mut buf| {
            unsafe { buf.set_len(0); }
            state.borrow_mut().bufs.push(buf);
            state.borrow_mut().read_more();
        });
    }
}

trait Dispatch: Sized {
    fn on_request(&mut self, request: &Request, response: Response<Self>);
}

struct Handler<D> {
    request: Request,
    /// Request dispatcher.
    dispatch: D,

    state: Rc<RefCell<ConnectionState<D>>>,
    wr: Rc<RefCell<Writer<TcpSocketWriter>>>,
    buf: Option<Vec<u8>>,

    /// Total requests read in this connection.
    count: usize,
}

impl<D: Dispatch + 'static> Handler<D> {
    fn new(dispatch: D, state: Rc<RefCell<ConnectionState<D>>>, wr: Writer<TcpSocketWriter>) -> Self {
        Handler {
            request: Request::new(),
            dispatch: dispatch,
            state: state,
            wr: Rc::new(RefCell::new(wr)),
            buf: None,
            count: 0,
        }
    }
}

impl<D: Dispatch + 'static> ParserHandler for Handler<D> {
    fn on_url(&mut self, parser: &mut Parser, url: &[u8]) -> bool {
        self.request.url.extend_from_slice(url);
        true
    }

    fn on_headers_complete(&mut self, parser: &mut Parser) -> bool {
        let buf = std::mem::replace(&mut self.buf, None).unwrap();
        let response = Response::new(
            self.count % PIPELINE_NUMBER,
            buf,
            self.wr.clone(),
            self.state.clone(),
            parser.http_version(),
            parser.should_keep_alive()
        );
        self.dispatch.on_request(&self.request, response);
        self.request.clear();
        true
    }

    fn on_message_begin(&mut self, parser: &mut Parser) -> bool {
        trace!("message begin");
        self.buf = self.state.borrow_mut().bufs.pop();
        true
    }

    fn on_message_complete(&mut self, parser: &mut Parser) -> bool {
        trace!("message completed [count: {}, bufs: {}]", self.count, self.state.borrow_mut().bufs.len());
        self.count += 1;
        // Continue to parse only if there are free response buffers, i.e there are less than N
        // pipelined sessions active.
        if self.state.borrow_mut().bufs.len() == 0 {
            parser.pause();
        }

        true
    }
}

struct Reader<D> {
    /// Readable stream.
    rd: Option<TcpSocketReader>,

    /// Read buffer for status line, headers and body.
    ///
    /// A request line cannot exceed the size of the buffer, or the 414 (URI Too Long)
    /// error is returned to the client.
    /// A request header field cannot exceed the size of the buffer as well, or the 400 error is
    /// returned to the client.
    /// Buffer is allocated only on demand after TCP connection establishment.
    /// By default, the buffer size is equal to 4K bytes, which equals page size on most systems.
    /// If after the end of request processing a connection is transitioned into the keep-alive
    /// state, this buffer is not released.
    ///
    /// Cursor's position represents left position of the buffer from where reading should be
    /// continued while reading HTTP status line with headers.
    /// Should be reset on state switch, i.e where there is transition between reading headers and
    /// body.
    buf: Cursor<Vec<u8>>,

    state: Rc<RefCell<ConnectionState<D>>>,

    parser: Parser,
    handler: Option<Handler<D>>,
}

impl<D: Dispatch + 'static> Reader<D> {
    pub fn new(rd: TcpSocketReader, state: Rc<RefCell<ConnectionState<D>>>, handler: Handler<D>) -> Self {
        Reader {
            rd: Some(rd),
            buf: Cursor::new(vec![0; 4096]),
            state: state,
            parser: Parser::request(),
            handler: Some(handler),
        }
    }

    fn read_more(mut self) {
        let mut rd = std::mem::replace(&mut self.rd, None).unwrap();
        let buf = std::mem::replace(&mut self.buf, Cursor::new(Vec::new()));
        debug!("switched reader state to `reading`");
        rd.async_recv(buf, move |nread, buf, sock| {
            self.on_recv(nread, buf, sock)
        });
    }

    fn on_recv(mut self, nread: Result<usize, Error>, buf: Cursor<Vec<u8>>, sock: TcpSocketReader) {
        match nread {
            Ok(0) => {
                trace!("EOF");
            }
            Ok(nread) => {
                let id = buf.position() as usize;
                trace!("buffer read: {:?}", str::from_utf8(&buf.get_ref()[..id + nread]).unwrap());

                self.parser.unpause();
                let mut handler = std::mem::replace(&mut self.handler, None).unwrap();
                let nparsed = self.parser.parse(&mut handler, &buf.get_ref()[..id + nread]);
                trace!("parsed {} bytes [fd: {}]", nparsed, sock.as_raw_fd());

                if self.state.borrow_mut().bufs.len() == 0 {
                    let mut buf = buf;
                    buf.set_position((id + nread - nparsed) as u64);
                    self.rd = Some(sock);
                    self.buf = buf;
                    self.handler = Some(handler);
                    let state = self.state.clone();
                    debug!("switched reader state to `idle` [fd: {}]", self.rd.as_ref().unwrap().as_raw_fd());
                    state.borrow_mut().rd = Some(self);
                    return;
                }

                if self.parser.has_error() {
                    error!("failed to parse HTTP request: {:?}", self.parser.error());
                    return;
                }

                // TODO: Should close the connection on any 4xx or 5xx?
                if nparsed != id + nread {
                    error!("failed to parse HTTP request: {:?}", self.parser.error());
                    // TODO: Write 400 and close connection.
                    return;
                }

                self.handler = Some(handler);
                sock.async_recv(buf, move |nread, buf, sock| {
                    self.on_recv(nread, buf, sock)
                })
            }
            Err(err) => {
                error!("failed to read HTTP stream: {:?}", err);
            }
        }
    }
}

enum WriterState<W> {
    /// Stream is idle.
    Idle(W),
    Flushing,
}

struct Writer<W> {
    state: WriterState<W>,

    bufs: Vec<Cursor<Vec<u8>>>,
    cur: usize,
    end: usize,

    callbacks: Vec<Option<Box<FnBox(Result<(), Error>, Vec<u8>)>>>,
}

impl<W: StreamWrite> Writer<W> {
    fn new(wr: W) -> Self {
        let mut callbacks = Vec::with_capacity(PIPELINE_NUMBER);
        for _ in 0..PIPELINE_NUMBER {
            callbacks.push(None);
        }

        Writer {
            state: WriterState::Idle(wr),

            bufs: vec![Cursor::new(Vec::new()); PIPELINE_NUMBER],
            cur: 0,
            end: 0,
            callbacks: callbacks,
        }
    }

    fn write<F>(&mut self, id: usize, buf: Vec<u8>, f: F)
        where F: Fn(Result<(), Error>, Vec<u8>) + 'static
    {
        // Try to write some data right away, if we don't have anything pending.
        if let WriterState::Idle(ref mut wr) = self.state {
            match wr.send(&buf[..]) {
                Ok(nsize) if nsize == buf.len() => {
                    trace!("written {} bytes [request {}, fd: {}]", nsize, id, wr.as_raw_fd());
                    // Operation has been completed immediately, such performance, wow.
                    wr.context().borrow_mut().post(move || {
                        f(Ok(()), buf);
                    });
                    return;
                }
                Ok(nsize) => {
                    unimplemented!();
                    // self.bufs[id] = Cursor::new(buf);
                    // self.bufs[id].set_position(nsize as u64);
                    // self.callbacks[id] = Some(box f)
                }
                Err(err) => {
                    error!("failed to write: {:?}", err);
                }
            }
        }

        // Otherwise switch to flushing state.
        if let WriterState::Idle(wr) = std::mem::replace(&mut self.state, WriterState::Flushing) {
            // self.wr.async_send();
        }
    }
}

struct ConnectionState<D> {
    /// Response buffers.
    ///
    /// This is a fixed-sized container of preallocated (but growable) buffers for responses. The
    /// total count of these buffers equals the number of maximum allowed concurrent requests. Each
    /// time a new request is ready to be processed we try to pop one buffer, which is returned
    /// after response write completion.
    /// If there are no more buffers, a reader task is paused until at least one response is
    /// written.
    bufs: Vec<Vec<u8>>,

    rd: Option<Reader<D>>,
}

impl<D: Dispatch + 'static> ConnectionState<D> {
    fn new() -> Self {
        let mut bufs = vec![vec![0; 4096]; PIPELINE_NUMBER];
        for buf in &mut bufs {
            buf.clear();
        }

        ConnectionState {
            bufs: bufs,
            rd: None,
        }
    }

    pub fn read_more(&mut self) {
        if let Some(rd) = std::mem::replace(&mut self.rd, None) {
            debug!("start reader task [fd: {}]", rd.rd.as_ref().unwrap().as_raw_fd());
            rd.read_more();
        }
    }
}

fn make_connection<D: Dispatch + 'static>(dispatch: D, sock: TcpSocket) {
    let (rd, wr) = sock.into_pair();

    let state = Rc::new(RefCell::new(ConnectionState::new()));
    let handler = Handler::new(dispatch, state.clone(), Writer::new(wr));
    state.borrow_mut().rd = Some(Reader::new(rd, state.clone(), handler));
    state.borrow_mut().read_more();
}

struct PingDispatch;

impl Dispatch for PingDispatch {
    fn on_request(&mut self, request: &Request, response: Response<Self>) {
        debug!("processed ping event with Request {{ url: '{}' }}", str::from_utf8(&request.url[..]).unwrap());
        response.finish();
    }
}

fn run(context: SharedContext) {
    let reactor = context.borrow_mut().reactor;
    let default: epoll::EpollEvent = unsafe {
        std::mem::uninitialized()
    };

    let mut events = [default; 1024];

    loop {
        // Process queue.
        let timeout = {
            let mut size = context.borrow_mut().queue.len();
            trace!("processing {} operations", size);

            while size > 0 {
                let op = context.borrow_mut().queue.pop_front();
                match op {
                    Some(Operation::User(op)) => {
                        trace!("user op");
                        op.call_box(())
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
            if context.borrow_mut().queue.is_empty() {
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
                    let fd = event.data as i32;
                    trace!("processing event, fd: {}, events: {:?}", fd, event.events);

                    let mut ev = context.borrow_mut().fdmap.remove(&fd).unwrap();

                    // TODO: 1. Out of band events.
                    if event.events.contains(epoll::EPOLLOUT) {
                        if let Some(callback) = std::mem::replace(&mut ev.handle_write, None) {
                            callback.call_box(());
                        }
                    }

                    if event.events.contains(epoll::EPOLLIN) {
                        if let Some(callback) = std::mem::replace(&mut ev.handle_read, None) {
                            callback.call_box(());
                        }
                    }

                    context.borrow_mut().fdmap.insert(fd, ev);
                }
            }
            Err(..) => break,
        }
    }
}

pub struct PipeRead {
    fd: FileDesc,
    ctx: SharedContext,
}

impl PipeRead {
    unsafe fn from_raw_fd(fd: RawFd, ctx: &SharedContext) -> Result<PipeRead, Error> {
        let ev = epoll::EpollEvent {
            events: epoll::EPOLLIN | epoll::EPOLLET,
            data: fd as u64,
        };
        epoll::epoll_ctl(ctx.borrow_mut().reactor, epoll::EpollOp::EpollCtlAdd, fd, &ev)?;

        let evd = EventData {
            events: ev.events,
            handle_read: None,
            handle_write: None,
        };
        ctx.borrow_mut().fdmap.insert(fd, evd);

        let pipe = PipeRead {
            fd: FileDesc::from_raw_fd(fd),
            ctx: ctx.clone(),
        };

        Ok(pipe)
    }
}

impl AsRawFd for PipeRead {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

impl Bound for PipeRead {
    fn context(&self) -> &SharedContext {
        &self.ctx
    }
}

impl StreamRead for PipeRead {}

fn on_connection(n: Result<usize, Error>, buf: Cursor<Vec<u8>>, rd: PipeRead) {
    n.unwrap();

    let fd: i32 = {
        let s = &buf.get_ref()[..4];

        ((s[0] as i32 & 0xFF)) | ((s[1] as i32 & 0xFF) << 8) | ((s[2] as i32 & 0xFF) << 16)  | ((s[3] as i32 & 0xFF) << 24)
    };

    trace!("scheduled new connection, fd: {}", fd);
    let sock = unsafe {
        TcpSocket::from_raw_fd(fd, rd.context()).unwrap()
    };

    make_connection(PingDispatch, sock);

    rd.async_read(buf, on_connection);
}

pub fn serve(nthreads: usize, port: u16) {
    let mut workers = Vec::with_capacity(nthreads);
    for tid in 0..nthreads {
        let (rd, wr) = pipe2(O_NONBLOCK | O_CLOEXEC)
            .expect("failed to create the control channel");

        let thread = thread::Builder::new().name(format!("work#{:02}", tid)).spawn(move || {
            let ctx = SharedContext::new(RefCell::new(Context::new().unwrap()));
            let rd = unsafe {
                PipeRead::from_raw_fd(rd, &ctx).unwrap()
            };

            rd.async_read(Cursor::new(vec![0; 4]), on_connection);

            debug!("worker thread has been started");
            run(ctx);
            debug!("worker thread has been stopped");
        }).expect("failed to spawn worker thread");

        workers.push((thread, wr, rd));
    }

    // Listen.
    let listener = TcpListener::bind(("::", port)).unwrap();
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
            // TODO: Handle EMFILE.
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

trait DispatchFactory {
    type Dispatch: Dispatch;

    fn create(&self) -> Self::Dispatch;
}

pub struct Terminator;

pub struct Server {
    nthreads: usize,
}

impl Server {
    pub fn new(nthreads: usize) -> Self {
        unimplemented!();
    }

    pub fn serve(&mut self) {
        unimplemented!();
    }

    pub fn terminator(&mut self) -> Terminator {
        unimplemented!();
    }
}
