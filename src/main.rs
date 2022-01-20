//! Requires the "client", "standard_framework", and "voice" features be enabled
//! in your Cargo.toml, like so:
//!
//! ```toml
//! [dependencies.serenity]
//! git = "https://github.com/serenity-rs/serenity.git"
//! features = ["client", "standard_framework", "voice"]
//! ```
use serde::{Deserialize, Serialize};
use std::env;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::Mutex;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::{
    connect_async, tungstenite::protocol::Message as TMessage, tungstenite::Error as TError,
};

use futures::prelude::stream::SplitSink;
use serenity::{
    async_trait,
    client::{Client, Context, EventHandler},
    framework::{
        standard::{
            macros::{command, group},
            Args, CommandResult,
        },
        StandardFramework,
    },
    futures::{SinkExt, StreamExt},
    http::Http,
    model::{channel::Message, gateway::Ready, id::ChannelId, misc::Mentionable},
    prelude::SerenityError,
    Result as SerenityResult,
};

use songbird::{
    driver::DecodeMode,
    model::payload::{ClientConnect, ClientDisconnect, Speaking},
    Config, CoreEvent, Event, EventContext, EventHandler as VoiceEventHandler, SerenityInit,
};

struct Handler;

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, _: Context, ready: Ready) {
        println!("{} is connected!", ready.user.name);
    }
}

struct Receiver {
    write: Mutex<
        SplitSink<
            WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
            tokio_tungstenite::tungstenite::Message,
        >,
    >,
    to_send: Mutex<Vec<i16>>,
}

impl Receiver {
    pub fn new(
        write: Mutex<
            SplitSink<
                WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
                tokio_tungstenite::tungstenite::Message,
            >,
        >,
    ) -> Self {
        // You can manage state here, such as a buffer of audio packet bytes so
        // you can later store them in intervals.
        Self {
            write,
            to_send: Mutex::new(vec![]),
        }
    }
}

#[async_trait]
impl VoiceEventHandler for Receiver {
    #[allow(unused_variables)]
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        use EventContext as Ctx;
        match ctx {
            Ctx::SpeakingStateUpdate(Speaking {
                speaking,
                ssrc,
                user_id,
                ..
            }) => {
                // Discord voice calls use RTP, where every sender uses a randomly allocated
                // *Synchronisation Source* (SSRC) to allow receivers to tell which audio
                // stream a received packet belongs to. As this number is not derived from
                // the sender's user_id, only Discord Voice Gateway messages like this one
                // inform us about which random SSRC a user has been allocated. Future voice
                // packets will contain *only* the SSRC.
                //
                // You can implement logic here so that you can differentiate users'
                // SSRCs and map the SSRC to the User ID and maintain this state.
                // Using this map, you can map the `ssrc` in `voice_packet`
                // to the user ID and handle their audio packets separately.
                println!(
                    "Speaking state update: user {:?} has SSRC {:?}, using {:?}",
                    user_id, ssrc, speaking,
                );
            }
            Ctx::SpeakingUpdate(data) => {
                // You can implement logic here which reacts to a user starting
                // or stopping speaking.
                println!(
                    "Source {} has {} speaking.",
                    data.ssrc,
                    if data.speaking { "started" } else { "stopped" },
                );
            }
            Ctx::VoicePacket(data) => {
                // An event which fires for every received audio packet,
                // containing the decoded data.
                if let Some(audio) = data.audio {
                    let mut to_send = self.to_send.lock().await;
                    (*to_send).extend(audio);
                    if to_send.len() > 96_000 {
                        let message: TMessage = convert(&to_send);
                        let mut write = self.write.lock().await;
                        if let Err(err) = (*write).send(message).await {
                            println!("Could not send audio data : {:?}", err);
                        }
                        (*to_send).clear()
                    }
                } else {
                    println!("RTP packet, but no audio. Driver may not be configured to decode.");
                }
            }
            Ctx::RtcpPacket(data) => {
                // An event which fires for every received rtcp packet,
                // containing the call statistics and reporting information.
                println!("RTCP packet received: {:?}", data.packet);
            }
            Ctx::ClientConnect(ClientConnect {
                audio_ssrc,
                video_ssrc,
                user_id,
                ..
            }) => {
                // You can implement your own logic here to handle a user who has joined the
                // voice channel e.g., allocate structures, map their SSRC to User ID.

                println!(
                    "Client connected: user {:?} has audio SSRC {:?}, video SSRC {:?}",
                    user_id, audio_ssrc, video_ssrc,
                );
            }
            Ctx::ClientDisconnect(ClientDisconnect { user_id, .. }) => {
                // You can implement your own logic here to handle a user who has left the
                // voice channel e.g., finalise processing of statistics etc.
                // You will typically need to map the User ID to their SSRC; observed when
                // speaking or connecting.

                println!("Client disconnected: user {:?}", user_id);
            }
            _ => {
                // We won't be registering this struct for any more event classes.
                unimplemented!()
            }
        }

