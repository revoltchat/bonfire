use std::net::SocketAddr;

use async_tungstenite::tungstenite::{handshake, Message};
use futures::{
    channel::oneshot::{self, Sender},
    pin_mut, select, FutureExt, SinkExt, StreamExt, TryStreamExt,
};
use revolt_quark::{
    events::{
        client::EventV1,
        state::{State, SubscriptionStateChange},
    },
    redis_kiss, Database,
};

use async_std::{net::TcpStream, task};

#[derive(Debug)]
struct ProtocolConfiguration {
    protocol_version: i32,
    format: String,
    session_token: Option<String>,
}

struct Callback {
    sender: Sender<ProtocolConfiguration>,
}

impl handshake::server::Callback for Callback {
    fn on_request(
        self,
        request: &handshake::server::Request,
        response: handshake::server::Response,
    ) -> Result<handshake::server::Response, handshake::server::ErrorResponse> {
        let query = request.uri().query().unwrap_or_default();
        let params = querystring::querify(query);

        let mut protocol_version = 1;
        let mut format = "json".into();
        let mut session_token = None;

        for (key, value) in params {
            match key {
                "version" => {
                    if let Ok(version) = value.parse() {
                        protocol_version = version;
                    }
                }
                "format" => match value {
                    // ! FIXME: support msgpack
                    "json" | "msgpack" => format = value.into(),
                    _ => {}
                },
                "token" => session_token = Some(value.into()),
                _ => {}
            }
        }

        if self
            .sender
            .send(ProtocolConfiguration {
                protocol_version,
                format,
                session_token,
            })
            .is_ok()
        {
            Ok(response)
        } else {
            Err(handshake::server::ErrorResponse::new(None))
        }
    }
}

pub fn spawn_client(db: &'static Database, stream: TcpStream, addr: SocketAddr) {
    task::spawn(async move {
        info!("User connected from {addr:?}");

        let (sender, receiver) = oneshot::channel();
        if let Ok(ws) =
            async_tungstenite::accept_hdr_async_with_config(stream, Callback { sender }, None).await
        {
            if let Ok(mut config) = receiver.await {
                info!(
                    "User {addr:?} provided protocol configuration (version = {}, format = {})",
                    config.protocol_version, config.format
                );

                // Split the socket for simultaneously read and write.
                let (mut write, mut read) = ws.split();

                // Authenticate user.
                if config.session_token.is_none() {
                    // TODO: get token
                }

                match db
                    .fetch_user_by_token(config.session_token.as_ref().unwrap())
                    .await
                {
                    Ok(user) => {
                        // Create local state.
                        let mut state = State::from(user);

                        // Notify socket we have authenticated.
                        write
                            .send(Message::Text(
                                serde_json::to_string(&EventV1::Authenticated).unwrap(),
                            ))
                            .await
                            .ok();

                        // Send Ready payload.
                        let ready_payload = state.generate_ready_payload(db).await;

                        write
                            .send(Message::Text(
                                serde_json::to_string(&ready_payload).unwrap(),
                            ))
                            .await
                            .ok();

                        // Create a PubSub connection to poll on.
                        let listener = async {
                            if let Ok(mut conn) = redis_kiss::open_pubsub_connection().await {
                                loop {
                                    // Check for state changes for subscriptions.
                                    match state.apply_state() {
                                        SubscriptionStateChange::Reset => {
                                            for id in state.iter_subscriptions() {
                                                conn.subscribe(id).await.unwrap();
                                            }
                                        }
                                        SubscriptionStateChange::Change { add, remove } => {
                                            for id in remove {
                                                conn.unsubscribe(id).await.unwrap();
                                            }

                                            for id in add {
                                                conn.subscribe(id).await.unwrap();
                                            }
                                        }
                                        SubscriptionStateChange::None => {}
                                    }

                                    // Handle incoming events.
                                    if let Some((channel, item)) =
                                        conn.on_message().next().await.map(|item| {
                                            (
                                                item.get_channel_name().to_string(),
                                                redis_kiss::decode_payload::<EventV1>(&item),
                                            )
                                        })
                                    {
                                        if let Ok(mut event) = item {
                                            state.handle_incoming_event_v1(db, &mut event).await;

                                            if write
                                                .send(Message::Text(
                                                    serde_json::to_string(&event).unwrap(),
                                                ))
                                                .await
                                                .is_err()
                                            {
                                                break;
                                            }
                                        } else {
                                            warn!("Failed to deserialise an event for {channel}!");
                                        }
                                    }
                                }
                            }
                        }
                        .fuse();

                        // Read from WebSocket stream.
                        let worker =
                            async { while let Ok(Some(_)) = read.try_next().await {} }.fuse();

                        // Pin both tasks.
                        pin_mut!(listener, worker);

                        // Wait for either disconnect or for listener to die.
                        select!(
                            () = listener => {},
                            () = worker => {}
                        );

                        // Combine the streams back once we are ready to disconnect.
                        /* ws = read.reunite(write).unwrap(); */
                    }
                    Err(err) => {
                        write
                            .send(Message::Text(serde_json::to_string(&err).unwrap()))
                            .await
                            .ok();
                    }
                }
            }

            // Disconnect the WebSocket if it isn't already.
            /*ws.close(Some(CloseFrame {
                code: CloseCode::Normal,
                reason: std::borrow::Cow::from(""),
            }))
            .await
            .unwrap();*/
        }

        info!("User disconnected from {addr:?}");
    });
}
