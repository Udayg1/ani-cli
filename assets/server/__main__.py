#!/usr/bin/env python3
"""
IPC server for the anichan.to / anikuro.to source backends.

Startup
-------
1. The parent writes ONE line to our stdin: the socket path to bind.
2. We bind + listen, then print a single JSON line to stdout:
       {"event": "ready", "protocol": 1, "socket": "<path>"}
3. stdout is never used again (all logging goes to stderr).
4. When the parent closes our stdin (or dies) the server shuts down and
   removes the socket.

Protocol (newline-delimited JSON, one request -> one response, in order)
------------------------------------------------------------------------
Search by name:
    -> {"id": 1, "type": "search", "query": "frieren"}
    <- {"id": 1, "ok": true, "result": [{"title": "...", "id": 12345}, ...]}

Resolve a playable source URL:
    -> {"id": 2, "type": "source", "anime_id": 12345, "episode": 3}
    <- {"id": 2, "ok": true, "result": {
            "url": "https://.../index.m3u8",   # best-bandwidth variant
            "audio": "https://.../audio.m3u8" | null,  # separate audio track, if the
                                               # stream has one (pass to mpv --audio-file)
            "sub": "https://.../en.vtt" | null,
            "headers": {"Referer": "https://anichan.to/"},
            "source": "anichan" | "fallback:<provider>"}}

Errors:
    <- {"id": 2, "ok": false, "error": "message"}

Dependencies: requests, cryptography
"""

import argparse
import base64
import hashlib
import hmac
import json
import os
import re
import signal
import socketserver
import stat
import sys
import threading
import traceback
from urllib.parse import quote, urljoin

import requests
from cryptography.hazmat.primitives.ciphers.aead import AESGCM

PROTOCOL_VERSION = 1

BASE_URL = "https://anichan.to/"
# Fragment of the JS chunk filename that holds the key material.
# If the site rebuilds and this changes, key extraction will fail.
KEYS_CHUNK_ID = "7621"
FALLBACK_SOURCE = "https://anikuro.to/api/v1/sources/"
FALLBACK_PROVIDERS = ("allanime", "animepahe", "anikoto")
USER_AGENT = "Mozilla/5.0 (X11; Linux x86_64; rv:156.0) Gecko/20100101 Firefox/156.0"
TIMEOUT = 15
MAX_LINE = 1 << 20  # max bytes in a single request line


def log(*args):
    print(*args, file=sys.stderr, flush=True)


class SourceError(Exception):
    """Expected failure that is reported to the client as-is."""


# --------------------------------------------------------------------------
# Key extraction (scraped from the site's JS bundle)
# --------------------------------------------------------------------------


def extract_keys(js: str) -> dict:
    # 1. anchor: the function containing AES-GCM decrypt
    idx = js.find("'AES-GCM'")
    if idx < 0:
        idx = js.find('"AES-GCM"')
    if idx < 0:
        raise ValueError("AES-GCM anchor not found")
    start = js.rfind("async function", 0, idx)
    body = js[start : js.find("catch", idx)]

    # 2. roles: `let n=DEC(CONST1), a=DEC(CONST2)` followed by the XOR loop
    m = re.search(r"(\w+)=(\w+)\((\w+)\),\s*(\w+)=\2\((\w+)\)", body)
    decoder, const1, const2 = m.group(2), m.group(3), m.group(5)

    # 3. localStorage key helper: function that calls getItem(KEY)
    key_name_var = re.search(r"localStorage\.getItem\((\w+)\)", js).group(1)

    def literal(name):
        r = re.search(
            rf'(?:\b(?:const|let|var)\s+|,\s*){name}\s*=\s*([\'"`])(.*?)\1', js
        )
        return r.group(2)

    return {
        "decoder_name": decoder,
        "C": literal(const1),
        "K": literal(const2),
        "storage_key": literal(key_name_var),
    }


def extract_k(js: str):
    m = re.search(r"""['"]X-Wk['"]\s*:\s*(\w+)""", js)
    if not m:
        return None
    var = m.group(1)
    # declaration: `k="..."`, `let k = '...'`, or `,k="..."` in a var list
    d = re.search(
        rf"""(?:\b(?:const|let|var)\s+|[,;{{}}\s]){var}\s*=\s*(['"`])(.*?)\1""", js
    )
    return d.group(2) if d else None


