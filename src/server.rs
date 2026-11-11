use std::{
    env,
    fmt::format,
    fs,
    io::{BufRead, BufReader, Cursor, Write},
    os::unix::{fs::PermissionsExt, net::UnixStream},
    path::{Path, PathBuf},
    process::{Child, ChildStderr, ChildStdin, Command, Stdio},
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub struct Server {
    _id: u64,
    _child: Child,
    _stdin: ChildStdin,
    _stderr: Option<ChildStderr>,
    output_dir: PathBuf,
    socket_handle: UnixStream,
    mpv_path: PathBuf,
}

#[derive(Serialize)]
struct Request<T: AsRef<str>> {
    id: u64,
    r#type: T,
    query: Option<T>,
    anime_id: Option<u64>,
    episode: Option<u64>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Show {
    pub title: String,
    pub id: u64,
}

#[derive(Deserialize, Debug)]
pub struct Headers {
    #[serde(rename = "Referer")]
    referer: String,
}

#[derive(Deserialize, Debug)]
pub struct Source {
    url: String,
    sub: Option<String>,
    headers: Headers,
    source: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct SourceResult {
    pub id: u64,
    pub ok: bool,
    pub result: Source,
}

#[derive(Deserialize, Debug)]
#[serde(untagged)]
pub enum Response<T> {
    Ok {
        id: Option<u64>,
        ok: bool,
        result: T,
    },
    Err {
        id: Option<u64>,
        ok: bool,
        error: String,
    },
}

static PYTHON: &[u8] = include_bytes!("../assets/python-bin.tar.zst");
static SERVER: &[u8] = include_bytes!("../assets/server.tar.zst");
static MPV: &[u8] = include_bytes!("../assets/mpv.AppImage");

impl Server {
    pub fn init() -> Self {
        let mut output_dir = env::temp_dir();
        output_dir.push(Uuid::new_v4().to_string());

        let mut socket = output_dir.clone();
        socket.push("backend.sock");

        let mut mpv_handle = output_dir.clone();
        mpv_handle.push("mpv");

        let decompressed_tar =
            zstd::decode_all(Cursor::new(PYTHON)).expect("failed to decode archive");
        let cursor = Cursor::new(decompressed_tar);
        let mut archive = tar::Archive::new(cursor);
        archive
            .unpack(output_dir.clone())
            .expect("failed to write to output directory");

        let output_dir_string = output_dir.clone().into_os_string().into_string().unwrap();
        write_server(output_dir_string.clone());

        let server = [output_dir_string.to_string() + "/server"];
        let mut child =
            Command::new(String::from(&output_dir_string) + "/python-bin/bin/python3.13")
                .args(server)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("failed to start server");
        let mut stdi = child.stdin.take().expect("got none for stdin");
        writeln!(stdi, "{}", socket.display()).expect("failed to write to stdin");
        let stdout = child.stdout.take().expect("stdout was piped");
        let _stderr = child.stderr.take();
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .expect("error while reading from the server");

        let socket_handle = UnixStream::connect(socket).expect("failure connecting to socket");

        ensure_mpv(&output_dir);

        Server {
            _id: 1,
            _child: child,
            _stdin: stdi,
            _stderr,
            output_dir: output_dir.clone(),
            socket_handle,
            mpv_path: mpv_handle,
        }
    }

    pub fn search(&mut self, query: impl AsRef<str>) -> Response<Vec<Show>> {
        let query_struct = Request {
            id: self._id,
            r#type: "search",
            query: Some(query.as_ref()),
            anime_id: None,
            episode: None,
        };
        self._id += 1;
        let query =
            serde_json::to_string(&query_struct).expect("failed to make json for search query");
        writeln!(self.socket_handle, "{}", query).expect("failed to write to socket");

        let response_string = self.read_response();
        serde_json::from_str(&response_string).expect("failed to convert response to struct")
    }

    fn read_response(&mut self) -> String {
        let mut read_handle = BufReader::new(
            self.socket_handle
                .try_clone()
                .expect("cant clone socket handle"),
        );
        let mut line = String::new();
        read_handle
            .read_line(&mut line)
            .expect("failed to read response");
        line
    }

    pub fn get_source(&mut self, id: u64, episode: u64) -> SourceResult {
        let request_struct = Request {
            id: self._id,
            r#type: "source",
            query: None,
            anime_id: Some(id),
            episode: Some(episode),
        };
        self._id += 1;
        let request_json = serde_json::to_string(&request_struct).expect("cant create source json");
        writeln!(self.socket_handle, "{}", request_json)
            .expect("cant write to socket for source request");
        let response = self.read_response();
        println!("{response}");
        serde_json::from_str(&response).expect("cant convert response")
    }

    pub fn play_episode(&mut self, source: Source) {
        let mut mpv_args = Vec::new();
        mpv_args.push(source.url);
        if !source.sub.is_none() {
            mpv_args.push(format!("--sub-file={}", source.sub.clone().unwrap()));
        }
        mpv_args.push(format!("--referrer={}", source.headers.referer));
        mpv_args.push("--no-terminal".to_string());

        let mut mpv = Command::new(&self.mpv_path)
            .args(mpv_args)
            .stderr(Stdio::piped())
            .spawn()
            .expect("cant spawn mpv");
        mpv.wait();
    }

    pub fn cleanup(&mut self) {
        fs::remove_dir_all(&self.output_dir).unwrap_or(());
    }
}

fn ensure_mpv(data_dir: &Path) -> std::io::Result<PathBuf> {
    let path = data_dir.join("mpv");
    if !path.exists() {
        let tmp = data_dir.join("mpv.tmp");
        fs::File::create(&tmp)?.write_all(MPV)?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))?;
        fs::rename(&tmp, &path)?; // atomic, so a crash never leaves a half-written binary
    }
    Ok(path)
}

fn write_server(output_dir: impl AsRef<str>) {
    let decompressed_tar = zstd::decode_all(Cursor::new(SERVER)).expect("failed to decode archive");

    // 2. Feed the tar bytes into a Cursor and unpack with the `tar` crate
    let cursor = Cursor::new(decompressed_tar);
    let mut archive = tar::Archive::new(cursor);
    archive
        .unpack(output_dir.as_ref().to_string())
        .expect("failed to write to output direcotry");
}
