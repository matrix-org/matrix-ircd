#![feature(rustc_macro, question_mark, conservative_impl_trait)]

#[macro_use]
extern crate serde_derive;
extern crate serde_json;
extern crate serde;
#[macro_use]
extern crate futures;
#[macro_use]
extern crate tokio_core;
extern crate tokio_curl;
extern crate tokio_proto;
#[macro_use]
extern crate slog;
extern crate slog_term;
extern crate curl;
extern crate url;
#[macro_use]
extern crate lazy_static;
#[macro_use]
extern crate pest;
#[macro_use]
extern crate quick_error;
extern crate itertools;


use futures::Future;
use futures::stream::Stream;

use slog::DrainExt;

use std::cell::RefCell;
use std::env;
use std::net::SocketAddr;

use tokio_core::net::TcpListener;
use tokio_core::reactor::Core;


lazy_static! {
    static ref DEFAULT_LOGGER: slog::Logger = {
        let drain = slog_term::streamer().compact().build().fuse();
        slog::Logger::root(drain, o!("version" => env!("CARGO_PKG_VERSION")))
    };
}


task_local! {
    static CONTEXT: RefCell<Option<ConnectionContext>> = RefCell::new(None)
}

#[macro_use]
mod macros;
mod bridge;
mod irc;
mod matrix;
mod stream_fold;


#[derive(Clone)]
pub struct ConnectionContext {
    logger: slog::Logger,
    peer_addr: SocketAddr,
}


fn main() {
    let log = &DEFAULT_LOGGER;

    info!(log, "Starting up");

    let addr_str = env::args().nth(1).unwrap_or("127.0.0.1:5999".to_string());
    let addr = addr_str.parse::<SocketAddr>().unwrap();

    let mut l = Core::new().unwrap();
    let handle = l.handle();

    let socket = TcpListener::bind(&addr, &handle).unwrap();

    info!(log, "Started listening"; "addr" => addr_str);

    let done = socket.incoming().for_each(move |(socket, addr)| {
        let peer_log = log.new(o!("ip" => format!("{}", addr.ip()), "port" => addr.port()));

        let new_handle = handle.clone();

        // We wrap the code in a lazy future so that its run in the new task.
        handle.spawn(futures::lazy(move || {
            debug!(peer_log, "Accepted connection");

            let ctx = ConnectionContext {
                logger: peer_log.clone(),
                peer_addr: addr,
            };

            CONTEXT.with(|m| {
                *m.borrow_mut() = Some(ctx.clone());
            });

            let url = url::Url::parse("http://localhost:8080/").unwrap();

            let irc_server_name = "localhost".into();

            bridge::Bridge::create(new_handle, url, socket, irc_server_name, ctx)
            .and_then(|bridge| {
                bridge
            }).map_err(move |err| {
                warn!(peer_log, "Unhandled IO error"; "error" => format!("{}", err));
            })

        }));

        Ok(())
    });
    l.run(done).unwrap();
}