def fetch_keys(session: requests.Session):
    """Returns (C, K, X-Wk) scraped from the site's JS bundle."""
    headers = {"User-Agent": USER_AGENT, "Referer": BASE_URL}
    html = requests.get(BASE_URL, headers=headers, timeout=TIMEOUT).text

    # Match any .js reference (script src, preload hrefs, or paths inside
    # inline Next.js JSON)
    pattern = re.compile(
        r'["\'(]([^"\'()\s]*?/' + re.escape(KEYS_CHUNK_ID) + r'[^"\'()\s/]*?\.js)'
    )
    match = pattern.search(html)
    if not match:
        raise SourceError("could not locate the key script on the site")
    path = match.group(1).replace("\\u002F", "/")

    r = session.get(urljoin(BASE_URL, path), headers=headers, timeout=TIMEOUT)
    r.raise_for_status()
    try:
        keys = extract_keys(r.text)
    except (AttributeError, ValueError) as exc:
        raise SourceError(f"could not extract keys (site JS changed?): {exc}")
    return keys["C"], keys["K"], extract_k(r.text)


# --------------------------------------------------------------------------
# Decryption of the /api/watch/servers payload
# --------------------------------------------------------------------------


def decode_b64(s: str) -> bytes:
    """base64 / base64url with padding fixed up."""
    s = s.replace("-", "+").replace("_", "/")
    return base64.b64decode(s + "=" * (-len(s) % 4))


def decrypt(payload: dict, session_n: str, const_c: str, const_k: str):
    """
    payload: {"i": iv, "d": ciphertext}. Returns parsed JSON, or None on failure.
    """
    if not session_n:
        return None
    try:
        n, i = decode_b64(const_c), decode_b64(const_k)
        xored = bytes(n[j] ^ (i[j] if j < len(i) else 0) for j in range(len(n)))
        key = hmac.new(xored, session_n.encode("utf-8"), hashlib.sha256).digest()
        pt = AESGCM(key).decrypt(
            decode_b64(payload["i"]), decode_b64(payload["d"]), None
        )
        return json.loads(pt.decode("utf-8"))
    except Exception as exc:
        log("decryption failed:", repr(exc))
        return None


# --------------------------------------------------------------------------
# HLS playlist helpers
# --------------------------------------------------------------------------

_ATTR_RE = re.compile(r'([A-Z0-9-]+)=("[^"]*"|[^,]*)')


def parse_attrs(line: str) -> dict:
    """Parse the attribute list of an HLS tag: `#EXT-X-MEDIA:TYPE=AUDIO,URI="a.m3u8"`.
    Quoted values may contain commas, so a plain split(",") is not enough."""
    _, _, rest = line.partition(":")
    return {k: v.strip().strip('"') for k, v in _ATTR_RE.findall(rest)}


# --------------------------------------------------------------------------
# Source client
# --------------------------------------------------------------------------


