use rusqlite::NO_PARAMS;
use rusqlite::{Connection, params};
use std::{fs, thread, panic, process, env};
use tokio_tungstenite::{connect_async, tungstenite::Message::Pong};
use serde::Deserialize;
use url::Url;
use log::{info, debug, error};
use clap::{load_yaml, crate_authors, crate_description, crate_version, App};
use env_logger::Env;
use regex::Regex;
use futures_util::{future, pin_mut, StreamExt};
use tokio::time::timeout;
use std::time::Duration;
use twitch_api2::helix::{HelixClient, streams::get_streams, videos::get_videos, clips::get_clips};
use twitch_oauth2::{AppAccessToken, ClientId, ClientSecret, TwitchToken};
use reqwest::Client as ReqwestClient;
use reqwest::get as ReqwestGet;

#[derive(Deserialize)]
struct Message {
    data: String,
    timestamp: i64,
}

#[derive(Deserialize)]
struct YoutubeOEmbed {
    title: String,
    author_name: String,
}

#[derive(Debug)]
struct RemoveEmbed {
    platform: String,
    url: String
}

const OEMBED_URL: &str = "https://www.youtube.com/oembed";

fn split_once(in_string: &str) -> (&str, &str) {
    let mut splitter = in_string.splitn(2, ' ');
    let first = splitter.next().unwrap();
    let second = splitter.next().unwrap();
    (first, second)
}

#[tokio::main]
async fn embed_cleanup() {
    let _ = dotenv::dotenv();
    let twitch_client: HelixClient<ReqwestClient> = HelixClient::default();
    let client_id = std::env::var("TWITCH_CLIENT_ID")
        .ok()
        .map(ClientId::new)
        .expect("Please set env: TWITCH_CLIENT_ID");
    let secret = std::env::var("TWITCH_CLIENT_SECRET")
        .ok()
        .map(ClientSecret::new)
        .expect("Please set env: TWITCH_CLIENT_SECRET");
    let mut token = AppAccessToken::get_app_access_token(&twitch_client, client_id, secret,vec![]).await.unwrap();

    let conn = Connection::open("./data/embeddb.db").unwrap();
    loop {
        let mut embeds = conn.prepare(
            "SELECT link,count(link) as freq from embeds where timest >= strftime('%s', 'now') - 60 group by link order by freq"
        ).unwrap();
        let embeds_iter = embeds.query_map(NO_PARAMS, |row| {
            let raw_embed: String = row.get(0).unwrap();
            let (platform, url) = raw_embed.split_once('/').unwrap();
            Ok(RemoveEmbed {
                platform: platform[1..].to_string(),
                url: url.to_string(),
            })
        }).unwrap();

        for embed in embeds_iter {
            let embed = embed.unwrap();
            match embed.platform.as_str() {
                "twitch" => {
                    match token.validate_token(&twitch_client).await {
                        Err(_) => {
                            token.refresh_token(&twitch_client).await.unwrap();
                        },
                        Ok(_) => {}
                    }
                    let req = get_streams::GetStreamsRequest::builder()
                        .user_login(vec![embed.url.clone().into()]).build();
                    let resp = twitch_client.req_get(req, &token).await.unwrap().data;
                    if resp.len() == 0 {
                        conn.execute(
                            "DELETE from embeds WHERE link = ?1 AND timest >= strftime('%s', 'now') - 60",
                            params![format!("#{}/{}", embed.platform, embed.url)]
                        ).unwrap();
                    }
                },
                "twitch-vod" => {
                    match token.validate_token(&twitch_client).await {
                        Err(_) => {
                            token.refresh_token(&twitch_client).await.unwrap();
                        },
                        Ok(_) => {}
                    }
                    let req = get_videos::GetVideosRequest::builder()
                        .id(vec![embed.url.clone().into()]).build();
                    let resp = twitch_client.req_get(req, &token).await.unwrap().data;
                    if resp.len() == 0 {
                        conn.execute(
                            "DELETE from embeds WHERE link = ?1 AND timest >= strftime('%s', 'now') - 60",
                            params![format!("#{}/{}", embed.platform, embed.url)]
                        ).unwrap();
                    }
                },
                "twitch-clip" => {
                    match token.validate_token(&twitch_client).await {
                        Err(_) => {
                            token.refresh_token(&twitch_client).await.unwrap();
                        },
                        Ok(_) => {}
                    }
                    let req = get_clips::GetClipsRequest::builder()
                        .id(vec![embed.url.clone().into()]).build();
                    let resp = twitch_client.req_get(req, &token).await.unwrap().data;
                    if resp.len() == 0 {
                        conn.execute(
                            "DELETE from embeds WHERE link = ?1 AND timest >= strftime('%s', 'now') - 60",
                            params![format!("#{}/{}", embed.platform, embed.url)]
                        ).unwrap();
                    }
                },
                _ => ()
            }
        }
        thread::sleep(Duration::from_secs(60));
    }
}

