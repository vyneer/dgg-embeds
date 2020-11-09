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

    let regex = Regex::new(r"(^|\s)((#twitch|#twitch-vod|#twitch-clip|#youtube)/(?:[A-z0-9_\-]{3,64}))\b").unwrap();

    let (mut socket, response) =
        connect(Url::parse("wss://chat.destiny.gg/ws").unwrap()).expect("Can't connect");

    info!("Connected to the server");
    debug!("Response HTTP code: {}", response.status());
    debug!("Response contains the following headers:");
    for (ref header, _value) in response.headers() {
        debug!("* {}", header);
    }

    loop {
        let msg_og = socket.read_message().expect("Error reading message");
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
                },
                _ => (),
            }
        }
    }
}
