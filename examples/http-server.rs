extern crate clap;
#[macro_use]
extern crate log;
extern crate xyio;

use clap::{App, Arg};

use xyio::logging;
use xyio::http::server::serve;

fn main() {
    let matches = App::new("Proof of concept of asynchronous HTTP 1.1 server")
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
        .expect("failed to parse THREADS argument");

    serve(nthreads);

    // let server = ServerBuilder::new()
    //     .with_threads(nthreads)
    //     .build();
    // server.serve(PingDispatchFactory).join();
}
