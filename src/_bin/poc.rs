#![feature(fnbox)]

use std::boxed::FnBox;
use std::cell::RefCell;
use std::collections::VecDeque;

enum Operation<'a> {
    User(Box<FnBox() + 'a>),
}

struct Queue<'a> {
    pub queue: RefCell<VecDeque<Operation<'a>>>,
}

impl<'a> Queue<'a> {
    fn new() -> Queue<'a> {
        Queue {
            queue: RefCell::new(VecDeque::new()),
        }
    }

    fn push(&self, operation: Operation<'a>) {
        self.queue.borrow_mut().push_back(operation);
    }

    fn pop_front(&self) -> Option<Operation<'a>> {
        self.queue.borrow_mut().pop_front()
    }

    fn len(&self) -> usize {
        self.queue.borrow_mut().len()
    }
}

struct ConnectionHandler<'a: 'r, 'r> {
    socket: Socket<'a, 'r>
}

impl<'a: 'r, 'r> ConnectionHandler<'a, 'r> {
    fn new(socket: Socket<'a, 'r>) -> ConnectionHandler<'a, 'r> {
        ConnectionHandler {
            socket: socket,
        }
    }
}

trait Handler {
    fn do_complete(self);
}

impl<'a, 'r> Handler for ConnectionHandler<'a, 'r> {
    fn do_complete(self) {
//         // let queue: &'r Queue<'a> = self.into();
//         // let handler = MyHandler::new(queue);
//         // let socket = self.socket.clone();
//         // socket.async_do_some(self);
    }
}

#[derive(Clone)]
struct Socket<'a: 'r, 'r> {
    queue: &'r Queue<'a>,
}

impl<'a: 'r, 'r> Socket<'a, 'r> {
    fn new(queue: &'r Queue<'a>) -> Socket<'a, 'r> {
        Socket {
            queue: queue,
        }
    }

    fn async_do_some<H>(&mut self, handler: H)
        where H: Handler + 'r // Replace for 'b and it works.
    {
        // self.queue.push(Operation::User(Box::new(move || {
        //     handler.do_complete();
        // })));
    }
}

fn main() {
    let _queue = Queue::new();                              // -+ queue   | 'a: 'r
    let queue = &_queue;                                    // -+ &queue  | 'r
                                                            //  |
    let mut socket = Socket::new(queue);                    // -+ socket  | 'b == 'r
                                                            //  |
    let handler = ConnectionHandler::new(socket.clone());   // -+ handler | 'c == 'b == 'r
    socket.async_do_some(handler);                          //  |

    // loop {
    //     let mut size = queue.len();
    //
    //     while size > 0 {
    //         let operation = queue.pop_front().unwrap();
    //
    //         match operation {
    //             Operation::User(operation) => {
    //                 operation()
    //             }
    //         }
    //
    //         size -= 1;
    //     }
    // }
}