        None
    }
}

#[derive(Debug, Serialize)]
struct Sample {
    raw: String,
    sampling_rate: usize,
    format: String,
}

fn convert(audio: &Vec<i16>) -> TMessage {
    // Very dumb downsampling.
    let sampling_rate = 24_000;
    let skip = 96_000 / sampling_rate;
    let audio_f32: Vec<f32> = audio
        .iter()
        .step_by(skip)
        .map(|&value_i16| value_i16 as f32 / i16::MAX as f32)
        .collect();
    let buffer: Vec<u8> = audio_f32
        .into_iter()
        .map(|c| c.to_le_bytes())
        .flatten()
        .collect::<Vec<_>>();

    let sample = Sample {
        raw: base64::encode(buffer),
        sampling_rate,
        format: "f32le".to_string(),
    };
    let serialized = serde_json::to_string(&sample).unwrap();
    TMessage::text(serialized)
}

#[group]
#[commands(join, leave, ping)]
struct General;

#[command]
#[only_in(guilds)]
async fn join(ctx: &Context, msg: &Message, mut args: Args) -> CommandResult {
    let guild = msg.guild(&ctx.cache).await.unwrap();
    let guild_id = guild.id;
    let channels = ctx.http.get_channels(*guild_id.as_u64()).await?;

    let connect_to = match args.single::<String>() {
        Ok(name) => {
            if let Some(channel) = channels.iter().find(|c| c.name == name) {
                ChannelId(*channel.id.as_u64())
            } else {
                check_msg(
                    msg.reply(ctx, format!("Could not find channel {:?}", name))
                        .await,
                );
                return Ok(());
            }
        }
        Err(_) => {
            check_msg(
                msg.reply(ctx, "Requires a valid voice channel ID be given")
                    .await,
            );

            return Ok(());
        }
    };

    let model_id = match args.single::<String>() {
        Ok(model_id) => model_id,
        Err(_) => "facebook/wav2vec2-base-960h".to_string(),
    };

    let api_token = env::var("HF_API_TOKEN").expect("Expected an API token in the environment");
    let url = format!(
        "wss://api-inference.huggingface.co/asr/live/cpu/{}",
        model_id
    );
    // let url = "ws://localhost:8000/asr/live/gpu/facebook/wav2vec2-base-960h";
    println!("Connecting to  {:?}", url);
    let (ws_stream, _) = connect_async(url).await.expect("Failed to connect");
    let (mut write, read) = ws_stream.split();

    let http: Arc<Http> = Arc::clone(&ctx.http);
    let channel_id = msg.channel_id;
    let content_msg = channel_id.say(&http, "...").await.unwrap();
    let ws_out = {
        // Fold shenanigans because FnMut and borrow checker.
        read.fold((channel_id, http, content_msg), on_receive)
    };
    tokio::spawn(ws_out);
    let message = TMessage::text(format!("Bearer {}", api_token));
    write.send(message).await.unwrap();

    let manager = songbird::get(ctx)
        .await
        .expect("Songbird Voice client placed in at initialisation.")
        .clone();

    let (handler_lock, conn_result) = manager.join(guild_id, connect_to).await;
    if let Ok(_) = conn_result {
        // NOTE: this skips listening for the actual connection result.
        let mut handler = handler_lock.lock().await;

        let receiver = Receiver::new(Mutex::new(write));
        // handler.add_global_event(CoreEvent::SpeakingStateUpdate.into(), &receiver);

        // handler.add_global_event(CoreEvent::SpeakingUpdate.into(), &receiver);

        handler.add_global_event(CoreEvent::VoicePacket.into(), receiver);

        // handler.add_global_event(CoreEvent::RtcpPacket.into(), &receiver);

        // handler.add_global_event(CoreEvent::ClientConnect.into(), &receiver);

        // handler.add_global_event(CoreEvent::ClientDisconnect.into(), &receiver);

        check_msg(
            msg.channel_id
                .say(&ctx.http, &format!("Joined {}", connect_to.mention()))
                .await,
        );
    } else {
        check_msg(
            msg.channel_id
                .say(&ctx.http, "Error joining the channel")
                .await,
        );
    }

    Ok(())
}

