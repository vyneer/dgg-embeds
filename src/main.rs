use crossbeam_channel::{Receiver, Sender};
use env_logger::Env;
use futures_util::{future, pin_mut, StreamExt};
use log::{debug, error, info};
use regex::Regex;
use reqwest::{get as ReqwestGet, Client as ReqwestClient};
use rusqlite::{params, Connection};
use serde::Deserialize;
use std::{
    collections::HashMap,
    convert::TryInto,
    fs, panic,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::time::timeout;
use tokio_tungstenite::{connect_async, tungstenite::Message::Pong};
use twitch_api2::helix::{
    clips::get_clips, streams::get_streams, videos::get_videos, ClientRequestError, HelixClient,
    HelixRequestGetError,
};
use twitch_oauth2::{AppAccessToken, ClientId, ClientSecret, TwitchToken};
use url::{ParseError, Url};

#[allow(dead_code)]
#[derive(Deserialize)]
struct Message {
    nick: String,
    features: Vec<String>,
    timestamp: i64,
    data: String,
}

#[derive(Deserialize)]
struct YoutubeOEmbed {
    title: String,
    author_name: String,
}

#[allow(dead_code)]
#[derive(Debug)]
struct CacheEntry {
    timestamp: i64,
    platform: String,
    channel: String,
    title: String,
}

#[derive(Debug)]
enum WebsocketThreadError {
    RefreshToken,
}

enum TimeoutMsg {
    Ok,
    Shutdown,
}

const OEMBED_URL: &str = "https://www.youtube.com/oembed";

fn split_once(in_string: &str) -> (&str, &str) {
    let mut splitter = in_string.splitn(2, ' ');
    let first = splitter.next().unwrap();
    let second = splitter.next().unwrap();
    (first, second)
}

#[tokio::main]
async fn websocket_thread_func(
    regex: Regex,
    token: AppAccessToken,
    twitch_client: HelixClient<ReqwestClient>,
    timer_tx: Sender<Result<TimeoutMsg, WebsocketThreadError>>,
    val_rx: Receiver<u64>,
    ctrlc_inner_rx: Receiver<()>,
    ctrlc_outer_tx: Sender<()>,
    ctrlc_validation_tx: Sender<()>,
) {
    let conn = Connection::open("./data/embeddb.db").unwrap();

    let ws = connect_async(Url::parse("wss://chat.destiny.gg/ws").unwrap());

    let (socket, response) = match timeout(Duration::from_secs(10), ws).await {
        Ok(ws) => {
            let (socket, response) = match ws {
                Ok((socket, response)) => {
                    if response.status() != 101 {
                        panic!("Response isn't 101, can't continue (restarting the thread).")
                    }
                    (socket, response)
                }
                Err(e) => {
                    panic!("Unexpected error, restarting the thread: {}", e)
                }
            };
            (socket, response)
        }
        Err(e) => {
            panic!("Connection timed out, restarting the thread: {}", e);
        }
    };

    info!("Connected to the server");
    debug!("Response HTTP code: {}", response.status());

    let (stdin_tx, stdin_rx) = futures_channel::mpsc::unbounded();

    // lidl cache so as not too spam the apis too much
    // wrapping the hashmap in the arc mutex meme to share between threads
    let cache: Arc<Mutex<HashMap<String, CacheEntry>>> = Arc::new(Mutex::new(HashMap::new()));
    let cache_thread = cache.clone();
    let cache_main = cache.clone();

    // clean the cache every minute
    thread::spawn(move || loop {
        thread::sleep(Duration::from_secs(5));
        cache_thread.lock().unwrap().retain(|_, v| {
            v.timestamp
                > (SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("Time went backwards monkaS")
                    .as_millis()
                    - 60 * 1000)
                    .try_into()
                    .unwrap()
        });
    });

    let mut val_time: u64 = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let (write, mut read) = socket.split();

    // futures/websocket shenanigans
    // i think the next line assumes anything
    // that we send through the stdin_tx channel is Ok,
    // unwraps the inner value and forwards it into the websocket
    let stdin_to_ws = stdin_rx.map(Ok).forward(write);
    let ws_to_stdout = {
        // wait for message and assign it to msg
        while let Some(msg) = read.next().await {
            let msg_og = match msg {
                Ok(msg_og) => msg_og,
                Err(tokio_tungstenite::tungstenite::Error::Io(e)) => {
                    panic!("Tungstenite IO error, restarting the thread: {}", e);
                }
                Err(e) => {
                    panic!(
                        "Some kind of other error occured, restarting the thread: {}",
                        e
                    );
                }
            };
            // send Ok(()) to our timer channel,
            // letting that other thread know we're alive
            timer_tx.send(Ok(TimeoutMsg::Ok)).unwrap();
            // try and receive ctrl-c signal to shutdown
            match ctrlc_inner_rx.try_recv() {
                Ok(_) => {
                    ctrlc_outer_tx.send(()).unwrap();
                    ctrlc_validation_tx.send(()).unwrap();
                    timer_tx.send(Ok(TimeoutMsg::Shutdown)).unwrap();
                    break;
                }
                Err(_) => {}
            }
            // if there's something in the validation channel (should be every 30 minutes)
            // check the token
            match val_rx.try_recv() {
                Ok(n) => {
                    if (n > val_time) && (n - val_time) > (60 * 10 * 3) {
                        val_time = n;
                        match token.validate_token(&twitch_client).await {
                            Err(_) => {
                                timer_tx
                                    .send(Err(WebsocketThreadError::RefreshToken))
                                    .unwrap();
                                panic!("The twitch token has expired, panicking.");
                            }
                            Ok(_) => {}
                        }
                    }
                }
                Err(_) => (),
            }
            if msg_og.is_text() {
                let (msg_type, msg_data) = split_once(msg_og.to_text().unwrap());
                match msg_type {
                    "MSG" => {
                        let msg_des: Message = serde_json::from_str(&msg_data).unwrap();
                        // capture every embed from message
                        let capt = regex.captures_iter(msg_des.data.as_str());
                        let mut capt_vector = Vec::new();
                        // add them all to a vector
                        for result in capt {
                            let full_link = result[2].to_string();
                            if full_link.contains("strims.gg") {
                                let parsed_link_init = Url::parse(full_link.as_str());
                                let parsed_link = match parsed_link_init {
                                    Ok(url) => url,
                                    Err(e) => match e {
                                        ParseError::RelativeUrlWithoutBase => match Url::parse(
                                            format!("https://{}", full_link.as_str()).as_str(),
                                        ) {
                                            Ok(url) => url,
                                            Err(e2) => {
                                                panic!("{}", e2);
                                            }
                                        },
                                        _ => {
                                            panic!("{}", e);
                                        }
                                    },
                                };
                                let parsed_link_path = parsed_link.path();
                                let parsed_link_frags: Vec<&str> =
                                    parsed_link.path_segments().unwrap().collect();
                                match parsed_link_frags.len() {
                                    0 => {}
                                    _ => match parsed_link_frags[0] {
                                        "profile" => {}
                                        "login" => {}
                                        "logout" => {}
                                        "beand" => {}
                                        _ => {
                                            capt_vector
                                                .push(format!("strims.gg{}", parsed_link_path));
                                        }
                                    },
                                }
                            } else {
                                capt_vector.push(full_link);
                            }
                        }
                        if capt_vector.len() != 0 {
                            capt_vector.dedup();
                            'captures: for result in capt_vector {
                                let mut link = result.to_owned();
                                let (platform, channel) = result.split_once('/').unwrap();
                                let platform = if !platform.contains("strims.gg") {
                                    &platform[1..]
                                } else {
                                    platform
                                };
                                let mut channel = channel.to_string();
                                let mut title = "".to_string();
                                // process based on platform
                                match platform {
                                    "twitch" => {
                                        link = link.to_lowercase();
                                        // if not in cache, actually check if the stream is live
                                        if !cache_main.lock().unwrap().contains_key(&link) {
                                            let req = get_streams::GetStreamsRequest::builder()
                                                .user_login(vec![channel.clone().into()])
                                                .build();
                                            let resp = twitch_client.req_get(req, &token).await;
                                            match resp {
                                                Err(e) => {
                                                    match e {
                                                        ClientRequestError::RequestError(e) => {
                                                            error!("{}", e)
                                                        },
                                                        ClientRequestError::HelixRequestGetError(a) => {
                                                            match a {
                                                                HelixRequestGetError::Error {error: _, status, message: _, uri: _} => {
                                                                    match status {
                                                                        reqwest::StatusCode::TOO_MANY_REQUESTS => {
                                                                            error!("Twitch API 429 - Too Many Requests")
                                                                        },
                                                                        reqwest::StatusCode::SERVICE_UNAVAILABLE => {
                                                                            error!("Twitch API 503 - Service Unavailable")
                                                                        },
                                                                        _ => {}
                                                                    }
                                                                },
                                                                _ => {}
                                                            }
                                                        },
                                                        _ => panic!("{}", e)
                                                    }
                                                },
                                                Ok(res) => {
                                                    if res.data.len() != 0 {
                                                        title = res.data.get(0).unwrap().title.clone();
                                                        cache_main.lock().unwrap().insert(link.clone(), CacheEntry {
                                                            timestamp: msg_des.timestamp,
                                                            platform: platform.clone().to_string(),
                                                            channel: channel.clone(),
                                                            title: title.clone()
                                                        });
                                                    } else {
                                                        continue 'captures;
                                                    }
                                                }
                                            }
                                        } else {
                                            title = cache_main
                                                .lock()
                                                .unwrap()
                                                .get(&link)
                                                .unwrap()
                                                .title
                                                .clone();
                                        }
                                    }
                                    "twitch-vod" => {
                                        let req = get_videos::GetVideosRequest::builder()
                                            .id(vec![channel.clone().into()])
                                            .build();
                                        let resp = twitch_client.req_get(req, &token).await;
                                        if !cache_main.lock().unwrap().contains_key(&link) {
                                            match resp {
                                                Err(e) => {
                                                    match e {
                                                        ClientRequestError::RequestError(e) => {
                                                            match e.status().unwrap() {
                                                                reqwest::StatusCode::SERVICE_UNAVAILABLE => {
                                                                    error!("Twitch API 503 - Service Unavailable")
                                                                },
                                                                _ => {}
                                                            }
                                                        },
                                                        ClientRequestError::HelixRequestGetError(a) => {
                                                            match a {
                                                                HelixRequestGetError::Error {error: _, status, message: _, uri: _} => {
                                                                    match status {
                                                                        reqwest::StatusCode::TOO_MANY_REQUESTS => {
                                                                            error!("Twitch API 429 - Too Many Requests")
                                                                        },
                                                                        reqwest::StatusCode::SERVICE_UNAVAILABLE => {
                                                                            error!("Twitch API 503 - Service Unavailable")
                                                                        },
                                                                        _ => {}
                                                                    }
                                                                },
                                                                _ => {}
                                                            }
                                                        },
                                                        _ => panic!("{}", e)
                                                    }
                                                },
                                                Ok(res) => {
                                                    if res.data.len() != 0 {
                                                        title = res.data.get(0).unwrap().title.clone();
                                                    } else {
                                                        continue 'captures;
                                                    }
                                                }
                                            }
                                        } else {
                                            title = cache_main
                                                .lock()
                                                .unwrap()
                                                .get(&link)
                                                .unwrap()
                                                .title
                                                .clone();
                                        }
                                    }
                                    "twitch-clip" => {
                                        let req = get_clips::GetClipsRequest::builder()
                                            .id(vec![channel.clone().into()])
                                            .build();
                                        let resp = twitch_client.req_get(req, &token).await;
                                        if !cache_main.lock().unwrap().contains_key(&link) {
                                            match resp {
                                                Err(e) => {
                                                    match e {
                                                        ClientRequestError::RequestError(e) => {
                                                            match e.status().unwrap() {
                                                                reqwest::StatusCode::SERVICE_UNAVAILABLE => {
                                                                    error!("Twitch API 503 - Service Unavailable")
                                                                },
                                                                _ => {}
                                                            }
                                                        },
                                                        ClientRequestError::HelixRequestGetError(a) => {
                                                            match a {
                                                                HelixRequestGetError::Error {error: _, status, message: _, uri: _} => {
                                                                    match status {
                                                                        reqwest::StatusCode::TOO_MANY_REQUESTS => {
                                                                            error!("Twitch API 429 - Too Many Requests")
                                                                        },
                                                                        reqwest::StatusCode::SERVICE_UNAVAILABLE => {
                                                                            error!("Twitch API 503 - Service Unavailable")
                                                                        },
                                                                        _ => {}
                                                                    }
                                                                },
                                                                _ => {}
                                                            }
                                                        },
                                                        _ => panic!("{}", e)
                                                    }
                                                },
                                                Ok(res) => {
                                                    if res.data.len() != 0 {
                                                        title = res.data.get(0).unwrap().title.clone();
                                                    } else {
                                                        continue 'captures;
                                                    }
                                                }
                                            }
                                        } else {
                                            title = cache_main
                                                .lock()
                                                .unwrap()
                                                .get(&link)
                                                .unwrap()
                                                .title
                                                .clone();
                                        }
                                    }
                                    "youtube" => {
                                        if !cache_main.lock().unwrap().contains_key(&link) {
                                            let oembed_url = Url::parse_with_params(
                                                OEMBED_URL,
                                                &[
                                                    (
                                                        "url",
                                                        format!("https://youtu.be/{}", channel),
                                                    ),
                                                    ("format", "json".to_string()),
                                                ],
                                            )
                                            .unwrap();
                                            match ReqwestGet(oembed_url.as_str()).await {
                                                Ok(resp) => {
                                                    if resp.status() == 200 {
                                                        let oembed_data = resp
                                                            .json::<YoutubeOEmbed>()
                                                            .await
                                                            .unwrap();
                                                        channel =
                                                            oembed_data.author_name.to_owned();
                                                        title = oembed_data.title.to_owned();
                                                        cache_main.lock().unwrap().insert(
                                                            link.clone(),
                                                            CacheEntry {
                                                                timestamp: msg_des.timestamp,
                                                                platform: platform
                                                                    .clone()
                                                                    .to_string(),
                                                                channel: channel.clone(),
                                                                title: title.clone(),
                                                            },
                                                        );
                                                    } else {
                                                        continue 'captures;
                                                    }
                                                }
                                                Err(e) => {
                                                    error!("{}", e);
                                                }
                                            };
                                        } else {
                                            title = cache_main
                                                .lock()
                                                .unwrap()
                                                .get(&link)
                                                .unwrap()
                                                .title
                                                .clone();
                                        }
                                    }
                                    _ => {}
                                }
                                conn.execute(
                                    "INSERT INTO embeds (timest, link, platform, channel, title) VALUES (?1, ?2, ?3, ?4, ?5)", 
                                    params![msg_des.timestamp/1000, link, platform, channel, title]
                                ).unwrap();
                                debug!("Added embed to db: {}", link);
                            }
                        }
                    }
                    _ => (),
                }
            }
            if msg_og.is_ping() {
                stdin_tx
                    .unbounded_send(Pong(msg_og.clone().into_data()))
                    .unwrap();
            }
            if msg_og.is_close() {
                panic!("Server closed the connection, restarting the thread.");
            }
        }
        read.into_future()
    };

    pin_mut!(stdin_to_ws, ws_to_stdout);
    future::select(stdin_to_ws, ws_to_stdout).await;
}

