use ConnectionContext;

use futures::{Async, Future, Poll};
use futures::stream::Stream;

use irc::{IrcCommand, IrcUserConnection, Numeric};

use matrix::{LoginError, MatrixClient};
use matrix::models::{SyncResponse, JoinedRoomSyncResponse};

use std::io;
use std::collections::BTreeMap;

use tokio_core::io::Io;
use tokio_core::reactor::Handle;
use url::Url;

mod room;


pub struct Bridge<IS: Io> {
    irc_conn: IrcUserConnection<IS>,
    matrix_client: MatrixClient,
    ctx: ConnectionContext,
    closed: bool,
    rooms: BTreeMap<String, room::Room>,
    channel_to_room_id: BTreeMap<String, String>,
}

impl<IS: Io> Bridge<IS> {
    pub fn create(handle: Handle, base_url: Url, stream: IS, irc_server_name: String, ctx: ConnectionContext) -> impl Future<Item=Bridge<IS>, Error=io::Error> {
        IrcUserConnection::await_login(irc_server_name, stream, ctx.clone())
        .and_then(move |mut user_connection| {
            MatrixClient::login(
                handle, base_url, user_connection.user.clone(), user_connection.password.clone()
            ).then(move |res| {
                match res {
                    Ok(matrix_client) => Ok(Bridge {
                        irc_conn: user_connection,
                        matrix_client: matrix_client,
                        ctx: ctx,
                        closed: false,
                        rooms: BTreeMap::new(),
                        channel_to_room_id: BTreeMap::new(),
                    }),
                    Err(LoginError::InvalidPassword) => {
                        user_connection.write_invalid_password();
                        Err(io::Error::new(io::ErrorKind::Other, "Invalid password"))
                    }
                    Err(LoginError::Io(e)) => Err(e),
                }
            })
        })
        .and_then(|mut bridge| {
            bridge.irc_conn.welcome();
            Ok(bridge)
        })
    }

    fn handle_irc_cmd(&mut self, line: IrcCommand) {
        debug!(self.ctx.logger, "Received IRC line"; "command" => line.command());

        match line {
            IrcCommand::Join { channel } => {
                let Bridge { ref channel_to_room_id, ref rooms, ref mut irc_conn, .. } = *self;
                if let Some(room_id) = channel_to_room_id.get(&channel) {
                    let room = rooms.get(room_id).expect("channel to room_id is incorrect");
                    irc_conn.write_join(&room.irc_name);
                    irc_conn.write_topic(&room.irc_name, "TEST TOPIC");
                    irc_conn.write_names(&room.irc_name, &["test".into()]);
                }
            }
            IrcCommand::Who{ matches } => {
                if matches.starts_with('#') {
                    self.handle_who_channel_cmd(matches);
                }
            }
            _ => {}
        }
    }

    fn handle_sync_response(&mut self, sync_response: SyncResponse) {
        trace!(self.ctx.logger, "Received sync response"; "batch" => sync_response.next_batch);
        // self.irc_conn.write_line(&format!("Batch: {}", sync_response.next_batch));

        for (room_id, sync) in &sync_response.rooms.join {
            self.handle_room_sync(room_id, sync);
        }
    }

    fn handle_room_sync(&mut self, room_id: &str, sync: &JoinedRoomSyncResponse) {
        if !self.rooms.contains_key(room_id) {
            let room = room::Room::from_sync(room_id.into(), sync);
            self.channel_to_room_id.insert(room.irc_name.clone(), room_id.into());
            self.send_join_irc_result(&room);
            self.rooms.insert(room_id.to_string(), room);
        }
    }

    fn send_join_irc_result(&mut self, room: &room::Room) {
        self.irc_conn.write_join(&room.irc_name);
        self.irc_conn.write_topic(&room.irc_name, "TEST TOPIC");
        self.irc_conn.write_names(&room.irc_name, &["test".into()]);
    }


    fn handle_who_channel_cmd(&mut self, channel: String) {
        let Bridge { ref channel_to_room_id, ref rooms, ref mut irc_conn, .. } = *self;

        if let Some(room_id) = channel_to_room_id.get(&channel) {
            let room = rooms.get(room_id).expect("channel to room_id is incorrect");

            irc_conn.write_numeric(Numeric::RPL_WHOREPLY, &format!("{} test cloak host test H :0 Test", &channel));
        }

        irc_conn.write_numeric(Numeric::RPL_ENDOFWHO, &format!("{} :End of /WHO", &channel));
    }

    fn poll_irc(&mut self) -> Poll<(), io::Error> {
        if let Some(line) = try_ready!(self.irc_conn.poll()) {
            self.handle_irc_cmd(line);
        } else {
            self.closed = true;
        }
        Ok(Async::Ready(()))
    }

    fn poll_matrix(&mut self) -> Poll<(), io::Error> {
        if let Some(sync_response) = try_ready!(self.matrix_client.poll()) {
            self.handle_sync_response(sync_response);
        } else {
            self.closed = true;
        }
        Ok(Async::Ready(()))
    }
}

impl<IS: Io> Future for Bridge<IS> {
    type Item = ();
    type Error = io::Error;

    fn poll(&mut self) -> Poll<(), io::Error> {
        trace!(self.ctx.logger, "Bridge Polled");

        loop {
            if self.closed {
                return Ok(Async::Ready(()));
            }

            if let (Async::NotReady, Async::NotReady) = (self.poll_irc()?, self.poll_matrix()?) {
                break;
            }
        }

        Ok(Async::NotReady)
    }
}
