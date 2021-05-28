use rusqlite::NO_PARAMS;
use rusqlite::{Connection, params};
use std::fs;
use tokio_tungstenite::{connect_async, tungstenite::Message::Pong};
use serde::Deserialize;
use url::Url;
use log::{info, debug};
use clap::{load_yaml, crate_authors, crate_description, crate_version, App};
use std::env;
use env_logger::Env;
use regex::Regex;
use futures_util::{future, pin_mut, StreamExt};
use tokio::time::timeout;
use std::time::Duration;

#[derive(Deserialize)]
struct Message {
    data: String,
    timestamp: i64,
}

fn split_once(in_string: &str) -> (&str, &str) {
    let mut splitter = in_string.splitn(2, ' ');
    let first = splitter.next().unwrap();
    let second = splitter.next().unwrap();
    (first, second)
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

    conn.execute(
        "create table if not exists embeds (
             timest integer,
             link text
         )",
        NO_PARAMS,
    ).unwrap();

    let regex = Regex::new(r"(^|\s)((#twitch|#twitch-vod|#twitch-clip|#youtube|#youtube-live)/(?:[A-z0-9_\-]{3,64}))\b").unwrap();

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
        Err(_) => panic!("Connection timed out, panicking.")
    };
    
    info!("Connected to the server");
    debug!("Response HTTP code: {}", response.status());

    let (stdin_tx, stdin_rx) = futures_channel::mpsc::unbounded();

    let (write, read) = socket.split();

    let stdin_to_ws = stdin_rx.map(Ok).forward(write);
    let ws_to_stdout  = {
        read.for_each(|msg| async {
            let msg_og = match msg {
                Ok(msg_og) => msg_og,
                Err(tokio_tungstenite::tungstenite::Error::Io(e)) => {
                    panic!("Tungstenite IO error, panicking: {}", e);
                },
                Err(e) => {
                    panic!("Some kind of other error occured, panicking: {}", e);
                }
            };
            if msg_og.is_text() {
                let (msg_type, msg_data) = split_once(msg_og.to_text().unwrap());
                match msg_type {
                    "MSG" => {
                        let msg_des: Message = serde_json::from_str(&msg_data).unwrap();
                        let capt = regex.captures_iter(msg_des.data.as_str());
                        let mut capt_vector = Vec::new();
                        for result in capt {
                            capt_vector.push(result[2].to_string());
                        }
                        if capt_vector.len() != 0 {
                            capt_vector.dedup();
                            for mut result in capt_vector {
                                if result.contains("#twitch/") {
                                    result = result.to_lowercase();
                                }
                                conn.execute("INSERT INTO embeds (timest, link) VALUES (?1, ?2)", params![msg_des.timestamp/1000, result]).unwrap();
                                debug!("Added embed to db: {}", result);
                            }
                        }
                    },
                    _ => (),
                }
            }
            if msg_og.is_ping() {
                debug!("{:?}", Pong(msg_og.clone().into_data()));
                stdin_tx.unbounded_send(Pong(msg_og.clone().into_data())).unwrap();
            }
            if msg_og.is_close() {
                panic!("Server closed the connection, panicking.")
            }
        })
    };

    /*thread::spawn(move || {
        stdin_tx.unbounded_send(Ping("ping".as_bytes().to_vec())).unwrap();

        thread::sleep(Duration::from_secs(5));
    }); */

    pin_mut!(stdin_to_ws, ws_to_stdout);
    future::select(stdin_to_ws, ws_to_stdout).await;
}
