mod anilist;
mod server;
use std::io::{self, Write};

use discord_rpc_client::{Client, Event};

use crate::{anilist::User, server::Server};
static DISCORD_CLIENT: u64 = 1408296956266025022;

#[derive(PartialEq)]
enum State {
    Search,
    Error,
    Playing,
}

fn main() {
    let mut drpc = create_drpc();
    let mut server = Server::init();
    let mut current_state = State::Search;
    let anilist = User::init();
    while current_state == State::Search {
        let resp = server.search("overlord");
        let mut chosen: server::Show;
        match resp {
            server::Response::Ok {
                id: _,
                ok: _,
                result,
            } => {
                let mut index = 0;
                for item in &result {
                    println!("{}. {}", index + 1, item.title);
                    index += 1;
                }
                let mut choice = String::from("");
                loop {
                    let parsed_choice = choice.trim().parse::<usize>();
                    if parsed_choice.is_err() {
                        choice.clear();
                        print!(">>> ");
                        io::stdout().flush().expect("no stdout");
                        io::stdin().read_line(&mut choice).expect("no stdin");
                    } else {
                        let indexed = result.iter().nth(parsed_choice.unwrap() - 1);
                        if indexed.is_none() {
                            choice.clear();
                            continue;
                        } else {
                            chosen = indexed.unwrap().clone();
                            break;
                        }
                    }
                }
            }
            server::Response::Err {
                id: _,
                ok: _,
                error,
            } => {
                println!("failed to get response {error}");
                continue;
            }
        }

        let result = server.get_source(chosen.id, 1);
        println!("{result:?}");
        server.play_episode(result.result);
    }

    server.cleanup();
}

fn write_drpc(drpc: &mut Client, message: String) -> Result<(), discord_rpc_client::Error> {
    drpc.set_activity(|act| act.state(message))?;
    Ok(())
}

fn create_drpc() -> Client {
    let mut drpc = Client::new(DISCORD_CLIENT);
    drpc.on_event(Event::Ready, |_| {});
    drpc.on_event(Event::Error, |handle| println!("{handle:?}"));
    drpc.start();
    drpc
}
