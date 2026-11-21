use std::{
    collections::HashMap,
    env,
    error::Error,
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    path::Path,
    process::exit,
};

use crate::anilist::Id::{Num, Str};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct AnilistEntry {
    pub progress: u64,
    #[serde(rename = "mediaId")]
    pub media_id: u64,
    pub id: u64,
    pub media: Media,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Media {
    pub episodes: Option<u64>,
    #[serde(rename = "nextAiringEpisode")]
    pub next_airing_episode: Option<NextEpisode>,
    pub title: Title,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Title {
    pub english: String,
    pub romaji: String,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct NextEpisode {
    pub episode: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(untagged)]
pub enum Id {
    Num(u64),
    Str(String),
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
struct SourceIds {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    anichan: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    allanime: Option<Id>,
}

#[derive(Serialize, Deserialize, Debug)]
struct Cache {
    version: f64,
    data: HashMap<String, SourceIds>,
}

#[derive(Clone, Copy, Debug)]
pub enum Source {
    Anichan,
    Allmanga,
}

impl From<u64> for Id {
    fn from(n: u64) -> Self {
        Id::Num(n)
    }
}
impl From<String> for Id {
    fn from(s: String) -> Self {
        Id::Str(s)
    }
}
impl From<&str> for Id {
    fn from(s: &str) -> Self {
        Id::Str(s.to_string())
    }
}

static ANILIST_URL: &str = "https://graphql.anilist.co";
static CLIENT_ID: &str = "28320";
const CACHE_VERSION: f64 = 1.12;
const QUERY: &str = "query Media($userId: Int, $type: MediaType, $status: MediaListStatus) { \
    MediaListCollection(userId: $userId, type: $type, status: $status) { \
        lists { isCustomList entries { id mediaId progress media { \
            episodes nextAiringEpisode { episode } title { english romaji } } } } } }";
const SAVE_MUTATION: &str = "mutation($mediaId: Int, $progress: Int, $status: MediaListStatus, $scoreRaw: Int) {
    SaveMediaListEntry(mediaId: $mediaId, progress: $progress, status: $status, scoreRaw: $scoreRaw) {
        id progress status score
    } }";

pub struct User {
    client: Client,
    token: String,
    id: u64,
    cache_file_handle: Cache, // change it to concrete json type for the cache file format
}

impl User {
    pub fn init() -> Self {
        let mut token = Self::read_token();
        if token.is_none() {
            token = Some(Self::regen_token());
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

        let mut home = env::home_dir().unwrap();
        home.push(".local/share/ani-watch/");
        let cache_file_handle = Cache::load(&home.join("info.txt"));

        User {
            client,
            token,
            id: anilist_id as u64,
            cache_file_handle,
        }
    }
    pub fn get_user_data(&mut self) -> Option<Vec<AnilistEntry>> {
        let body = json!({
            "query": QUERY,
            "variables": {
                "userId": self.id,
                "type": "ANIME",
                "status": "CURRENT",
            }
        });
        let res = self
            .client
            .clone()
            .post(ANILIST_URL)
            .header("Authorization", format!("Bearer {}", &self.token))
            .json(&body)
            .send()
            .expect("request");
        let status_code = res.status();
        if !status_code.is_success() {
            if status_code.is_client_error() {
                println!("Token expired, please rerun the app with cli argument -t");
            } else {
                println!("Server side error, please try again later...");
            }
            exit(1);
        }
        let anilist_resp = res.json::<Value>();
        if anilist_resp.is_err() {
            return None;
        } else {
            let anilist_resp = anilist_resp.unwrap();
            let anilist_data = anilist_resp
                .get("data")
                .and_then(|v| v.get("MediaListCollection"))
                .and_then(|v| v.get("lists"))
                .and_then(Value::as_array)
                .and_then(|v| v.get(0))
                .and_then(|v| v.get("entries"));
            if anilist_data.is_none() {
                return None;
            } else {
                let anilist_data_array =
                    serde_json::from_value::<Vec<AnilistEntry>>(anilist_data.unwrap().clone());
                if anilist_data_array.is_err() {
                    return None;
                } else {
                    return Some(anilist_data_array.unwrap());
                }
            }
        }
    }
    pub fn regen_token() -> String {
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
        Self::write_token(&new_token.trim()).expect("cant write token to file");
        new_token.trim().to_string()
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

    fn write_token<T: AsRef<str>>(token: T) -> Result<(), io::Error> {
        let mut home = env::home_dir().unwrap();
        home.push(".local/share/ani-watch/");

        let mut token_file_handle = OpenOptions::new()
            .create(true)
            .truncate(true)
            .open(home.join("token.txt"))?;
        token_file_handle.write_all(token.as_ref().as_bytes())?;
        Ok(())
    }
    pub fn get_cached_id(&self, id: String) -> Option<u64> {
        self.cache_file_handle.get_id(id)
    }
    pub fn save_cached_id(
        &mut self,
        anilist_id: String,
        source: Source,
        id: impl Into<Id>,
    ) -> Option<()> {
        self.cache_file_handle.set_source_id(anilist_id, source, id)
    }
    pub fn update_progress(
        &mut self,
        entry: &AnilistEntry,
        episode: u64,
    ) -> Result<Update, Box<dyn Error>> {
        // never lower progress (e.g. when rewatching an old episode)
        if episode <= entry.progress {
            return Ok(Update::Unchanged);
        }
        // can't have watched an episode that hasn't aired
        if Self::aired_episodes(entry).is_some_and(|aired| episode > aired) {
            return Err(format!("episode {episode} hasn't aired yet").into());
        }

        let total = entry.media.episodes; // null while the length is unknown
        let completed = total.is_some_and(|t| episode >= t);
        let progress = total.map_or(episode, |t| episode.min(t));

        let client = reqwest::blocking::Client::new();
        let mut vars = json!({ "mediaId": entry.media_id, "progress": progress });
        if completed {
            vars["status"] = json!("COMPLETED");
            if let Some(raw) = Self::prompt_score() {
                vars["scoreRaw"] = json!(raw);
            }
        }
        let token = &self.token.to_string();
        Self::graphql(&client, Some(token), SAVE_MUTATION, vars)?;

        Ok(if completed {
            Update::Completed
        } else {
            Update::Progress
        })
    }

    /// Latest episode that has actually aired, if known. Reuse yours if you already have it.
    fn aired_episodes(e: &AnilistEntry) -> Option<u64> {
        match &e.media.next_airing_episode {
            Some(next) => Some(next.episode.saturating_sub(1)), // airing: next one isn't out yet
            None => e.media.episodes,                           // finished: the total
        }
    }
    fn prompt_score() -> Option<u32> {
        loop {
            print!("Finished! Score it (0-10, blank to skip): ");
            let _ = io::stdout().flush();
            let mut line = String::new();
            match io::stdin().read_line(&mut line) {
                Ok(0) | Err(_) => return None,
                Ok(_) => {}
            }
            let s = line.trim();
            if s.is_empty() {
                return None;
            }
            match s.parse::<f64>() {
                Ok(x) if (0.0..=10.0).contains(&x) => return Some((x * 10.0).round() as u32),
                _ => eprintln!("enter a number between 0 and 10"),
            }
        }
    }
    fn graphql(
        client: &reqwest::blocking::Client,
        token: Option<&str>,
        query: &str,
        variables: Value,
    ) -> Result<Value, Box<dyn Error>> {
        let mut req = client
            .post(ANILIST_URL)
            .header("Accept", "application/json")
            .json(&json!({ "query": query, "variables": variables }));
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        // AniList sends error details in the JSON body even on 4xx, so read the body first
        let body: Value = req.send()?.json()?;
        if let Some(errors) = body.get("errors") {
            let msg = errors[0]["message"]
                .as_str()
                .unwrap_or("unknown AniList error");
            return Err(format!("AniList error: {msg}").into());
        }
        Ok(body["data"].clone())
    }
}

impl Cache {
    fn default() -> Self {
        Self {
            version: CACHE_VERSION,
            data: HashMap::new(),
        }
    }
    fn load(path: &Path) -> Cache {
        let parsed = fs::File::open(path)
            .map_err(|e| e.to_string())
            .and_then(|f| {
                serde_json::from_reader::<_, Cache>(io::BufReader::new(f))
                    .map_err(|e| e.to_string())
            });

        match parsed {
            Ok(c) if c.version == CACHE_VERSION => c,
            Ok(c) => {
                eprintln!(
                    "cache version {} != {CACHE_VERSION}, starting fresh",
                    c.version
                );
                Cache::default()
            }
            Err(e) => {
                eprintln!("cache not loaded ({e}), starting fresh");
                Cache::default()
            }
        }
    }
    fn save(&self, path: &Path) -> io::Result<()> {
        let tmp = path.with_extension("json.tmp");
        serde_json::to_writer_pretty(fs::File::create(&tmp)?, self)?;
        fs::rename(&tmp, path) // atomic: a crash can't leave a half-written cache
    }
    fn get_id(&self, id: String) -> Option<u64> {
        match self.data.get(&id) {
            Some(e) => {
                if !e.anichan.is_none() {
                    let source = e.anichan.clone().unwrap();
                    match source {
                        Num(i) => return Some(i),
                        Str(_) => return None,
                    }
                } else {
                    return None;
                }
            }
            None => return None,
        }
    }
    fn set_source_id(
        &mut self,
        anilist_id: String,
        source: Source,
        id: impl Into<Id>,
    ) -> Option<()> {
        let ids = self.data.entry(anilist_id).or_default();
        let slot = match source {
            Source::Anichan => &mut ids.anichan,
            Source::Allmanga => &mut ids.allanime,
        };
        slot.replace(id.into())?;
        let mut home = env::home_dir().unwrap();
        home.push(".local/share/ani-watch/info.txt");
        self.save(&home);
        Some(())
    }
}

pub enum Update {
    Unchanged, // episode was already counted, nothing sent
    Progress,  // progress saved
    Completed, // progress saved, status set to COMPLETED, score prompted
}
