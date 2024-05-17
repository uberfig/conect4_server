// lots of stuff from: https://github.com/tokio-rs/axum/blob/main/examples/websockets/Cargo.toml
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::IntoResponse,
    routing::get,
    Router,
};
use axum_extra::TypedHeader;
use tokio::sync::Mutex;

// use std::ops::ControlFlow;
use std::{
    // borrow::{BorrowMut, Cow},
    mem,
    sync::Arc,
};
use std::{net::SocketAddr, path::PathBuf};
use tower_http::{
    services::ServeDir,
    trace::{DefaultMakeSpan, TraceLayer},
};

use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

//allows to extract the IP of connecting user
use axum::extract::connect_info::ConnectInfo;
// use axum::extract::ws::CloseFrame;

//allows to split the websocket stream into separate TX and RX branches
// use futures::{sink::SinkExt, stream::StreamExt};

struct Client {
    socket: WebSocket,
    who: SocketAddr,
    uname: String,
}

struct MatchMaking {
    data: Mutex<Option<Client>>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "example_websockets=debug,tower_http=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let assets_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets");

    let shared_state = Arc::new(MatchMaking {
        data: Mutex::new(None),
    });

    // build our application with some routes
    let app = Router::new()
        .fallback_service(ServeDir::new(assets_dir).append_index_html_on_directories(true))
        .route("/ws", get(ws_handler))
        .with_state(shared_state)
        // logging so we can see whats going on
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::default().include_headers(true)),
        );

    // run it with hyper
    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000")
        .await
        .unwrap();
    tracing::debug!("listening on {}", listener.local_addr().unwrap());
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .unwrap();
}

