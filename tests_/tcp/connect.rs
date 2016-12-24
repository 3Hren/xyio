use std::cell::RefCell;
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;

use nix::errno::Errno;

use xyio::io::Context;
use xyio::io::net::ip::tcp::TcpStream;

#[test]
fn connect() {
    let (tx, rx) = mpsc::channel();

    let thread = thread::spawn(move || {
        let acceptor = TcpListener::bind(("::", 0)).unwrap();
        let endpoint = acceptor.local_addr().unwrap();
        tx.send(endpoint).unwrap();

        acceptor.accept().ok().expect("expected socket acception");
    });

    let endpoint = rx.recv().unwrap();

    let connected = RefCell::new(false);
    let mut context = Context::new().unwrap();

    let callback = |result: Result<TcpStream, Errno>| {
        result.unwrap();
        *connected.borrow_mut() = true;
    };

    TcpStream::connect(endpoint, callback, &context).unwrap();

    context.run();

    assert!(*connected.borrow_mut());

    thread.join().unwrap();
}

#[test]
fn write() {
    let (tx, rx) = mpsc::channel();

    let thread = thread::spawn(move || {
        use std::io::Read;

        let acceptor = TcpListener::bind(("::", 0)).unwrap();
        let endpoint = acceptor.local_addr().unwrap();
        tx.send(endpoint).unwrap();

        let mut stream = acceptor.accept().ok().expect("expected socket acception").0;
        let mut buf = [0u8; 16];
        let nread = stream.read(&mut &mut buf[..]).unwrap();

        assert_eq!(4, nread);
        assert_eq!(&[0, 1, 2, 3][..], &buf[..4]);
    });

    let endpoint = rx.recv().unwrap();

    let written = RefCell::new(false);
    let mut context = Context::new().unwrap();

    let callback = |result: Result<TcpStream, Errno>| {
        result.unwrap();
        *written.borrow_mut() = true;
    };

    TcpStream::connect(endpoint, callback, &context).unwrap();

    context.run();

    assert!(*written.borrow_mut());

    thread.join().unwrap();
}
