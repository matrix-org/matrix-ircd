// Copyright 2016 Openmarket
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Matrix IRCd is an IRCd implementation backed by Matrix, allowing IRC clients to interact
//! directly with a Matrix home server.

#[macro_use]
extern crate serde_derive;
extern crate serde_json;
extern crate serde;
#[macro_use]
extern crate futures;
#[macro_use]
extern crate tokio_core;
extern crate tokio_proto;
extern crate tokio_io;
extern crate tokio_tls;
extern crate tokio_dns;
#[macro_use]
extern crate slog;
extern crate slog_async;
extern crate slog_term;
extern crate url;
#[macro_use]
extern crate lazy_static;
#[macro_use]
extern crate pest;
#[macro_use]
extern crate quick_error;
extern crate openssl;
#[macro_use]
extern crate clap;
extern crate httparse;
extern crate netbuf;
extern crate rand;
extern crate tasked_futures;
extern crate native_tls;
extern crate regex;


use clap::{Arg, App};

use futures::Future;
use futures::stream::Stream;

use slog::Drain;

use std::cell::RefCell;
use std::net::SocketAddr;
use std::fs::File;
use std::io::{self, Read};
use std::sync::Arc;

use tokio_core::net::TcpListener;
use tokio_core::reactor::Core;

use native_tls::{TlsAcceptor, Identity};

use tasked_futures::TaskExecutor;



lazy_static! {
    static ref DEFAULT_LOGGER: slog::Logger = {
        let decorator = slog_term::TermDecorator::new().build();
        let drain = slog_term::CompactFormat::new(decorator).build().fuse();
        let drain = slog_async::Async::new(drain).build().fuse();
        slog::Logger::root(drain, o!("version" => env!("CARGO_PKG_VERSION")))
    };
}


task_local! {
    // A task local context describing the connection (from an IRC client).
    static CONTEXT: RefCell<Option<ConnectionContext>> = RefCell::new(None)
}


#[macro_use]
pub mod macros;
pub mod bridge;
pub mod irc;
pub mod matrix;
pub mod stream_fold;
pub mod http;


/// A task local context describing the connection (from an IRC client).
#[derive(Clone)]
pub struct ConnectionContext {
    logger: Arc<slog::Logger>,
    peer_addr: SocketAddr,
}


fn load_pkcs12_from_file(cert_file: &str, password: &str) -> Result<Identity, String> {
    File::open(&cert_file)
    .and_then(|mut file| {
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;
        Ok(buf)
    })
    .map_err(|err| format!("Failed to load Pkcs12: {}", err))
    .and_then(|buf| {
        Identity::from_pkcs12(&buf, password)
        .map_err(|err| format!("Failed to load Pkcs12: {}", err))
    })
}

fn main() {
    let matches = App::new("IRC Matrix Daemon")
        .version(crate_version!())
        .author(crate_authors!())
        .arg(Arg::with_name("BIND")
            .short("b")
            .long("bind")
            .help("Sets the address to bind to. Defaults to 127.0.0.1:5999")
            .takes_value(true)
            .validator(|addr| {
                addr.parse::<SocketAddr>()
                .map(|_| ())
                .map_err(|err| format!("Invalid bind address: {}", err))
            })
        )
        .arg(Arg::with_name("PKCS12")
            .long("pkcs12")
            .help("Sets the PKCS#12 file to read TLS cert and pkeyfrom")
            .takes_value(true)
            .requires("PASSWORD")
        )
        .arg(Arg::with_name("PASSWORD")
            .long("password")
            .help("The password of the PKCS#12 file")
            .takes_value(true)
             .requires("PKCS12")
        )
        .arg(Arg::with_name("MATRIX_HS")
            .long("url")
            .help("The base url of the Matrix HS")
            .required(true)
            .takes_value(true)
            .validator(|hs| {
                url::Url::parse(&hs)
                .map(|_| ())
                .map_err(|err| format!("Invalid url: {}", err))
            })
        )
        .get_matches();

    let log = &DEFAULT_LOGGER;

    info!(log, "Starting up");

    let bind_addr = matches.value_of("BIND").unwrap_or("127.0.0.1:5999");
    let addr = bind_addr.parse::<SocketAddr>().unwrap();
    let matrix_url = url::Url::parse(matches.value_of("MATRIX_HS").unwrap()).unwrap();

    let mut tls = false;
    let tls_acceptor = if let Some(pkcs12_file) = matches.value_of("PKCS12") {
        tls = true;

        let pass = matches.value_of("PASSWORD").unwrap();

        let pkcs12 = load_pkcs12_from_file(pkcs12_file, pass).expect("error reading pkcs12");
        let acceptor = TlsAcceptor::builder(pkcs12).build().unwrap();
        let tokio_acceptor = tokio_tls::TlsAcceptor::from(acceptor);
        Some(Arc::new(tokio_acceptor))
    } else {
        None
    };

    let mut l = Core::new().unwrap();
    let handle = l.handle();

    let socket = TcpListener::bind(&addr, &handle).unwrap();

    info!(log, "Started listening"; "addr" => bind_addr, "tls" => tls);

    // This is the main loop where we accept incoming *TCP* connections.
    let done = socket.incoming().for_each(move |(socket, addr)| {
        let peer_log = log.new(o!("ip" => format!("{}", addr.ip()), "port" => addr.port()));

        let ctx = ConnectionContext {
            logger: Arc::new(peer_log),
            peer_addr: addr,
        };

        let new_handle = handle.clone();

        let cloned_ctx = ctx.clone();

        // Set up a new task for the connection. We do this early so that the logging is correct.
        let setup_future = futures::lazy(move || {
            debug!(cloned_ctx.logger.as_ref(), "Accepted connection");

            CONTEXT.with(|m| {
                *m.borrow_mut() = Some(cloned_ctx);
            });

            Ok(socket)
        });

        // TODO: This should be configurable. Maybe use the matrix HS server_name?
        let irc_server_name = "localhost".into();

        // We need to clone here as the borrow checker doesn't like us taking ownership through
        // two levels of closures (understandably)
        let cloned_url = matrix_url.clone();

        if let Some(acceptor) = tls_acceptor.clone() {
            // Do the TLS handshake and then set up the bridge
            let future = setup_future.and_then(move |socket| {
                acceptor.accept(socket)
                .map_err(|err| io::Error::new(io::ErrorKind::Other, err))
            })
            .map_err(move |err| {
                task_warn!("TLS handshake failed"; "error" => format!("{}", err));
            })
            .and_then(move |tls_socket| {
                // the Bridge::create returns a future that resolves to a Bridge object. The
                // Bridge object is *also* a future that we want to wait on, so we use `flatten()`
                bridge::Bridge::create(new_handle, cloned_url, tls_socket, irc_server_name, ctx)
                .map(|bridge| bridge.into_future())
                .flatten()
                .map_err(|err| task_warn!("Unhandled IO error"; "error" => format!("{}", err)))
            }).then(|r| {
                task_info!("Finished");
                r
            });

            // We spawn the future off, otherwise we'd block the stream of incoming connections.
            // This is what causes the future to be in its own chain.
            handle.spawn(future);
        } else {
            // Same as above except with less TLS.
            let future = setup_future
            .and_then(move |socket| {
                bridge::Bridge::create(new_handle, cloned_url, socket, irc_server_name, ctx)
            })
            .map(|bridge| bridge.into_future())
            .flatten()
            .map_err(|err| task_warn!("Unhandled IO error"; "error" => format!("{}", err)));

            handle.spawn(future);
        };

        Ok(())
    });
    l.run(done).unwrap();
}