#[tokio::main]
async fn main() {
    let yaml = load_yaml!("cli.yml");
    let matches = App::from_yaml(yaml)
        .version(crate_version!())
        .about(crate_description!())
        .author(crate_authors!())
        .get_matches();

    let mut log_level = "info";
    if matches.is_present("verbose") {
        log_level = "debug";
    }

    env_logger::init_from_env(
        Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, log_level));
    
    let path = "./data";
    match fs::create_dir_all(path) {
        Ok(_) => (),
        Err(_) => panic!("weow")
    }

    let conn = Connection::open("./data/embeddb.db").unwrap();

    let orig_hook = panic::take_hook();
    panic::set_hook(Box::new(move |panic_info| {
        orig_hook(panic_info);
        process::exit(1);
    }));

    conn.execute(
        "create table if not exists embeds (
             timest integer,
             link text,
             platform text,
             channel text,
             title text
         )",
        NO_PARAMS,
    ).unwrap();

    let regex = Regex::new(r"(^|\s)((#twitch|#twitch-vod|#twitch-clip|#youtube|#youtube-live|(?:https://|http://|)strims\.gg/angelthump)/([A-z0-9_\-]{3,64}))\b").unwrap();

    thread::spawn(|| {
        embed_cleanup();
    });

    loop {
        let ws = connect_async(Url::parse("wss://chat.destiny.gg/ws").unwrap());

        let (socket, response) = match timeout(Duration::from_secs(10), ws).await {
            Ok(ws) => {
                let (socket, response) = match ws {
                    Ok((socket, response)) => {
                        if response.status() != 101 {
                            panic!("Response isn't 101, can't continue.")
                        }
                        (socket, response)
                    },
                    Err(e) => {
                        panic!("Unexpected error: {}", e)
                    }
                };
                (socket, response)
            },
            Err(e) => {
                error!("Connection timed out, restarting the loop: {}", e);
                continue;
            }
        };
        
        info!("Connected to the server");
        debug!("Response HTTP code: {}", response.status());
    
        let (stdin_tx, stdin_rx) = futures_channel::mpsc::unbounded();

        let (timer_tx, timer_rx) = std::sync::mpsc::channel();

        thread::spawn(move || {
            loop {
                match timer_rx.recv_timeout(Duration::from_secs(60)) {
                    Ok(_) => (),
                    Err(e) => panic!("Lost connection, panicking: {}", e)
                }
            }
        });
    
        let (write, mut read) = socket.split();
    
        let stdin_to_ws = stdin_rx.map(Ok).forward(write);
        let ws_to_stdout  = {
            while let Some(msg) = read.next().await {
                let msg_og = match msg {
                    Ok(msg_og) => msg_og,
                    Err(tokio_tungstenite::tungstenite::Error::Io(e)) => {
                        panic!("Tungstenite IO error, panicking: {}", e);
                    },
                    Err(e) => {
                        error!("Some kind of other error occured, restarting the loop: {}", e);
                        continue;
                    }
                };
                timer_tx.send(0).unwrap();
                if msg_og.is_text() {
                    let (msg_type, msg_data) = split_once(msg_og.to_text().unwrap());
                    match msg_type {
                        "MSG" => {
                            let msg_des: Message = serde_json::from_str(&msg_data).unwrap();
                            let capt = regex.captures_iter(msg_des.data.as_str());
                            let mut capt_vector = Vec::new();
                            for result in capt {
                                let full_link = result[2].to_string();
                                if full_link.contains("strims.gg/angelthump") {
                                    capt_vector.push(format!("strims.gg/angelthump/{}", result[4].to_string()));
                                } else {
                                    capt_vector.push(full_link);
                                }
                            }
                            if capt_vector.len() != 0 {
                                capt_vector.dedup();
                                for result in capt_vector {
                                    let mut link = result.to_owned();
                                    let (platform, channel) = result.split_once('/').unwrap();
                                    let platform = if !platform.contains("strims.gg") { &platform[1..] } else { platform };
                                    let mut channel = channel.to_string();
                                    let mut title = "".to_string();
                                    match platform {
                                        "twitch" => {
                                            link = link.to_lowercase();
                                        },
                                        "youtube" => {
                                            let oembed_url = Url::parse_with_params(
                                                OEMBED_URL,
                                                &[("url", format!("https://youtu.be/{}", channel)), ("format", "json".to_string())]
                                            ).unwrap();
                                            let resp = ReqwestGet(oembed_url.as_str())
                                                .await.unwrap();
                                            if resp.status() == 200 {
                                                let oembed_data = resp.json::<YoutubeOEmbed>().await.unwrap();
                                                channel = oembed_data.author_name.to_owned();
                                                title = oembed_data.title.to_owned();
                                            } else {
                                                continue;
                                            }
                                        },
                                        _ => {}
                                    }
                                    conn.execute(
                                        "INSERT INTO embeds (timest, link, platform, channel, title) VALUES (?1, ?2, ?3, ?4, ?5)", 
                                        params![msg_des.timestamp/1000, link, platform, channel, title]
                                    ).unwrap();
                                    debug!("Added embed to db: {}", link);
                                }
                            }
                        },
                        _ => (),
                    }
                }
                if msg_og.is_ping() {
                    stdin_tx.unbounded_send(Pong(msg_og.clone().into_data())).unwrap();
                }
                if msg_og.is_close() {
                    error!("Server closed the connection, restarting the loop.");
                    continue;
                }
            }
            read.into_future()
        };
    
        pin_mut!(stdin_to_ws, ws_to_stdout);
        future::select(stdin_to_ws, ws_to_stdout).await;
    }
}
