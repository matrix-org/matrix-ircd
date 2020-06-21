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

// TODO: move this one to `use` format as well. It turns out its actually pretty difficult with
// ~50 errors and importing the macros in the files still results in errors
#[macro_use]
extern crate slog;

use clap::{App, Arg};

use futures3::stream::StreamExt;
use futures3::{Future, Stream};

use slog::Drain;

use std::cell::RefCell;
use std::fs::File;
use std::io::{self, Read};
use std::net::SocketAddr;
use std::sync::Arc;
use std::pin::Pin;

use tokio::net::TcpListener;

use native_tls::{Identity };

//use tasked_futures::TaskExecutor;

use std::task::Context;

lazy_static::lazy_static! {
    static ref DEFAULT_LOGGER: slog::Logger = {
        let decorator = slog_term::TermDecorator::new().build();
        let drain = slog_term::CompactFormat::new(decorator).build().fuse();
        let drain = slog_async::Async::new(drain).build().fuse();
        slog::Logger::root(drain, o!("version" => env!("CARGO_PKG_VERSION")))
    };
}

futures::task_local! {
    // A task local context describing the connection (from an IRC client).
    static CONTEXT: RefCell<Option<ConnectionContext>> = RefCell::new(None)
}

#[macro_use]
pub mod macros;
pub mod bridge;
pub mod http;
pub mod irc;
pub mod matrix;
pub mod stream_fold;

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

#[tokio::main]
async fn main() {
    let matches = App::new("IRC Matrix Daemon")
        .version(clap::crate_version!())
        .author(clap::crate_authors!())
        .arg(
            Arg::with_name("BIND")
                .short("b")
                .long("bind")
                .help("Sets the address to bind to. Defaults to 127.0.0.1:5999")
                .takes_value(true)
                .validator(|addr| {
                    addr.parse::<SocketAddr>()
                        .map(|_| ())
                        .map_err(|err| format!("Invalid bind address: {}", err))
                }),
        )
        .arg(
            Arg::with_name("PKCS12")
                .long("pkcs12")
                .help("Sets the PKCS#12 file to read TLS cert and pkeyfrom")
                .takes_value(true)
                .requires("PASSWORD"),
        )
        .arg(
            Arg::with_name("PASSWORD")
                .long("password")
                .help("The password of the PKCS#12 file")
                .takes_value(true)
                .requires("PKCS12"),
        )
        .arg(
            Arg::with_name("MATRIX_HS")
                .long("url")
                .help("The base url of the Matrix HS")
                .required(true)
                .takes_value(true)
                .validator(|hs| {
                    url::Url::parse(&hs)
                        .map(|_| ())
                        .map_err(|err| format!("Invalid url: {}", err))
                }),
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
        let acceptor = native_tls::TlsAcceptor::builder(pkcs12).build().unwrap();
        let tokio_acceptor = tokio_native_tls::TlsAcceptor::from(acceptor);
        Some(Arc::new(tokio_acceptor))
    } else {
        None
    };

    let mut socket = Box::pin(TcpListener::bind(&addr).await.unwrap());

    info!(log, "Started listening"; "addr" => bind_addr, "tls" => tls);

    // This is the main loop where we accept incoming *TCP* connections.
    while let Some(Ok(tcp_stream)) = socket.next().await {
        let addr = if let Ok(addr) = tcp_stream.peer_addr() {
            addr
        } else {
            // TODO: log this
            continue;
        };

        let peer_log = log.new(o!("ip" => format!("{}", addr.ip()), "port" => addr.port()));

        let ctx = ConnectionContext {
            logger: Arc::new(peer_log),
            peer_addr: addr,
        };

        //let new_handle = handle.clone();

        let cloned_ctx = ctx.clone();

        // Set up a new task for the connection. We do this early so that the logging is correct.
        let setup_future = futures3::future::lazy(move |cx: &mut Context| {
            debug!(cloned_ctx.logger.as_ref(), "Accepted connection");

            CONTEXT.with(|m| {
                *m.borrow_mut() = Some(cloned_ctx);
            });

        });

        // TODO: This should be configurable. Maybe use the matrix HS server_name?
        let irc_server_name = "localhost".into();

        // We need to clone here as the borrow checker doesn't like us taking ownership through
        // two levels of closures (understandably)
        let cloned_url = matrix_url.clone();

        if let Some(acceptor) = tls_acceptor.clone() {
            // Do the TLS handshake and then set up the bridge
            let spawn_fut = futures3::future::lazy(move |cx: &mut Context| async move {

                let socket = setup_future.await;
                let tls_socket = acceptor.accept(tcp_stream).await.map_err(|err| {
                    task_warn!("TLS handshake failed"; "error" => format!("{}", err));
                    io::Error::new(io::ErrorKind::Other, err);
                }).unwrap();

                let bridge = bridge::Bridge::create(
                    cloned_url, 
                    tls_socket, 
                    irc_server_name, 
                    ctx)
                    .await;
                          //.map_err(
                          //    //|err: Box<dyn futures3::future::Future<Output=Result<_, bridge::Error>>>| task_warn!("Unhandled IO error"; "error" => format!("{:?}", err)),
                          //    |err| task_warn!("Unhandled IO error"; "error" => format!("{:?}", err)),
                          //);

                task_info!("Finished");

            });

            // We spawn the future off, otherwise we'd block the stream of incoming connections.
            // This is what causes the future to be in its own chain.
            //handle.spawn(future);

            tokio::spawn(spawn_fut);
        } else {
           // // Same as above except with less TLS.
           // let future = setup_future
           //     .and_then(move |socket| {
           //         bridge::Bridge::create(cloned_url, socket, irc_server_name, ctx)
           //     })
           //     .map(|bridge| bridge.into_future())
           //     .flatten()
           //     .map_err(|err| task_warn!("Unhandled IO error"; "error" => format!("{}", err)));

           // //handle.spawn(future);
        };

    }
    //l.run(done).unwrap();
}
