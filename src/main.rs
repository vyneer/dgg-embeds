use rusqlite::NO_PARAMS;
use rusqlite::{Connection, Result, params};
use std::fs;
use tungstenite::connect;
use serde::Deserialize;
use url::Url;
use log::{info, debug};
use clap::{load_yaml, crate_authors, crate_description, crate_version, App};
use std::env;
use env_logger::Env;
use regex::Regex;

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

fn main() -> Result<()> {
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

    let conn = Connection::open("./data/embeddb.db")?;

    conn.execute(
        "create table if not exists embeds (
             timest integer,
             link text
         )",
        NO_PARAMS,
    )?;

    let regex = Regex::new(r"(^|\s)((#twitch|#twitch-vod|#twitch-clip|#youtube|#youtube-live)/(?:[A-z0-9_\-]{3,64}))\b").unwrap();

    let (mut socket, response) = match connect(Url::parse("wss://chat.destiny.gg/ws").unwrap()) {
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

    info!("Connected to the server");
    debug!("Response HTTP code: {}", response.status());

    if split_once(socket.read_message().unwrap().to_text().unwrap()).0 != "NAMES" {
        panic!("Couldn't recieve the first message.")
    }

    loop {
        if socket.can_write() {
            let msg_og = socket.read_message().unwrap();
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
                            for result in capt_vector {
                                conn.execute("INSERT INTO embeds (timest, link) VALUES (?1, ?2)", params![msg_des.timestamp/1000, result])?;
                                debug!("Added embed to db: {}", result);
                            }
                        }
                        socket.write_message(tungstenite::Message::Ping("ping".as_bytes().to_vec())).unwrap();
                    },
                    _ => (socket.write_message(tungstenite::Message::Ping("ping".as_bytes().to_vec())).unwrap()),
                }
            }
        }
    }
}
