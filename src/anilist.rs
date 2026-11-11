use std::{
    collections::HashMap,
    env,
    fs::OpenOptions,
    io::{self, Read},
};

use reqwest::blocking::Client;
use serde_json::Value;

static ANILIST_URL: &str = "https://graphql.anilist.co";
static CLIENT_ID: &str = "28320";

pub struct User {
    client: Client,
    token: String,
    id: u64,
}

impl User {
    pub fn init() -> Self {
        let mut token = read_token();
        if token.is_none() {
            let mut new_token = String::new();
            let auth_url = format!(
                "https://anilist.co/api/v2/oauth/authorize?client_id={}&response_type=token",
                CLIENT_ID
            );
            println!(
                "Open this url in browser: \n{}\n\nAuth token here -> ",
                auth_url
            );
            io::stdin()
                .read_line(&mut new_token)
                .expect("stdin not found");
            token = Some(new_token);
        }
        let token = token.unwrap();
        let client = Client::new();
        let data = HashMap::from([("query", "query {Viewer {id}}")]);
        let res = client
            .post(ANILIST_URL)
            .header("Authorization", format!("Bearer {}", token))
            .json(&data)
            .send()
            .expect("failed to get user data from anilist");
        let res_text = res.json::<Value>().expect("cant make response json");
        let anilist_id = res_text
            .get("data")
            .and_then(|v| v.get("Viewer"))
            .and_then(|v| v.get("id"))
            .and_then(Value::as_i64)
            .expect("anilist id not found");
        User {
            client,
            token,
            id: anilist_id as u64,
        }
    }
}

fn read_token() -> Option<String> {
    let mut token = String::new();
    let mut home = env::home_dir().unwrap();
    home.push(".local/share/ani-watch/");

    let token_file_handle = OpenOptions::new().read(true).open(home.join("token.txt"));

    if token_file_handle.is_err() {
        return None;
    } else {
        let mut token_file_handle = token_file_handle.unwrap();
        token_file_handle
            .read_to_string(&mut token)
            .expect("failed to read token file");
        return Some(token);
    }
}
