mod error;

use async_trait::async_trait;
use error::BotResult;
use futures_util::{
    stream::{SplitSink, SplitStream},
    SinkExt, StreamExt,
};
use serde::{Deserialize, Serialize};
use tokio::{join, net::TcpStream, sync::mpsc};
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};

#[cfg(feature = "enable-tracing")]
use tracing::{error, info, trace};

#[derive(Deserialize, Debug)]
pub struct Obstacle {
    pub r#type: String,
    pub origin_x: f32,
    pub origin_y: f32,
    pub height: i32,
    pub width: i32,
    pub close_area_height: i32,
    pub close_area_width: i32,
}

#[derive(Deserialize, Debug)]
pub struct Player {
    pub height: i32,
    pub width: i32,
    pub pos_x: f32,
    pub pos_y: f32,
    pub rotation: f32,
    pub r#state: String,
}

#[derive(Deserialize, Debug)]
pub struct PlayState {
    pub level_time: f32,
    pub score: f32,
    pub player: Player,
    pub obstacles: Option<Vec<Obstacle>>,
}

#[derive(Serialize, Debug)]
pub struct Trigger {
    pub fly: bool,
}

#[async_trait]
pub trait FlappyConsumer {
    async fn handle_message(&mut self, message: PlayState) -> Trigger;
}

#[derive(Default)]
pub struct FlappyBot;

impl FlappyBot {
    pub async fn start<F>(&self, url: &str, name: &str, mut consumer: F) -> BotResult<()>
    where
        F: FlappyConsumer + Send + 'static,
    {
        #[cfg(feature = "enable-tracing")]
        info!("Starting FlappyBot");

        let (message_sender, message_receiver) = mpsc::channel::<Trigger>(64);
        let (consumer_sender, mut consumer_receiver) = mpsc::channel::<PlayState>(64);

        tokio::spawn(async move {
            while let Some(play_state) = consumer_receiver.recv().await {
                #[cfg(feature = "enable-tracing")]
                trace!("Received play_state for handling: {play_state:?}");

                let response = consumer.handle_message(play_state).await;

                if let Err(error) = message_sender.send(response).await {
                    error!("{error}");
                };
            }
        });

        let socket_handler = SocketHandler::new(consumer_sender, message_receiver);
        if let Err(error) = start_socket(url, name, socket_handler).await {
            #[cfg(feature = "enable-tracing")]
            error!("Failed to start socket: {error}");
        }
        Ok(())
    }
}

struct SocketHandler {
    pub(crate) sender: mpsc::Sender<PlayState>,
    pub(crate) receiver: mpsc::Receiver<Trigger>,
}

impl SocketHandler {
    fn new(sender: mpsc::Sender<PlayState>, receiver: mpsc::Receiver<Trigger>) -> Self {
        Self { sender, receiver }
    }
}

async fn start_socket(url: &str, name: &str, socket_handler: SocketHandler) -> BotResult<()> {
    #[cfg(feature = "enable-tracing")]
    info!("Connecting to WebSocket at: {url}");

    let request = format!("{url}/{name}");
    let (ws_stream, _) = connect_async(request).await?;

    let (write, read) = ws_stream.split();

    let join = tokio::spawn(async move {
        join!(
            read_from_socket(read, socket_handler.sender),
            write_to_socket(write, socket_handler.receiver)
        )
    });

    join.await?;
    Ok(())
}

async fn read_from_socket(
    mut read: SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    sender: mpsc::Sender<PlayState>,
) {
    while let Some(Ok(message)) = read.next().await {
        match message {
            Message::Binary(message) => {
                let play_state: PlayState = match serde_json::from_slice(&message) {
                    Ok(state) => state,
                    Err(error) => {
                        #[cfg(feature = "enable-tracing")]
                        error!("Failed to deserialize message: {error}");
                        continue;
                    }
                };

                #[cfg(feature = "enable-tracing")]
                trace!("Received binary message: {:?}", play_state);

                if let Err(error) = sender.send(play_state).await {
                    #[cfg(feature = "enable-tracing")]
                    error!("Failed to send message to channel: {error}");
                }
            }
            Message::Close(close_frame) => {
                #[cfg(feature = "enable-tracing")]
                info!("Received close frame: {close_frame:?}");
            }
            _ => {
                #[cfg(feature = "enable-tracing")]
                trace!("Received unhandled message type");
            }
        }
    }
}

async fn write_to_socket(
    mut write: SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
    mut receiver: mpsc::Receiver<Trigger>,
) {
    while let Some(trigger) = receiver.recv().await {
        #[cfg(feature = "enable-tracing")]
        trace!("Sending trigger to WebSocket: {:?}", trigger);

        let message = match serde_json::to_vec(&trigger) {
            Ok(bytes) => bytes,
            Err(error) => {
                #[cfg(feature = "enable-tracing")]
                error!("Failed to serialize trigger: {:?}", error);
                continue;
            }
        };

        if let Err(error) = write.send(Message::Binary(message.into())).await {
            #[cfg(feature = "enable-tracing")]
            error!("Failed to send message over WebSocket: {error}");
        }
    }
}
