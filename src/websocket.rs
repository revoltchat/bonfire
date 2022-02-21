use std::net::SocketAddr;

use async_tungstenite::tungstenite::Message;
use futures::{channel::oneshot, pin_mut, select, FutureExt, SinkExt, StreamExt, TryStreamExt};
use revolt_quark::{
    events::{
        client::EventV1,
        server::ClientMessage,
        state::{State, SubscriptionStateChange},
    },
    presence::{presence_create_session, presence_delete_session},
    redis_kiss, Database,
};

use async_std::{channel, net::TcpStream, task};

use crate::config::WebsocketHandshakeCallback;

pub fn spawn_client(db: &'static Database, stream: TcpStream, addr: SocketAddr) {
    task::spawn(async move {
        info!("User connected from {addr:?}");

        let (sender, receiver) = oneshot::channel();
        if let Ok(ws) = async_tungstenite::accept_hdr_async_with_config(
            stream,
            WebsocketHandshakeCallback::from(sender),
            None,
        )
        .await
        {
            if let Ok(mut config) = receiver.await {
                info!(
                    "User {addr:?} provided protocol configuration (version = {}, format = {:?})",
                    config.get_protocol_version(),
                    config.get_protocol_format()
                );

                // Split the socket for simultaneously read and write.
                let (mut write, mut read) = ws.split();

                // If the user has not provided authentication, request information.
                if config.get_session_token().is_none() {
                    'outer: while let Ok(message) = read.try_next().await {
                        let msg = config.decode(message.as_ref().unwrap()).unwrap();

                        if let ClientMessage::Authenticate { token } = msg {
                            config.set_session_token(token);
                            break 'outer;
                        }
                    }
                }

                // Try to authenticate the user.
                if let Some(token) = config.get_session_token().as_ref() {
                    match db.fetch_user_by_token(token).await {
                        Ok(user) => {
                            info!("User {addr:?} authenticated as @{}", user.username);

                            // Create local state.
                            let mut state = State::from(user);
                            let user_id = state.cache.user_id.clone();

                            // Create presence session.
                            let session_id = presence_create_session(&user_id, 0).await;

                            // Notify socket we have authenticated.
                            write
                                .send(config.encode(&EventV1::Authenticated))
                                .await
                                .ok();

                            // Download required data to local cache and send Ready payload.
                            if let Ok(ready_payload) = state.generate_ready_payload(db).await {
                                write.send(config.encode(&ready_payload)).await.ok();

                                // Write channel to WebSocket.
                                let (send, mut recv) = channel::unbounded::<Message>();
                                let socket_worker = async {
                                    while let Some(msg) = recv.next().await {
                                        write.send(msg).await.ok();
                                    }
                                }
                                .fuse();

                                // Create a PubSub connection to poll on.
                                let listener = async {
                                    if let Ok(mut conn) = redis_kiss::open_pubsub_connection().await
                                    {
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

                                            // Debug logging of current subscriptions.
                                            #[cfg(debug_assertions)]
                                            info!(
                                                "User {addr:?} is subscribed to {:?}",
                                                state
                                                    .iter_subscriptions()
                                                    .collect::<Vec<&String>>()
                                            );

                                            // Handle incoming events.
                                            match conn.on_message().next().await.map(|item| {
                                                (
                                                    item.get_channel_name().to_string(),
                                                    redis_kiss::decode_payload::<EventV1>(&item),
                                                )
                                            }) {
                                                Some((channel, item)) => {
                                                    if let Ok(mut event) = item {
                                                        if state
                                                            .handle_incoming_event_v1(
                                                                db, &mut event,
                                                            )
                                                            .await
                                                            && send
                                                                .send(config.encode(&event))
                                                                .await
                                                                .is_err()
                                                        {
                                                            break;
                                                        }
                                                    } else {
                                                        warn!(
                                                    "Failed to deserialise an event for {channel}!"
                                                );
                                                    }
                                                }
                                                // No more data, assume we disconnected or otherwise
                                                // something bad occurred, so disconnect user.
                                                None => break,
                                            }
                                        }
                                    }
                                }
                                .fuse();

                                // Read from WebSocket stream.
                                let worker = async {
                                    while let Ok(Some(msg)) = read.try_next().await {
                                        if let Ok(payload) = config.decode(&msg) {
                                            match payload {
                                                ClientMessage::BeginTyping { channel } => {
                                                    EventV1::ChannelStartTyping {
                                                        id: channel.clone(),
                                                        user: user_id.clone(),
                                                    }
                                                    .p(channel.clone())
                                                    .await;
                                                }
                                                ClientMessage::EndTyping { channel } => {
                                                    EventV1::ChannelStopTyping {
                                                        id: channel.clone(),
                                                        user: user_id.clone(),
                                                    }
                                                    .p(channel.clone())
                                                    .await;
                                                }
                                                ClientMessage::Ping { data, responded } => {
                                                    if responded.is_none() {
                                                        send.send(
                                                            config.encode(&EventV1::Pong { data }),
                                                        )
                                                        .await
                                                        .ok();
                                                    }
                                                }
                                                _ => {}
                                            }
                                        }
                                    }
                                }
                                .fuse();

                                // Pin both tasks.
                                pin_mut!(socket_worker, listener, worker);

                                // Wait for either disconnect or for listener to die.
                                select!(
                                    () = socket_worker => {},
                                    () = listener => {},
                                    () = worker => {}
                                );

                                // * Combine the streams back once we are ready to disconnect.
                                /* ws = read.reunite(write).unwrap(); */
                            }

                            // Clean up presence session.
                            presence_delete_session(&user_id, session_id).await;
                        }
                        Err(err) => {
                            write.send(config.encode(&err)).await.ok();
                        }
                    }
                }
            }

            // * Disconnect the WebSocket if it isn't already.
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
