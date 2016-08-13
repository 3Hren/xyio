use std;
use std::boxed::FnBox;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use nix::errno::Errno;
use nix::sys::event;

pub trait HandleWrite {
    fn complete(self, result: Result<usize, Errno>);
    fn buf(&self) -> &[u8];
}

pub trait HandleRead {
    fn complete(self, result: Result<usize, Errno>);
    fn buf(&mut self) -> &mut [u8];
}

pub enum Operation<'a> {
    Poll,
    User(Box<FnBox() -> () + 'a>),
}

pub struct EventData<'a> {
    pub events: event::EventFilter,
    pub read_handler: Option<Box<FnBox() + 'a>>,
    pub write_handler: Option<Box<FnBox() + 'a>>,
}

impl<'a> EventData<'a> {
    pub fn new(read_handler: Option<Box<FnBox() + 'a>>, write_handler: Option<Box<FnBox() + 'a>>) -> EventData<'a> {
        EventData {
            events: event::EventFilter::EVFILT_SYSCOUNT,
            read_handler: read_handler,
            write_handler: write_handler,
        }
    }
}

pub struct DescriptorSet<'a>(RefCell<HashMap<i32, EventData<'a>>>);

impl<'a> DescriptorSet<'a> {
    pub fn new() -> DescriptorSet<'a> {
        DescriptorSet(RefCell::new(HashMap::new()))
    }

    pub fn insert(&self, fd: i32, val: EventData<'a>) -> Option<EventData<'a>> {
        self.0.borrow_mut().insert(fd, val)
    }

    pub fn get_mut(&self, fd: &i32) -> Option<&mut EventData<'a>> {
        unsafe {
            (*self.0.as_unsafe_cell().get()).get_mut(&fd)
        }
    }
}

pub struct OperationQueue<'a> {
    queue: RefCell<VecDeque<Operation<'a>>>,
}

impl<'a> OperationQueue<'a> {
    fn new() -> OperationQueue<'a> {
        OperationQueue {
            queue: RefCell::new(VecDeque::new()),
        }
    }

    pub fn push_back(&self, operation: Operation<'a>) {
        self.queue.borrow_mut().push_back(operation);
    }

    fn pop_front(&self) -> Option<Operation<'a>> {
        self.queue.borrow_mut().pop_front()
    }

    fn len(&self) -> usize {
        self.queue.borrow_mut().len()
    }

    fn is_empty(&self) -> bool {
        self.queue.borrow_mut().is_empty()
    }
}

pub struct ContextPriv<'a> {
    queue: OperationQueue<'a>,
    descriptors: DescriptorSet<'a>,
}

#[derive(Clone)]
pub struct Context<'a> {
    // TODO: This is the placeholder for reactor (or other magic for Windows).
    fd: i32,
    d: Rc<ContextPriv<'a>>,
}

impl<'a> Context<'a> {
    pub fn new() -> Result<Context<'a>, Errno> {
        let fd = event::kqueue().ok().expect("unable to initialize kqueue");

        let d = ContextPriv {
            queue: OperationQueue::new(),
            descriptors: DescriptorSet::new(),
        };

        let context = Context {
            fd: fd,
            d: Rc::new(d)
        };

        Ok(context)
    }

    pub fn run(&mut self) {
        let default: event::KEvent = unsafe { std::mem::uninitialized() };
        let mut events = [default; 1024];

        loop {
            let mut block = false;

            let mut size = self.d.queue.len();
            trace!(target: "Loop", "processing {} operations", size);

            if size == 0 {
                break;
            }

            while size > 0 {
                // Unwrapping here is ok, since we're sure about pending operations count.
                let op = self.d.queue.pop_front().unwrap();

                match op {
                    Operation::User(op) => {
                        trace!(target: "Loop", "user operation");
                        op()
                    },
                    Operation::Poll => {
                        trace!(target: "Loop", "poll operation");
                        block = true;
                    }
                }

                size -= 1;
            }

            let timeout = if self.d.queue.is_empty() {
                60000
            } else {
                0
            };

            if block {
                match event::kevent(self.fd, &[], &mut events, timeout) {
                    Ok(0) => {
                        trace!(target: "Work", "kqueue tick: timeout");
                    }
                    Ok(size) => {
                        trace!(target: "Work", "kqueue tick, size: {}", size);

                        let mut poll = false;
                        for event in &events[..size] {
                            let fd = event.ident as i32;
                            trace!(target: "Work", "processing event, fd: {}, events: {:?}, flags: {:?}", fd, event.filter, event.flags);

                            {
                                let mut ev = self.d.descriptors.get_mut(&fd).expect("fd not found");
                                // TODO: 1. Out of band events.
                                if event.filter == event::EventFilter::EVFILT_WRITE {
                                    let handler = std::mem::replace(&mut ev.write_handler, None);
                                    if let Some(handler) = handler {
                                        handler();
                                    }
                                }

                                if event.filter == event::EventFilter::EVFILT_READ {
                                    let handler = std::mem::replace(&mut ev.read_handler, None);
                                    if let Some(handler) = handler {
                                        handler();
                                    }
                                }

                                if ev.write_handler.is_some() || ev.read_handler.is_some() {
                                    poll = true;
                                }
                            }
                        }

                        if poll {
                            self.d.queue.push_back(Operation::Poll);
                        }
                    }
                    Err(..) => break,
                }
            }
        }
    }
}

pub mod net;