class SourceClient:
    def __init__(self):
        self.http = requests.Session()
        self.http.headers["User-Agent"] = USER_AGENT
        self._lock = threading.RLock()
        self._ready = False
        self.const_c = ""
        self.const_k = ""
        self.session_n = ""
        self.headers: dict = {}

    # -- session handling --------------------------------------------------

    def ensure_session(self, force: bool = False):
        """Scrape keys + open a watch session. Lazy, and redone on demand."""
        with self._lock:
            if self._ready and not force:
                return
            self._ready = False
            const_c, const_k, wk = fetch_keys(self.http)
            r = self.http.post(
                BASE_URL + "api/watch/session",
                json={"token": ""},
                headers={"Referer": BASE_URL},
                timeout=TIMEOUT,
            )
            r.raise_for_status()
            session_n = r.json()["n"]
            cookie = r.headers["Set-Cookie"].split(";")[0]

            self.const_c, self.const_k, self.session_n = const_c, const_k, session_n
            self.headers = {"Referer": BASE_URL, "Cookie": cookie, "X-Wk": wk}
            self._ready = True

    def _with_session_retry(self, fn):
        """Run fn(); on failure refresh the session once and try again."""
        last = None
        for attempt in range(2):
            try:
                self.ensure_session(force=attempt > 0)
                return fn()
            except (
                requests.RequestException,
                ValueError,
                KeyError,
                SourceError,
            ) as exc:
                last = exc
                log(f"attempt {attempt + 1} failed: {exc!r}")
        raise SourceError(f"source request failed: {last}")

    # -- search ------------------------------------------------------------

    def search(self, query: str) -> list:
        q = quote(" ".join(query.split()), safe="")

        def call():
            r = self.http.get(
                f"{BASE_URL}api/suggest?q={q}", headers=self.headers, timeout=TIMEOUT
            )
            r.raise_for_status()
            return [
                {"title": item["title"], "id": item["id"]}
                for item in r.json()["results"]
            ]

        return self._with_session_retry(call)

    # -- source resolution -------------------------------------------------

    def _fetch_servers(self, anime_id, episode: int) -> list:
        r = self.http.get(
            BASE_URL + "api/watch/servers",
            params={
                "anilistId": str(anime_id),
                "ep": str(episode),
                "cagtegory": "sub",  # (sic) spelled this way in the original script
                "tier": "fast",
            },
            headers=self.headers,
            timeout=TIMEOUT,
        )
        r.raise_for_status()
        data = decrypt(r.json(), self.session_n, self.const_c, self.const_k)
        if not data or "servers" not in data:
            raise SourceError("could not decrypt server list")
        return data["servers"]

    def pick_best_variant(self, url: str) -> dict:
        """
        If `url` is an HLS master playlist, return {"url": <highest-bandwidth variant>,
        "audio": <its separate audio playlist or None>}, both absolute.
        Anything else is returned unchanged with audio None.

        Many streams split audio into its own playlist (#EXT-X-MEDIA:TYPE=AUDIO).
        Once we hand a player the bare variant URL, that audio is lost unless we
        pass it along separately.
        """
        plain = {"url": url, "audio": None}
        with self.http.get(url, stream=True, timeout=TIMEOUT) as r:
            r.raise_for_status()
            head = next(r.iter_content(1 << 20), b"")  # never download a whole video
        text = head.decode("utf-8", "replace")
        if not text.lstrip().startswith("#EXTM3U"):
            return plain

        lines = text.splitlines()
        variants = []  # (bandwidth, uri, audio group id)
        audio_groups = {}  # group id -> [attrs of renditions that have their own URI]
        for i, line in enumerate(lines):
            if line.startswith("#EXT-X-STREAM-INF"):
                attrs = parse_attrs(line)
                uri = next(
                    (
                        l.strip()
                        for l in lines[i + 1 :]
                        if l.strip() and not l.startswith("#")
                    ),
                    None,
                )
                try:
                    bandwidth = int(attrs.get("BANDWIDTH", ""))
                except ValueError:
                    continue
                if uri:
                    variants.append((bandwidth, uri, attrs.get("AUDIO")))
            elif line.startswith("#EXT-X-MEDIA"):
                attrs = parse_attrs(line)
                # a rendition without URI is muxed into the variant: nothing separate to return
                if attrs.get("TYPE") == "AUDIO" and attrs.get("URI"):
                    audio_groups.setdefault(attrs.get("GROUP-ID"), []).append(attrs)
        if not variants:
            return plain

        _, uri, group = max(variants, key=lambda v: v[0])
        audio = None
        renditions = audio_groups.get(group) or []
        if renditions:
            pick = next(
                (a for a in renditions if a.get("DEFAULT") == "YES"),
                next(
                    (a for a in renditions if a.get("AUTOSELECT") == "YES"),
                    renditions[0],
                ),
            )
            audio = urljoin(url, pick["URI"])
        return {"url": urljoin(url, uri), "audio": audio}

    def _anichan_source(self, anime_id, episode: int):
        servers = self._with_session_retry(
            lambda: self._fetch_servers(anime_id, episode)
        )

        candidates = []
        for item in servers:
            stream = item.get("stream")
            if not stream:
                continue
            if (item.get("host") or "").lower() == "anichan":
                continue
            sub = None
            for s in item.get("subtitles") or []:
                lang = s.get("lang", "")
                if "English" in lang and "forced" not in lang.lower():
                    sub = s.get("url")
            candidates.append(
                {"stream": stream, "sub": sub, "rank": float(item["rank"])}
            )
        candidates.sort(key=lambda c: c["rank"])

        for c in candidates:
            try:
                best = self.pick_best_variant(c["stream"])
            except requests.RequestException as exc:
                log(f"variant selection failed for {c['stream']}: {exc!r}")
                continue
            url = best["url"]
            headers = {} if "mp4upload" in url else {"Referer": BASE_URL}
            return {
                "url": url,
                "audio": best["audio"],
                "sub": c["sub"],
                "headers": headers,
                "source": "anichan",
            }
        return None

    def _fallback_source(self, anime_id, episode: int):
        for provider in FALLBACK_PROVIDERS:
            try:
                r = self.http.get(
                    f"{FALLBACK_SOURCE}{provider}/{anime_id}:{episode}",
                    timeout=TIMEOUT,
                )
                if r.status_code != 200:
                    continue
                raw = r.json()["data"]["raw"]["sub"]
                url, headers = raw["originalUrl"], raw["headers"]
                # make sure the URL is actually reachable, without downloading it
                with self.http.get(
                    url, headers=headers, stream=True, timeout=TIMEOUT
                ) as probe:
                    if probe.status_code != 200:
                        continue
                return {
                    "url": url,
                    "audio": None,
                    "sub": None,
                    "headers": headers,
                    "source": f"fallback:{provider}",
                }
            except (requests.RequestException, ValueError, KeyError, TypeError) as exc:
                log(f"fallback {provider} failed: {exc!r}")
        return None

    def get_source(self, anime_id, episode: int) -> dict:
        result = None
        try:
            result = self._anichan_source(anime_id, episode)
        except SourceError as exc:
            log(f"primary source failed: {exc}")
        if result is None:
            result = self._fallback_source(anime_id, episode)
        if result is None:
            raise SourceError("no sources available for this episode")
        return result


