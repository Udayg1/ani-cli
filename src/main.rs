mod anilist;
mod server;
use std::{
    env,
    io::{self, Write},
    process::exit,
};

use discord_rpc_client::{Client, Event};

use crate::{
    anilist::{
        AnilistEntry,
        Id::{Num, Str},
        Source, Update, User,
    },
    server::{Server, SourceResult},
};
static DISCORD_CLIENT: u64 = 1408296956266025022;

enum State {
    Search {
        chosen: AnilistEntry,
        index: usize,
    },
    Error,
    Playing {
        chosen_id: u64,
        chosen: AnilistEntry,
        cached_resp: Option<SourceResult>,
        index: usize,
    },
    Exit,
    Choose,
}

fn main() {
    let cli_args = env::args().collect::<Vec<String>>();
    for i in cli_args {
        if i.to_ascii_lowercase() == "-t" {
            User::regen_token();
            exit(0);
        }
    }
    let mut drpc = create_drpc();
    let mut server = Server::init();
    let mut current_state = State::Choose;
    let mut anilist = User::init();
    let wrapped_data = anilist.get_user_data();
    let mut data = wrapped_data.unwrap_or(Vec::new());
    loop {
        current_state = match current_state {
            State::Choose => {
                if data.is_empty() {
                    println!(
                        "No anime entries associated with this account, please add some before proceeding.\nTo switch accounts, rerun the program with -t flag defined."
                    );
                    server.cleanup();
                    exit(0)
                }
                for (i, entry) in data.iter().enumerate() {
                    let prog = entry.progress;
                    let media = entry.media.clone();
                    let total_ep = entry.media.episodes.clone();
                    let mut ep_behind = -1;
                    if total_ep.is_none() {
                        let next_air = media.next_airing_episode;
                        if !next_air.is_none() {
                            let next_airing = next_air.unwrap().episode;
                            ep_behind = (next_airing - 1 - prog) as i64;
                        }
                    } else {
                        let total_episodes = total_ep.unwrap();
                        ep_behind = (total_episodes - prog - 1) as i64;
                    }
                    if ep_behind > 0 {
                        println!(
                            "{}. \x1b[32m{} **({} episodes behind)\x1b[0m",
                            i + 1,
                            entry.media.title.english,
                            (ep_behind)
                        )
                    } else {
                        println!("{}. {}", i + 1, entry.media.title.english)
                    }
                }
                println!("\nEnter anime number (0 - exit)");
                let mut input = read_input();
                while input as usize >= data.len() {
                    input = read_input();
                }
                if input == 0 {
                    State::Exit
                } else {
                    let chosen = data.iter().nth((input - 1) as usize).unwrap().clone();
                    let cached_id = anilist.get_cached_id(chosen.media_id.to_string());
                    if cached_id.is_none() {
                        State::Search {
                            chosen: chosen.clone(),
                            index: (input - 1) as usize,
                        }
                    } else {
                        State::Playing {
                            chosen_id: cached_id.unwrap(),
                            chosen: chosen.clone(),
                            cached_resp: None,
                            index: (input - 1) as usize,
                        }
                    }
                }
            }
            State::Search { chosen, index } => {
                let cached_id = anilist.get_cached_id(chosen.media_id.to_string());
                if cached_id.is_none() {
                    let resp = server.search(&chosen.media.title.english);
                    match resp {
                        server::Response::Err {
                            id: _,
                            ok: _,
                            error: _,
                        } => {
                            let new_resp = server.search(&chosen.media.title.romaji);
                            match new_resp {
                                server::Response::Ok {
                                    id: _,
                                    ok: _,
                                    result,
                                } => {
                                    for (index, item) in result.iter().enumerate() {
                                        println!("{}. {}", index + 1, item.title);
                                    }
                                    let mut input = read_input();
                                    while input as usize > result.len() {
                                        input = read_input();
                                    }
                                    if input == 0 {
                                        State::Exit
                                    } else {
                                        let chosen_result = result
                                            .iter()
                                            .nth((input - 1) as usize)
                                            .unwrap()
                                            .clone();
                                        anilist.save_cached_id(
                                            chosen.id.to_string(),
                                            Source::Anichan,
                                            chosen_result.id,
                                        );
                                        State::Playing {
                                            chosen_id: chosen_result.id,
                                            chosen,
                                            cached_resp: None,
                                            index,
                                        }
                                    }
                                }
                                server::Response::Err {
                                    id: _,
                                    ok: _,
                                    error,
                                } => {
                                    println!(
                                        "cant find {} -- {}",
                                        chosen.media.title.romaji, error
                                    );
                                    State::Error
                                }
                            }
                        }
                        server::Response::Ok {
                            id: _,
                            ok: _,
                            result,
                        } => {
                            for (index, item) in result.iter().enumerate() {
                                println!("{}. {}", index + 1, item.title);
                            }
                            let mut input = read_input();
                            while input as usize > result.len() {
                                input = read_input();
                            }
                            if input == 0 {
                                State::Exit
                            } else {
                                let chosen_result =
                                    result.iter().nth((input - 1) as usize).unwrap().clone();
                                anilist.save_cached_id(
                                    chosen.id.to_string(),
                                    Source::Anichan,
                                    chosen_result.id,
                                );
                                State::Playing {
                                    chosen_id: chosen_result.id,
                                    chosen,
                                    cached_resp: None,
                                    index,
                                }
                            }
                        }
                    }
                } else {
                    State::Playing {
                        chosen_id: cached_id.unwrap(),
                        chosen,
                        cached_resp: None,
                        index,
                    }
                }
            }
            State::Exit => {
                server.cleanup();
                break;
            }
            State::Playing {
                chosen_id,
                mut chosen,
                cached_resp,
                index,
            } => {
                let source_res;
                if cached_resp.is_none() {
                    source_res = server.get_source(chosen_id, chosen.progress + 1);
                } else {
                    source_res = cached_resp.unwrap()
                }
                let source = source_res.result;
                println!("\n-> Playing episode {}", chosen.progress + 1);
                let mpv_handle = server.play_episode(source);
                let total_ep = chosen.media.episodes.clone();
                let media = chosen.media.clone();
                let mut total_episodes = 0;
                if total_ep.is_none() {
                    let next_air = media.next_airing_episode;
                    if !next_air.is_none() {
                        total_episodes = next_air.unwrap().episode;
                    }
                } else {
                    total_episodes = total_ep.unwrap();
                }
                if chosen.progress + 2 < total_episodes {
                    let cache_resp = server.get_source(chosen_id, chosen.progress + 2);
                    if !mpv_handle.is_err() {
                        let mpv_status = mpv_handle.unwrap();
                        if !mpv_status.duration.is_none() {
                            let dur = mpv_status.duration.clone().unwrap();
                            if (mpv_status.position / dur) >= 0.90 || mpv_status.reached_end {
                                let episode = chosen.progress + 1;
                                match anilist.update_progress(&chosen, episode) {
                                    Ok(Update::Completed) => {
                                        data.remove(index);
                                    } // no longer CURRENT
                                    Ok(Update::Progress) => {
                                        data[index].progress = episode;
                                        chosen.progress += 1;
                                        println!("-> Progress updated to {episode}");
                                    }
                                    Ok(Update::Unchanged) => {}
                                    Err(e) => eprintln!("couldn't update AniList: {e}"),
                                }
                                State::Playing {
                                    chosen_id,
                                    chosen,
                                    cached_resp: Some(cache_resp),
                                    index,
                                }
                            } else {
                                println!("\n-> Skipping the update\n");
                                State::Choose
                            }
                        } else {
                            println!("\n-> Skipping the update\n");
                            State::Choose
                        }
                    } else {
                        println!("\n-> Skipping the update\n");
                        State::Choose
                    }
                } else {
                    println!("\n-> Skipping the update\n");
                    State::Choose
                }
            }
            State::Error => State::Exit,
        };
    }
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

fn read_input() -> u64 {
    loop {
        print!(">>> ");
        let _ = io::stdout().flush();
        let mut line = String::new();
        let _ = io::stdin().read_line(&mut line);
        match line.trim().parse::<u64>() {
            Ok(n) => return n,
            Err(_) => continue,
        }
    }
}
