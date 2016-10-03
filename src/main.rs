#![feature(rustc_macro, question_mark, conservative_impl_trait)]

#[macro_use]
extern crate serde_derive;
extern crate serde_json;
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

macro_rules! task_log {
    ($lvl:expr, $($args:tt)+) => {{
        use CONTEXT;

        let o = CONTEXT.with(move |m| {
            m.borrow().as_ref().map(|c| c.logger.clone())
        });
        if let Some(log) = o {
            log!($lvl, log, $($args)+)
        } else {
            log!($lvl, ::DEFAULT_LOGGER, $($args)+)
        }
    }}
}

macro_rules! task_trace {
    ($($args:tt)+) => {{
        task_log!(::slog::Level::Trace, $($args)+);
    }}
}

macro_rules! task_debug {
    ($($args:tt)+) => {{
        task_log!(::slog::Level::Debug, $($args)+);
    }}
}

macro_rules! task_info {
    ($($args:tt)+) => {{
        task_log!(::slog::Level::Info, $($args)+);
    }}
}

macro_rules! task_warn {
    ($($args:tt)+) => {{
        task_log!(::slog::Level::Warn, $($args)+);
    }}
}

macro_rules! task_error {
    ($($args:tt)+) => {{
        task_log!(::slog::Level::Error, $($args)+);
    }}
}

macro_rules! task_crit {
    ($($args:tt)+) => {{
        task_log!(::slog::Level::Crit, $($args)+);
    }}
}

mod bridge;
mod irc;
mod matrix;


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

    let access_token = String::from("MDAxY2xvY2F0aW9uIGxvY2FsaG9zdDo4NDgwCjAwMTNpZGVudGlmaWVyIGtleQowMDEwY2lkIGdlbiA9IDEKMDAyN2NpZCB1c2VyX2lkID0gQHRlc3Q6bG9jYWxob3N0Ojg0ODAKMDAxNmNpZCB0eXBlID0gYWNjZXNzCjAwMWRjaWQgdGltZSA8IDE0NzU1NzQ4MTA4OTMKMDAyZnNpZ25hdHVyZSBoN-vbJfT8QG1DPOq4YlWO3kpd0k-upt2kKtcqO5-qGwo");

    let done = socket.incoming().for_each(move |(socket, addr)| {
        let peer_log = log.new(o!("ip" => format!("{}", addr.ip()), "port" => addr.port()));

        let new_handle = handle.clone();
        let access_token = access_token.clone();
        handle.spawn(futures::lazy(move || {
            debug!(peer_log, "Accepted connection");

            let ctx = ConnectionContext {
                logger: peer_log,
                peer_addr: addr,
            };

            CONTEXT.with(|m| {
                *m.borrow_mut() = Some(ctx.clone());
            });

            let url = url::Url::parse("http://localhost:8080/").unwrap();
            let client = matrix::MatrixClient::new(new_handle, &url, access_token);
            let bridge = bridge::Bridge::new(socket, ctx, client);

            bridge.map_err(|e| {
                panic!("error: {}", e);
            })
        }));

        Ok(())
    });
    l.run(done).unwrap();
}