#[command]
#[only_in(guilds)]
async fn leave(ctx: &Context, msg: &Message) -> CommandResult {
    let guild = msg.guild(&ctx.cache).await.unwrap();
    let guild_id = guild.id;

    let manager = songbird::get(ctx)
        .await
        .expect("Songbird Voice client placed in at initialisation.")
        .clone();
    let has_handler = manager.get(guild_id).is_some();

    if has_handler {
        if let Err(e) = manager.remove(guild_id).await {
            check_msg(
                msg.channel_id
                    .say(&ctx.http, format!("Failed: {:?}", e))
                    .await,
            );
        }

        check_msg(msg.channel_id.say(&ctx.http, "Left voice channel").await);
    } else {
        check_msg(msg.reply(ctx, "Not in a voice channel").await);
    }

    Ok(())
}

#[command]
async fn ping(ctx: &Context, msg: &Message) -> CommandResult {
    check_msg(msg.channel_id.say(&ctx.http, "Pong!").await);

    Ok(())
}

/// Checks that a message successfully sent; if not, then logs why to stdout.
fn check_msg(result: SerenityResult<Message>) {
    if let Err(why) = result {
        println!("Error sending message: {:?}", why);
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
#[serde(rename_all = "lowercase")]
enum ApiMessage {
    Status(Status),
    Results(ASRResult),
}

#[derive(Debug, Serialize, Deserialize)]
struct Status {
    message: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct ASRResult {
    text: String,
    format: Vec<String>,
    partial: bool,
    length_in_s: f32,
}

type Packed = (ChannelId, Arc<Http>, Message);
#[derive(Debug, Error)]
enum OnReceiveError {
    #[error("websocket error")]
    WebsocketError(#[from] TError),
    #[error("serialization error")]
    SerdeError(#[from] serde_json::Error),
    #[error("discord error")]
    SerenityError(#[from] SerenityError),
}

async fn _on_receive(
    channel_id: ChannelId,
    http: Arc<Http>,
    content_msg: &mut Message,
    message: TMessage,
) -> Result<(), OnReceiveError> {
    let txt = message.into_text()?;
    let api_message: ApiMessage = serde_json::from_str(&txt)?;
    match api_message {
        ApiMessage::Results(res) => {
            if res.partial {
                if !res.text.is_empty() {
                    content_msg.edit(&http, |m| m.content(res.text)).await?;
                }
            } else {
                *content_msg = channel_id.say(&http, "...").await?;
            }
        }
        ApiMessage::Status(status) => {
            if status.message != "Successful login" {
                channel_id.say(&http, status.message).await?;
            }
        }
    };
    Ok(())
}

async fn on_receive(packed: Packed, message: Result<TMessage, TError>) -> Packed {
    if let Ok(ws_msg) = message {
        let (channel_id, http, mut content_msg) = packed;
        if let Ok(()) = _on_receive(channel_id, http.clone(), &mut content_msg, ws_msg).await {
            (channel_id, http, content_msg)
        } else {
            (channel_id, http, content_msg)
        }
    } else {
        println!("Errors on the socket {:?}", message);
        packed
    }
}

#[tokio::main]
async fn main() {
    // tracing_subscriber::fmt::init();

    // Configure the client with your Discord bot token in the environment.
    let token = env::var("HF_ASR_BOT").expect("Expected a token in the environment");

    let framework = StandardFramework::new()
        .configure(|c| c.prefix("!"))
        .group(&GENERAL_GROUP);

    // Here, we need to configure Songbird to decode all incoming voice packets.
    // If you want, you can do this on a per-call basis---here, we need it to
    // read the audio data that other people are sending us!
    let songbird_config = Config::default().decode_mode(DecodeMode::Decode);

    let mut client = Client::builder(&token)
        .event_handler(Handler)
        .framework(framework)
        .register_songbird_from_config(songbird_config)
        .await
        .expect("Err creating client");

    let _ = client
        .start()
        .await
        .map_err(|why| println!("Client ended: {:?}", why));
}