SRC = SourceClient()


# --------------------------------------------------------------------------
# IPC layer
# --------------------------------------------------------------------------


def dispatch(raw: bytes) -> dict:
    try:
        req = json.loads(raw)
        if not isinstance(req, dict):
            raise ValueError
    except ValueError:
        return {"id": None, "ok": False, "error": "request must be a JSON object"}

    rid = req.get("id")
    kind = req.get("type")
    try:
        if kind == "search":
            query = req.get("query")
            if not isinstance(query, str) or not query.strip():
                raise SourceError("'query' must be a non-empty string")
            result = SRC.search(query)
        elif kind == "source":
            anime_id = req.get("anime_id")
            if (
                isinstance(anime_id, bool)
                or not isinstance(anime_id, (int, str))
                or not str(anime_id).strip()
            ):
                raise SourceError("'anime_id' must be an int or string")
            try:
                episode = int(req.get("episode"))
            except (TypeError, ValueError):
                raise SourceError("'episode' must be an integer")
            if episode < 1:
                raise SourceError("'episode' must be >= 1")
            result = SRC.get_source(anime_id, episode)
        else:
            raise SourceError(f"unknown request type: {kind!r}")
        return {"id": rid, "ok": True, "result": result}
    except SourceError as exc:
        return {"id": rid, "ok": False, "error": str(exc)}
    except Exception as exc:
        log(traceback.format_exc())
        return {"id": rid, "ok": False, "error": f"internal error: {exc}"}


class Handler(socketserver.StreamRequestHandler):
    def _send(self, obj: dict):
        self.wfile.write(json.dumps(obj).encode("utf-8") + b"\n")
        self.wfile.flush()

    def handle(self):
        try:
            while True:
                line = self.rfile.readline(MAX_LINE + 1)
                if not line:
                    return
                if len(line) > MAX_LINE and not line.endswith(b"\n"):
                    self._send({"id": None, "ok": False, "error": "request too large"})
                    return
                line = line.strip()
                if line:
                    self._send(dispatch(line))
        except OSError:
            pass  # client went away


class Server(socketserver.ThreadingMixIn, socketserver.UnixStreamServer):
    daemon_threads = True


def make_server(path: str) -> Server:
    # remove a stale socket left by a crashed run, but nothing else
    try:
        if stat.S_ISSOCK(os.lstat(path).st_mode):
            os.unlink(path)
    except FileNotFoundError:
        pass
    old_umask = os.umask(0o177)  # socket created as 0600
    try:
        return Server(path, Handler)
    finally:
        os.umask(old_umask)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument(
        "--ignore-stdin-eof",
        action="store_true",
        help="keep running after stdin closes (handy for manual testing)",
    )
    args = ap.parse_args()

    path = sys.stdin.readline().strip()
    if not path:
        log("expected a socket path on the first line of stdin")
        sys.exit(2)

    server = make_server(path)

    def stop(*_):
        threading.Thread(target=server.shutdown, daemon=True).start()

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)

    def watch_stdin():
        try:
            sys.stdin.read()  # blocks until the parent closes the pipe
        except Exception:
            pass
        if not args.ignore_stdin_eof:
            stop()

    threading.Thread(target=watch_stdin, daemon=True).start()

    # warm up the session in the background so the first request is faster
    threading.Thread(target=lambda: _try(SRC.ensure_session), daemon=True).start()

    print(
        json.dumps({"event": "ready", "protocol": PROTOCOL_VERSION, "socket": path}),
        flush=True,
    )
    try:
        server.serve_forever()
    finally:
        server.server_close()
        try:
            os.unlink(path)
        except OSError:
            pass


def _try(fn):
    try:
        fn()
    except Exception as exc:
        log("warm-up failed (will retry on first request):", repr(exc))


if __name__ == "__main__":
    main()