#[tokio::main]
async fn main() {
    dotenv::dotenv().ok();
    let log_level = std::env::var("DEBUG")
        .ok()
        .map(|val| match val.as_str() {
            "0" | "false" | "" => "info",
            "1" | "true" => "debug",
            _ => panic!("Please set the DEBUG env correctly."),
        })
        .unwrap();
    let twitch_client: HelixClient<ReqwestClient> = HelixClient::default();
    let client_id = std::env::var("TWITCH_CLIENT_ID")
        .ok()
        .map(ClientId::new)
        .expect("Please set env: TWITCH_CLIENT_ID");
    let secret = std::env::var("TWITCH_CLIENT_SECRET")
        .ok()
        .map(ClientSecret::new)
        .expect("Please set env: TWITCH_CLIENT_SECRET");
    let mut token = AppAccessToken::get_app_access_token(&twitch_client, client_id, secret, vec![])
        .await
        .unwrap();

    env_logger::Builder::from_env(
        Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, log_level),
    )
    .format_timestamp_millis()
    .init();

    // making panics look nicer
    panic::set_hook(Box::new(move |panic_info| {
        if let Some(s) = panic_info.payload().downcast_ref::<WebsocketThreadError>() {
            error!(target: thread::current().name().unwrap(), "Panicked on a custom error: {:?}", s);
        } else {
            error!(target: thread::current().name().unwrap(), "{}", panic_info);
        }
    }));

    let path = "./data";
    match fs::create_dir_all(path) {
        Ok(_) => (),
        Err(_) => panic!("Couldn't create a 'data' folder, not sure what went wrong, panicking."),
    }

    let conn = Connection::open("./data/embeddb.db").unwrap();

    conn.execute(
        "create table if not exists embeds (
             timest integer,
             link text,
             platform text,
             channel text,
             title text
         )",
        [],
    )
    .unwrap();

    conn.close().unwrap();

    let regex = Regex::new(r"(^|\s)((#twitch|#twitch-vod|#twitch-clip|#youtube|(?:https://|http://|)strims\.gg(?:/angelthump|/facebook|/smashcast|/twitch-vod|/twitch|/ustream|/youtube-playlist|/youtube)?)/([A-z0-9_\-]{3,64}))\b").unwrap();

    let sleep_timer = Arc::new(AtomicU64::new(0));
    let refresh_bool = Arc::new(AtomicBool::new(false));
    let (ctrlc_outer_tx, ctrlc_outer_rx): (Sender<()>, Receiver<()>) =
        crossbeam_channel::unbounded();

    'outer: loop {
        let (ctrlc_validation_tx, ctrlc_validation_rx): (Sender<()>, Receiver<()>) =
            crossbeam_channel::unbounded();
        let cloned_ctrlc_outer_tx_ws = ctrlc_outer_tx.clone();

        match ctrlc_outer_rx.try_recv() {
            Ok(_) => {
                break 'outer;
            }
            Err(_) => {}
        }

        match ctrlc_validation_rx.try_recv() {
            Ok(_) => {
                break 'outer;
            }
            Err(_) => {}
        }

        let sleep_timer_inner = Arc::clone(&sleep_timer);
        let regex = regex.clone();
        let client_id = std::env::var("TWITCH_CLIENT_ID")
            .ok()
            .map(ClientId::new)
            .expect("Please set env: TWITCH_CLIENT_ID");
        let secret = std::env::var("TWITCH_CLIENT_SECRET")
            .ok()
            .map(ClientSecret::new)
            .expect("Please set env: TWITCH_CLIENT_SECRET");
        if refresh_bool.load(Ordering::Relaxed) {
            sleep_timer.store(0, Ordering::Release);
            token = AppAccessToken::get_app_access_token(&twitch_client, client_id, secret, vec![])
                .await
                .unwrap();
            refresh_bool.store(false, Ordering::Relaxed);
        }
        let refresh_bool_clone = Arc::clone(&refresh_bool);
        let token = token.clone();
        let twitch_client = twitch_client.clone();
        // timeout channels
        let (timer_tx, timer_rx): (
            Sender<Result<TimeoutMsg, WebsocketThreadError>>,
            Receiver<Result<TimeoutMsg, WebsocketThreadError>>,
        ) = crossbeam_channel::unbounded();
        // twitch access token validation channels
        // creating them with the sync_channel function so whatever we send wont get buffered
        let (val_tx, val_rx): (Sender<u64>, Receiver<u64>) = crossbeam_channel::bounded(1);
        let (ctrlc_inner_tx, ctrlc_inner_rx): (Sender<()>, Receiver<()>) =
            crossbeam_channel::unbounded();

        match sleep_timer.load(Ordering::Acquire) {
            0 => {}
            1 => info!(
                "One of the threads panicked, restarting in {} second",
                sleep_timer.load(Ordering::Acquire)
            ),
            _ => info!(
                "One of the threads panicked, restarting in {} seconds",
                sleep_timer.load(Ordering::Acquire)
            ),
        }
        thread::sleep(Duration::from_secs(sleep_timer.load(Ordering::Acquire)));

        // twitch access token validation thread
        // every 30 minutes sends a () thru a channel
        // signaling to validate the token
        let twitch_validation_thread = thread::Builder::new()
            .name("twitch_validation_thread".to_string())
            .spawn(move || loop {
                match ctrlc_validation_rx.try_recv() {
                    Ok(_) => {
                        break;
                    },
                    Err(_) => {
                        match val_tx.send(SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()) {
                            Ok(_) => {}
                            Err(e) => panic!("Got a send error in the validation thread, this shouldn't happen, panicking: {}", e),
                        }
                    }
                }
                thread::sleep(Duration::from_secs(5));
            }).unwrap();

        // this thread checks for the timeouts in the websocket thread
        // if there's nothing in the ws for a minute, panic
        let timeout_thread = thread::Builder::new()
            .name("timeout_thread".to_string())
            .spawn(move || loop {
                match timer_rx.recv_timeout(Duration::from_secs(60)) {
                    Ok(a) => match a {
                        Ok(m) => match m {
                            TimeoutMsg::Ok => {
                                if sleep_timer_inner.load(Ordering::Acquire) != 0 {
                                    sleep_timer_inner.store(0, Ordering::Release)
                                }
                            }
                            TimeoutMsg::Shutdown => {
                                break;
                            }
                        },
                        Err(e) => match e {
                            WebsocketThreadError::RefreshToken => {
                                refresh_bool_clone.store(true, Ordering::Relaxed);
                            }
                        },
                    },
                    Err(e) => {
                        panic!("Lost connection, terminating the timeout thread: {}", e);
                    }
                }
            })
            .unwrap();

        // the main websocket thread that does all the hard work
        let ws_thread = thread::Builder::new()
            .name("websocket_thread".to_string())
            .spawn(move || {
                websocket_thread_func(
                    regex,
                    token,
                    twitch_client,
                    timer_tx,
                    val_rx,
                    ctrlc_inner_rx,
                    cloned_ctrlc_outer_tx_ws,
                    ctrlc_validation_tx,
                )
            })
            .unwrap();

        ctrlc::set_handler(move || {
            ctrlc_inner_tx.send(()).unwrap();
        })
        .expect("Error setting Ctrl-C handler");

        match timeout_thread.join() {
            Ok(_) => {}
            Err(_) => {
                match sleep_timer.load(Ordering::Acquire) {
                    0 => sleep_timer.store(1, Ordering::Release),
                    1..=16 => sleep_timer
                        .store(sleep_timer.load(Ordering::Acquire) * 2, Ordering::Release),
                    _ => {}
                }
                continue 'outer;
            }
        }
        match twitch_validation_thread.join() {
            Ok(_) => {}
            Err(_) => {
                match sleep_timer.load(Ordering::Acquire) {
                    0 => sleep_timer.store(1, Ordering::Release),
                    1..=16 => sleep_timer
                        .store(sleep_timer.load(Ordering::Acquire) * 2, Ordering::Release),
                    _ => {}
                }
                continue 'outer;
            }
        }
        match ws_thread.join() {
            Ok(_) => {}
            Err(_) => {
                match sleep_timer.load(Ordering::Acquire) {
                    0 => sleep_timer.store(1, Ordering::Release),
                    1..=16 => sleep_timer
                        .store(sleep_timer.load(Ordering::Acquire) * 2, Ordering::Release),
                    _ => {}
                }
                continue 'outer;
            }
        }
    }
}