/// The handler for the HTTP request (this gets called when the HTTP GET lands at the start
/// of websocket negotiation). After this completes, the actual switching from HTTP to
/// websocket protocol will occur.
/// This is the last point where we can extract TCP/IP metadata such as IP address of the client
/// as well as things from HTTP headers such as user-agent of the browser etc.
async fn ws_handler(
    State(state): State<Arc<MatchMaking>>,
    ws: WebSocketUpgrade,
    user_agent: Option<TypedHeader<headers::UserAgent>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> impl IntoResponse {
    let user_agent = if let Some(TypedHeader(user_agent)) = user_agent {
        user_agent.to_string()
    } else {
        String::from("Unknown browser")
    };
    println!("`{user_agent}` at {addr} connected.");
    // finalize the upgrade process by returning upgrade callback.
    // we can customize the callback by sending additional info such as address.
    ws.on_upgrade(move |socket| handle_socket(socket, addr, state))
}

const BOARD_WIDTH: usize = 7;
const BOARD_HEIGHT: usize = 6;
pub struct Game {
    pub board_xy: [[u8; BOARD_WIDTH]; BOARD_HEIGHT],
}

fn flip_player(player: usize) -> usize {
    if player == 1 {
        return 0;
    }
    1
}

use rand::Rng;

/// Actual websocket statemachine (one will be spawned per connection)
async fn handle_socket(mut socket: WebSocket, who: SocketAddr, state: Arc<MatchMaking>) {
    if socket
        .send(Message::Text("enter nickname:".to_owned()))
        .await
        .is_ok()
    {
    } else {
        println!("Could not send ping {who}!");
        // no Error here since the only thing we can do is to close the connection.
        // If we can not send messages, there is no way to salvage the statemachine anyway.
        return;
    }

    let uname: String;

    loop {
        if let Some(msg) = socket.recv().await {
            if let Ok(msg) = msg {
                if let Message::Text(x) = msg {
                    uname = x[..3].to_owned().to_uppercase();
                    break;
                }
            } else {
                println!("client {who} abruptly disconnected");
                return;
            }
        }

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }

    let peer = {
        let mut test = state.data.lock().await;

        let data = mem::replace(&mut *test, None);

        match data {
            Some(x) => x,
            None => {
                if socket
                    .send(Message::Text("matchmaking".to_owned()))
                    .await
                    .is_err()
                {
                    return;
                }

                let my_client = Client { socket, who, uname };
                *test = Some(my_client);

                return;
            }
        }
    };

    let mut players = [Client { socket, who, uname }, peer];

    for i in 0..players.len() {
        let match_name = players[flip_player(i)].uname.clone();
        let r = send_message(&mut players, i, format!("matched against: {match_name}")).await;
        if r.is_err() {
            return;
        }
    }

    let first: usize = {
        let mut rng = rand::thread_rng();

        rng.gen_range(0..1)
    };

    let r = send_message(&mut players, first, format!("you are player:1")).await;
    if r.is_err() {
        return;
    }

    let r = send_message(
        &mut players,
        flip_player(first),
        format!("you are player:2"),
    )
    .await;
    if r.is_err() {
        return;
    }

    let mut board_xy: [[u8; BOARD_WIDTH]; BOARD_HEIGHT] = [[0; BOARD_WIDTH]; BOARD_HEIGHT];
    let mut current_turn = first;

    loop {
        let r = send_message(
            &mut players,
            current_turn,
            format!("which column do you want to place (0-6)"),
        )
        .await;
        if r.is_err() {
            return;
        }

        let result = get_response(&mut players, current_turn).await;
        match result {
            Ok(x) => {
                let choice = x.parse::<usize>();
                match choice {
                    Ok(x) => {
                        let allowed = place(&mut board_xy, x, current_turn);
                        if allowed {
                            let r = send_both(
                                &mut players,
                                format!("player:{current_turn} placement:{x}"),
                            )
                            .await;
                            if r.is_err() {
                                return;
                            }
                            let did_win = check_win(&board_xy, current_turn);
                            if did_win {
                                let nickname = players[current_turn].uname.clone();
                                let _ = send_both(&mut players, format!("winner! player:{nickname}")).await;
                                return;
                            }
                            current_turn = flip_player(current_turn);
                        } 

                        else {
                            let r = send_message(
                                &mut players,
                                current_turn,
                                format!("invalid placement"),
                            )
                            .await;
                            if r.is_err() {
                                return;
                            }
                        }
                    }
                    Err(_) => {
                        let r = send_message(&mut players, current_turn, format!("invalid input"))
                            .await;
                        if r.is_err() {
                            return;
                        }
                    }
                }
            }
            Err(_) => return,
        }
    }
}

async fn send_message(players: &mut [Client; 2], player: usize, message: String) -> Result<(), ()> {
    if players[player]
        .socket
        .send(Message::Text(message))
        .await
        .is_err()
    {
        let who = players[player].who;
        println!("client {who} abruptly disconnected");
        let _ = players[flip_player(player)]
            .socket
            .send(Message::Text("peer disconnected".to_owned()))
            .await;
        return Err(());
    }
    Ok(())
}

async fn send_both(players: &mut [Client; 2], message: String) -> Result<(), ()> {
    let _ = send_message(players, 0, message.clone()).await?;
    let _ = send_message(players, 1, message).await?;
    Ok(())
}

async fn get_response(players: &mut [Client; 2], player: usize) -> Result<String, ()> {
    loop {
        if let Some(msg) = players[player].socket.recv().await {
            if let Ok(msg) = msg {
                if let Message::Text(x) = msg {
                    return Ok(x);
                }
            } else {
                let who = players[player].who;
                println!("client {who} abruptly disconnected");
                let _ = players[flip_player(player)]
                    .socket
                    .send(Message::Text("peer disconnected".to_owned()))
                    .await;
                return Err(());
            }
        }

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
}

/// trys to place and returns true if successful
fn place(board_xy: &mut [[u8; BOARD_WIDTH]; BOARD_HEIGHT], x: usize, player: usize) -> bool {
    let player = (player + 1) as u8;
    if x > 6 {
        return false;
    }
    let mut can_place = false;
    for i in 0..board_xy[0].len() {
        if board_xy[x][i] == 0 {
            board_xy[x][i] = player;
            can_place = true;
            break;
        }
    }
    return can_place;
}

fn check_win(board_xy: &[[u8; BOARD_WIDTH]; BOARD_HEIGHT], player: usize) -> bool {
    let player = (player + 1) as u8;
    //check vertically
    for x in 0..board_xy.len() {
        let mut contigous_len = 0;
        let mut in_contigous = false;

        for y in 0..board_xy[0].len() {
            if board_xy[x][y] == player {
                if !in_contigous {
                    in_contigous = true;
                }

                contigous_len += 1;
                if contigous_len >= 4 {
                    return true;
                }
            } else {
                in_contigous = false;
                contigous_len = 0;
            }
        }
    }

    //check horizontally
    for y in 0..board_xy[0].len() {
        let mut contigous_len = 0;
        let mut in_contigous = false;

        for x in 0..board_xy.len() {
            if board_xy[x][y] == player {
                if !in_contigous {
                    in_contigous = true;
                }

                contigous_len += 1;
                if contigous_len >= 4 {
                    return true;
                }
            } else {
                in_contigous = false;
                contigous_len = 0;
            }
        }
    }

    //check diagonally from bottom left to top right
    for x in 0..board_xy.len() {
        let mut in_contigous = false;

        for y in 0..board_xy[0].len() {
            let mut check_x = x;
            let mut check_y = y;

            let mut contigous_len = 0;
            while check_x < board_xy.len() && check_y < board_xy[0].len() {
                if board_xy[check_x][check_y] == player {
                    if !in_contigous {
                        in_contigous = true;
                    }
                    contigous_len += 1;

                    if contigous_len >= 4 {
                        return true;
                    }
                } else {
                    in_contigous = false;
                    contigous_len = 0;
                }

                check_x += 1;
                check_y += 1;
            }
        }
    }

    //check diagonally from bottom right to top left
    for x in 0..board_xy.len() as isize {
        let mut in_contigous = false;

        for y in 0..board_xy[0].len() {
            let mut check_x = x;
            let mut check_y = y;

            let mut contigous_len = 0;
            while check_x >= 0 && check_y < board_xy[0].len() {
                if board_xy[check_x as usize][check_y] == player {
                    if !in_contigous {
                        in_contigous = true;
                    }
                    contigous_len += 1;

                    if contigous_len >= 4 {
                        return true;
                    }
                } else {
                    in_contigous = false;
                    contigous_len = 0;
                }

                check_x -= 1;
                check_y += 1;
            }
        }
    }

    return false;
}
