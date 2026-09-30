//! Jellyfin source plugin for ricercar (plugin protocol 1).
//!
//! Speaks JSON-RPC over stdin/stdout with the player, and the Jellyfin REST
//! API with the user's server. Tracks play from the original file (bit for
//! bit) unless the DAC cannot take its rate or depth, in which case Jellyfin
//! transcodes to FLAC at a rate it does take (see the `transcode` setting).
//!
//! Options:
//!   --server URL   prefill the server address on the sign-in page

mod items;
mod jellyfin;
mod login;

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};

use serde_json::{Value, json};

use items::{Output, Plan};
use jellyfin::{Client, Error, Session};

const PROTOCOL: u64 = 1;
const PAGE: u64 = 200;
/// Fields asked for with every item list.
const FIELDS: &str = "MediaStreams,Genres,ChildCount,ProductionYear,Studios";
/// Items per `related` shelf of `item.details`.
const SHELF: u64 = 12;
/// Ids per request when adding to or removing from a playlist: the query
/// string stays well under the server's limit.
const BATCH: usize = 100;

/// `<n>` random bytes from the kernel, as hex.
pub fn random_hex(n: usize) -> String {
    let mut buf = vec![0u8; n];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut buf);
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

struct RpcError {
    code: i64,
    message: String,
}

fn rpc_err(code: i64, message: impl Into<String>) -> RpcError {
    RpcError {
        code,
        message: message.into(),
    }
}

type Reply = Result<Value, RpcError>;

/// The values of the settings this plugin declares.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Settings {
    /// Send `playback.*` to the server.
    report: bool,
    /// Let the server transcode when the DAC cannot take a file.
    transcode: bool,
}

impl Settings {
    /// From `{key: value}`; missing or unknown values take the defaults.
    fn from_json(v: &Value) -> Settings {
        Settings {
            report: v["report_playback"].as_bool().unwrap_or(true),
            transcode: v["transcode"].as_str() != Some("never"),
        }
    }
}

/// Whether a BCP 47 `locale` (`fr`, `fr-BE`…) asks for French.
fn is_french(locale: &Value) -> bool {
    locale
        .as_str()
        .and_then(|l| l.split(['-', '_']).next())
        .is_some_and(|l| l.eq_ignore_ascii_case("fr"))
}

/// The declaration sent in the `initialize` result, and again with
/// `settings.declared` when the locale changes.
fn settings_schema(fr: bool) -> Value {
    let t = |en: &'static str, f: &'static str| if fr { f } else { en };
    let section = t("Playback", "Lecture");
    json!([
        {"key": "report_playback", "type": "bool", "section": section,
         "label": t("Report what I play", "Signaler les écoutes"),
         "description": t(
             "Tell the Jellyfin server which tracks you play: play counts, “played”, and “now playing” on its dashboard.",
             "Indiquer au serveur Jellyfin les pistes écoutées : nombre de lectures, « lu », et « en cours de lecture » dans son tableau de bord."),
         "default": true},
        {"key": "transcode", "type": "choice", "section": section,
         "label": t("Server conversion", "Conversion par le serveur"),
         "description": t(
             "When the DAC cannot take a file's sample rate or bit depth, Jellyfin can convert it to a format the DAC takes. With “Never”, such files are sent as they are and the player may refuse them.",
             "Quand le DAC ne prend pas la fréquence ou la résolution d'un fichier, Jellyfin peut le convertir dans un format que le DAC accepte. Avec « Jamais », ces fichiers sont envoyés tels quels et le lecteur peut les refuser."),
         "options": [
             {"value": "auto", "label": t("Only when the DAC needs it", "Seulement si le DAC l'exige")},
             {"value": "never", "label": t("Never (original files only)", "Jamais (fichiers d'origine seulement)")}
         ],
         "default": "auto"}
    ])
}

struct Out(Mutex<std::io::Stdout>);

impl Out {
    fn send(&self, v: Value) {
        let mut out = self.0.lock().unwrap();
        let _ = writeln!(out, "{v}");
        let _ = out.flush();
    }

    fn notify(&self, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }
}

struct Plugin {
    out: Arc<Out>,
    server_hint: String,
    data_dir: Mutex<PathBuf>,
    french: Mutex<bool>,
    output: Mutex<Output>,
    settings: Mutex<Settings>,
    /// Whether the user may edit a playlist, by Jellyfin id, as last asked.
    editable: Mutex<HashMap<String, bool>>,
    client: Mutex<Option<Arc<Client>>>,
    session: Mutex<Option<Session>>,
    /// The token was refused: signed in, but it needs renewing.
    expired: Mutex<bool>,
    login: Mutex<Option<login::Login>>,
}

impl Plugin {
    fn client(&self) -> Result<Arc<Client>, RpcError> {
        self.client
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| rpc_err(-32600, "not initialized"))
    }

    fn session(&self) -> Result<(Arc<Client>, Session), RpcError> {
        let client = self.client()?;
        let s = self.session.lock().unwrap().clone();
        match s {
            Some(s) if !*self.expired.lock().unwrap() => Ok((client, s)),
            _ => Err(rpc_err(-32001, "sign in to Jellyfin first")),
        }
    }

    fn auth_path(&self) -> PathBuf {
        self.data_dir.lock().unwrap().join("auth.json")
    }

    fn auth_status(&self) -> Value {
        match &*self.session.lock().unwrap() {
            None => json!({"state": "signed_out"}),
            Some(s) => {
                let state = if *self.expired.lock().unwrap() {
                    "expired"
                } else {
                    "signed_in"
                };
                let detail = if s.server_name.is_empty() {
                    s.server.clone()
                } else {
                    format!("{} · {}", s.server_name, s.server)
                };
                json!({"state": state, "account": {"display_name": s.user_name, "detail": detail}})
            }
        }
    }

    fn store(&self, s: Session) {
        let path = self.auth_path();
        let tmp = path.with_extension("tmp");
        let written = (|| {
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            f.write_all(s.to_json().to_string().as_bytes())?;
            std::fs::rename(&tmp, &path)
        })();
        if let Err(e) = written {
            eprintln!("cannot save the session in {}: {e}", path.display());
        }
        // The server only: the user name stays out of the logs.
        eprintln!("signed in on {}", s.server);
        *self.session.lock().unwrap() = Some(s);
        *self.expired.lock().unwrap() = false;
        self.out.notify("auth.changed", self.auth_status());
    }

    /// Map a Jellyfin failure; a refused token marks the session expired.
    fn fail(&self, e: Error) -> RpcError {
        match e {
            Error::Auth => {
                let was = std::mem::replace(&mut *self.expired.lock().unwrap(), true);
                if !was {
                    eprintln!("the server refused the access token");
                    self.out.notify("auth.changed", self.auth_status());
                }
                rpc_err(-32001, "the Jellyfin session has expired")
            }
            Error::NotFound => rpc_err(-32002, "not found on the server"),
            Error::Status(503, m) | Error::Status(429, m) => RpcError {
                code: -32004,
                message: m,
            },
            Error::Status(code, m) if code >= 500 => {
                rpc_err(-32005, format!("server error {code} {m}"))
            }
            Error::Status(code, m) => rpc_err(-32603, format!("server answered {code} {m}")),
            Error::Network(m) => rpc_err(-32005, m),
        }
    }

    // ---------------------------------------------------------------- setup

    fn initialize(&self, p: &Value) -> Reply {
        let data_dir = p["data_dir"]
            .as_str()
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let _ = std::fs::create_dir_all(&data_dir);
        *self.french.lock().unwrap() = is_french(&p["locale"]);
        *self.output.lock().unwrap() = Output::from_json(&p["output"]);
        *self.settings.lock().unwrap() = Settings::from_json(&p["settings"]);

        let id_path = data_dir.join("device_id");
        let device_id = std::fs::read_to_string(&id_path)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| s.len() == 32)
            .unwrap_or_else(|| {
                let id = random_hex(16);
                let _ = std::fs::write(&id_path, &id);
                id
            });
        let device = std::fs::read_to_string("/etc/hostname")
            .ok()
            .map(|h| h.trim().to_string())
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| "ricercar".into());
        *self.client.lock().unwrap() = Some(Arc::new(Client::new(device, device_id)));
        *self.data_dir.lock().unwrap() = data_dir;
        *self.session.lock().unwrap() = std::fs::read_to_string(self.auth_path())
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .and_then(|v| Session::from_json(&v));

        let proto = p["protocol"].as_u64().unwrap_or(0);
        if proto != PROTOCOL {
            eprintln!("host speaks protocol {proto}, this plugin {PROTOCOL}");
        }
        Ok(json!({
            "protocol": PROTOCOL,
            "plugin": {"id": "jellyfin", "name": "Jellyfin", "version": env!("CARGO_PKG_VERSION")},
            "capabilities": {
                "auth": true, "browse": true, "search": true, "resolve": true,
                "favorites": true, "reporting": true, "remote_control": false,
                "library": true, "lyrics": true, "playlist_edit": true, "details": true,
                "radio": true
            },
            "settings": settings_schema(*self.french.lock().unwrap())
        }))
    }

    // ----------------------------------------------------------------- auth

    fn auth_begin(self: &Arc<Self>) -> Reply {
        let client = self.client()?;
        let french = *self.french.lock().unwrap();
        let mut login = self.login.lock().unwrap();
        if login.is_none() {
            let me = self.clone();
            let hint = self
                .session
                .lock()
                .unwrap()
                .as_ref()
                .map(|s| s.server.clone())
                .unwrap_or_else(|| self.server_hint.clone());
            *login = Some(
                login::Login::start(client, Arc::new(move |s| me.store(s)), hint, french)
                    .map_err(|e| rpc_err(-32603, format!("cannot open the sign-in page: {e}")))?,
            );
        }
        let instructions = if french {
            "Saisissez l'adresse de votre serveur Jellyfin dans la page qui s'ouvre, puis validez le code Quick Connect ou utilisez votre mot de passe. Depuis un autre appareil, collez ici « adresse jeton » (un jeton d'accès Jellyfin)."
        } else {
            "Enter your Jellyfin server address on the page that opens, then approve the Quick Connect code or use your password. From another device, paste “address token” here (a Jellyfin access token)."
        };
        Ok(json!({
            "url": login.as_ref().unwrap().url,
            "instructions": instructions,
            "expects_input": false
        }))
    }

    /// The paste field: `<server address> <access token>`, or nothing (the
    /// page may already have signed the user in).
    fn auth_complete(&self, p: &Value) -> Reply {
        let input = p["input"].as_str().unwrap_or("").trim();
        let mut words = input.split_whitespace();
        if let (Some(addr), Some(token), None) = (words.next(), words.next(), words.next()) {
            let client = self.client()?;
            let server = jellyfin::normalize_server(addr)
                .ok_or_else(|| rpc_err(-32602, "expected “server-address access-token”"))?;
            match client.sign_in_token(&server, token) {
                Ok(s) => self.store(s),
                Err(Error::Auth) => {}
                Err(e) => return Err(self.fail(e)),
            }
        }
        Ok(self.auth_status())
    }

    fn sign_out(&self) -> Reply {
        let s = self.session.lock().unwrap().take();
        if let (Some(s), Ok(client)) = (s, self.client()) {
            client.sign_out(&s);
        }
        *self.expired.lock().unwrap() = false;
        let _ = std::fs::remove_file(self.auth_path());
        Ok(Value::Null)
    }

    // --------------------------------------------------------------- browse

    fn root(&self) -> Reply {
        self.session()?;
        let fr = *self.french.lock().unwrap();
        let t = |en: &'static str, f: &'static str| if fr { f } else { en };
        let sections = [
            ("recent", t("Recently added", "Ajouts récents")),
            ("albums", t("Albums", "Albums")),
            ("artists", t("Artists", "Artistes")),
            ("playlists", t("Playlists", "Listes de lecture")),
            ("favorites", t("Favourites", "Favoris")),
            ("libraries", t("Folders", "Dossiers")),
        ];
        let sections: Vec<Value> = sections
            .iter()
            .map(
                |(r, title)| json!({"ref": r, "kind": "folder", "title": title, "browsable": true}),
            )
            .collect();
        // Shelves of albums for the host's Home page.
        let home = [
            ("recent", t("Recently added", "Ajouts récents")),
            ("random", t("Random albums", "Albums au hasard")),
        ];
        let home: Vec<Value> = home
            .iter()
            .map(
                |(r, title)| json!({"ref": r, "kind": "folder", "title": title, "browsable": true}),
            )
            .collect();
        Ok(json!({ "sections": sections, "home": home }))
    }

    fn list(&self, p: &Value) -> Reply {
        let r = p["ref"].as_str().unwrap_or("");
        let offset = p["offset"].as_u64().unwrap_or(0);
        let limit = p["limit"].as_u64().unwrap_or(PAGE).clamp(1, PAGE);
        self.listing(r, offset, limit)
    }

    /// One page of a section (`albums`, `tracks`…) or of an item's children.
    fn listing(&self, r: &str, offset: u64, limit: u64) -> Reply {
        let (client, s) = self.session()?;
        let fr = *self.french.lock().unwrap();
        let uid = s.user_id.clone();
        let mut q: Vec<(&str, String)> = vec![
            ("userId", uid.clone()),
            ("StartIndex", offset.to_string()),
            ("Limit", limit.to_string()),
            ("Fields", FIELDS.into()),
            ("EnableTotalRecordCount", "true".into()),
        ];
        let mut path = "/Items".to_string();
        let music = "MusicAlbum,Audio";
        match r {
            "recent" => q.extend([
                ("IncludeItemTypes", "MusicAlbum".into()),
                ("Recursive", "true".into()),
                ("SortBy", "DateCreated,SortName".into()),
                ("SortOrder", "Descending".into()),
            ]),
            "random" => q.extend([
                ("IncludeItemTypes", "MusicAlbum".into()),
                ("Recursive", "true".into()),
                ("SortBy", "Random".into()),
            ]),
            "albums" => q.extend([
                ("IncludeItemTypes", "MusicAlbum".into()),
                ("Recursive", "true".into()),
                ("SortBy", "SortName".into()),
            ]),
            "tracks" => q.extend([
                ("IncludeItemTypes", "Audio".into()),
                ("Recursive", "true".into()),
                ("SortBy", "SortName".into()),
            ]),
            "artists" => {
                path = "/Artists/AlbumArtists".into();
                q.push(("SortBy", "SortName".into()));
            }
            "playlists" => q.extend([
                ("IncludeItemTypes", "Playlist".into()),
                ("Recursive", "true".into()),
                ("SortBy", "SortName".into()),
            ]),
            "favorites" => q.extend([
                ("IncludeItemTypes", music.into()),
                ("Filters", "IsFavorite".into()),
                ("Recursive", "true".into()),
                ("SortBy", "SortName".into()),
            ]),
            "libraries" => {
                // The user's music libraries, whole: no paging on the server.
                let views = client
                    .get(&s, &format!("/Users/{uid}/Views"), &[])
                    .map_err(|e| self.fail(e))?;
                let all: Vec<Value> = views["Items"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|v| v["CollectionType"] == "music")
                    .filter_map(|v| items::item(&s.server, v, fr))
                    .collect();
                return Ok(page(all, offset, limit));
            }
            // No paging on the server for these two: one list, cut here.
            _ if r.starts_with("m/") || r.starts_with("x/") => {
                let Some((k, id)) = items::split_ref(r) else {
                    return Err(rpc_err(-32002, "no such list"));
                };
                let v = if k == "m" {
                    self.instant_mix(&client, &s, id, PAGE)?
                } else {
                    self.similar(&client, &s, id, 50)?
                };
                return Ok(page(items::items(&s.server, &v, fr), offset, limit));
            }
            _ => match items::split_ref(r) {
                Some(("a", id)) => q.extend([
                    ("ParentId", id.to_string()),
                    ("IncludeItemTypes", "Audio".into()),
                    ("Recursive", "true".into()),
                    ("SortBy", "ParentIndexNumber,IndexNumber,SortName".into()),
                ]),
                Some(("r", id)) => q.extend([
                    ("AlbumArtistIds", id.to_string()),
                    ("IncludeItemTypes", "MusicAlbum".into()),
                    ("Recursive", "true".into()),
                    ("SortBy", "ProductionYear,SortName".into()),
                    ("SortOrder", "Descending,Ascending".into()),
                ]),
                Some(("s", id)) => q.extend([
                    ("StudioIds", id.to_string()),
                    ("IncludeItemTypes", "MusicAlbum".into()),
                    ("Recursive", "true".into()),
                    ("SortBy", "ProductionYear,SortName".into()),
                    ("SortOrder", "Descending,Ascending".into()),
                ]),
                Some(("p", id)) => path = format!("/Playlists/{id}/Items"),
                Some(("f", id)) => q.extend([
                    ("ParentId", id.to_string()),
                    ("SortBy", "IsFolder,SortName".into()),
                ]),
                _ => return Err(rpc_err(-32002, "no such list")),
            },
        }
        // An unknown `ParentId` is a 400 on Jellyfin 12, not a 404.
        let by_parent = q.iter().any(|(k, _)| *k == "ParentId");
        let get = |q: &[(&str, String)]| {
            client
                .get(&s, &path, q)
                .map_err(|e| match e {
                    Error::Status(400, _) if by_parent => Error::NotFound,
                    e => e,
                })
                .map_err(|e| self.fail(e))
        };
        let mut v = get(&q)?;
        // A label set on tracks only: its tracks, then.
        if r.starts_with("s/") && v["TotalRecordCount"] == 0 {
            for (k, val) in q.iter_mut() {
                match *k {
                    "IncludeItemTypes" => *val = "Audio".into(),
                    "SortBy" => *val = "Album,ParentIndexNumber,IndexNumber,SortName".into(),
                    "SortOrder" => *val = "Ascending".into(),
                    _ => {}
                }
            }
            v = get(&q)?;
        }
        let mut list = items::items(&s.server, &v, fr);
        if r == "artists" {
            self.artist_art(&client, &s, &mut list);
        }
        self.mark_editable(&client, &s, &mut list, false);
        let total = v["TotalRecordCount"].as_u64();
        let seen = v["Items"].as_array().map_or(0, Vec::len) as u64;
        let has_more = match total {
            Some(t) => offset + seen < t,
            None => seen == limit,
        };
        Ok(json!({"items": list, "total": total, "has_more": has_more}))
    }

    /// Artists without a picture of their own get the cover of one of their
    /// albums: one query per 50 artists (URLs stay short).
    fn artist_art(&self, client: &Client, s: &Session, list: &mut [Value]) {
        let bare: Vec<String> = list
            .iter()
            .filter(|it| it["kind"] == "artist" && it.get("art").is_none())
            .filter_map(|it| items::split_ref(it["ref"].as_str()?).map(|(_, id)| id.to_string()))
            .collect();
        let mut covers: std::collections::HashMap<String, String> = Default::default();
        for ids in bare.chunks(50) {
            let q = [
                ("userId", s.user_id.clone()),
                ("AlbumArtistIds", ids.join(",")),
                ("IncludeItemTypes", "MusicAlbum".into()),
                ("Recursive", "true".into()),
                ("ImageTypes", "Primary".into()),
                ("EnableUserData", "false".into()),
                ("Fields", String::new()),
            ];
            let Ok(v) = client.get(s, "/Items", &q) else {
                return;
            };
            for album in v["Items"].as_array().into_iter().flatten() {
                let Some(url) = items::art(&s.server, album) else {
                    continue;
                };
                for a in album["AlbumArtists"].as_array().into_iter().flatten() {
                    if let Some(id) = a["Id"].as_str() {
                        covers.entry(id.to_string()).or_insert_with(|| url.clone());
                    }
                }
            }
        }
        for it in list.iter_mut() {
            let id = it["ref"]
                .as_str()
                .and_then(items::split_ref)
                .map(|(_, id)| id);
            if let Some(url) = id.and_then(|id| covers.get(id)).cloned() {
                it["art"] = url.into();
            }
        }
    }

    fn search(&self, p: &Value) -> Reply {
        let (client, s) = self.session()?;
        let query = p["query"].as_str().unwrap_or("").trim().to_string();
        let offset = p["offset"].as_u64().unwrap_or(0);
        let limit = p["limit"].as_u64().unwrap_or(50).clamp(1, PAGE);
        let wanted: Vec<String> = p["kinds"]
            .as_array()
            .map(|k| {
                k.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_else(|| {
                ["artist", "album", "playlist", "track"]
                    .map(String::from)
                    .to_vec()
            });
        if query.is_empty() {
            return Ok(json!({ "groups": [] }));
        }
        let fr = *self.french.lock().unwrap();
        let mut groups = Vec::new();
        for kind in wanted {
            let (path, jf_type) = match kind.as_str() {
                "artist" => ("/Artists/AlbumArtists", None),
                "album" => ("/Items", Some("MusicAlbum")),
                "track" => ("/Items", Some("Audio")),
                "playlist" => ("/Items", Some("Playlist")),
                _ => continue,
            };
            let mut q: Vec<(&str, String)> = vec![
                ("userId", s.user_id.clone()),
                ("searchTerm", query.clone()),
                ("StartIndex", offset.to_string()),
                ("Limit", limit.to_string()),
                ("Fields", FIELDS.into()),
                ("Recursive", "true".into()),
                ("EnableTotalRecordCount", "true".into()),
            ];
            if let Some(t) = jf_type {
                q.push(("IncludeItemTypes", t.into()));
            }
            let v = client.get(&s, path, &q).map_err(|e| self.fail(e))?;
            let mut found = items::items(&s.server, &v, fr);
            if kind == "artist" {
                self.artist_art(&client, &s, &mut found);
            }
            self.mark_editable(&client, &s, &mut found, false);
            let total = v["TotalRecordCount"].as_u64();
            let seen = v["Items"].as_array().map_or(0, Vec::len) as u64;
            groups.push(json!({
                "kind": kind,
                "items": found,
                "total": total,
                "has_more": total.is_some_and(|t| offset + seen < t),
            }));
        }
        Ok(json!({ "groups": groups }))
    }

    /// One Jellyfin item by id, with `fields`.
    fn fetch(
        &self,
        client: &Client,
        s: &Session,
        id: &str,
        fields: &str,
    ) -> Result<Value, RpcError> {
        let v = client
            .get(
                s,
                "/Items",
                &[
                    ("userId", s.user_id.clone()),
                    ("Ids", id.to_string()),
                    ("Fields", fields.to_string()),
                ],
            )
            .map_err(|e| self.fail(e))?;
        v["Items"]
            .as_array()
            .and_then(|a| a.first())
            .cloned()
            .ok_or_else(|| rpc_err(-32002, "not found on the server"))
    }

    /// One Jellyfin item by id, through the single-item route (studios are
    /// not found by `/Items?Ids=`): 10.9 and later, then the older one.
    fn fetch_one(&self, client: &Client, s: &Session, id: &str) -> Result<Value, RpcError> {
        let q = [("userId", s.user_id.clone())];
        match client.get(s, &format!("/Items/{id}"), &q) {
            Err(Error::NotFound) => client.get(s, &format!("/Users/{}/Items/{id}", s.user_id), &[]),
            r => r,
        }
        .map_err(|e| self.fail(e))
    }

    fn item_get(&self, p: &Value) -> Reply {
        let (client, s) = self.session()?;
        let fr = *self.french.lock().unwrap();
        let (k, id) = items::split_ref(p["ref"].as_str().unwrap_or(""))
            .ok_or_else(|| rpc_err(-32002, "no such item"))?;
        match k {
            "s" => {
                let v = self.fetch_one(&client, &s, id)?;
                return items::item(&s.server, &v, fr)
                    .filter(|it| it["ref"] == format!("s/{id}"))
                    .ok_or_else(|| rpc_err(-32002, "not a label"));
            }
            // What the actions open: a folder named after its item.
            "m" | "x" => {
                let v = self.fetch(&client, &s, id, "")?;
                let t = |en: &'static str, f: &'static str| if fr { f } else { en };
                let title = match (k, v["Type"].as_str()) {
                    ("m", _) => t("Instant Mix", "Mix instantané"),
                    (_, Some("MusicArtist")) => t("Similar artists", "Artistes similaires"),
                    (_, Some("MusicAlbum")) => t("Similar albums", "Albums similaires"),
                    _ => t("Similar", "Similaires"),
                };
                let mut it = json!({"ref": format!("{k}/{id}"), "kind": "folder", "title": title,
                                    "browsable": true});
                if let Some(name) = v["Name"].as_str() {
                    it["subtitle"] = name.into();
                }
                if let Some(a) = items::art(&s.server, &v) {
                    it["art"] = a.into();
                }
                return Ok(it);
            }
            _ => {}
        }
        let v = self.fetch(&client, &s, id, FIELDS)?;
        let mut it =
            items::item(&s.server, &v, fr).ok_or_else(|| rpc_err(-32002, "not a music item"))?;
        self.mark_editable(&client, &s, std::slice::from_mut(&mut it), true);
        Ok(it)
    }

    fn favorite(&self, p: &Value) -> Reply {
        let (client, s) = self.session()?;
        let (_, id) = items::split_ref(p["ref"].as_str().unwrap_or(""))
            .ok_or_else(|| rpc_err(-32002, "no such item"))?;
        let on = p["on"].as_bool().unwrap_or(false);
        let q = [("userId", s.user_id.clone())];
        let call = |path: &str| {
            if on {
                client.post_empty(&s, path, &q)
            } else {
                client.delete(&s, path, &q)
            }
        };
        // 10.9 and later, then the older route.
        let r = match call(&format!("/UserFavoriteItems/{id}")) {
            Err(Error::NotFound) => call(&format!("/Users/{}/FavoriteItems/{id}", s.user_id)),
            r => r,
        };
        r.map(|_| Value::Null).map_err(|e| self.fail(e))
    }

    // -------------------------------------------------------------- library

    /// All the music the user can see on the server, the same lists as the
    /// sections of the same name.
    fn library(&self, method: &str, p: &Value) -> Reply {
        let offset = p["offset"].as_u64().unwrap_or(0);
        let limit = p["limit"].as_u64().unwrap_or(PAGE).clamp(1, PAGE);
        let r = match method {
            "library.albums" => "albums",
            "library.artists" => "artists",
            "library.playlists" => "playlists",
            _ => "tracks",
        };
        self.listing(r, offset, limit)
    }

    // -------------------------------------------------------------- resolve

    fn resolve(&self, p: &Value) -> Reply {
        let (client, s) = self.session()?;
        let r = p["ref"].as_str().unwrap_or("");
        let Some(("t", id)) = items::split_ref(r) else {
            return Err(rpc_err(-32002, "not a track"));
        };
        let v = self.fetch(&client, &s, id, "MediaSources,MediaStreams")?;
        if v["Type"] != "Audio" {
            return Err(rpc_err(-32002, "not a track"));
        }
        let stored = items::format(&v).unwrap_or(json!({}));
        let rate = stored["sample_rate"].as_u64().map(|r| r as u32);
        let bits = stored["bits"].as_u64().map(|b| b as u8);
        let plan = if self.settings.lock().unwrap().transcode {
            items::plan(&self.output.lock().unwrap(), rate, bits)
        } else {
            Plan::Direct
        };
        let source = v["MediaSources"][0]["Id"].as_str().unwrap_or(id);
        let mut url = format!(
            "{}/Audio/{id}/stream?static=true&mediaSourceId={source}&deviceId={}&ApiKey={}",
            s.server,
            client.device_id(),
            s.token
        );
        let mut format = stored.clone();
        let transcode = match plan {
            Plan::Direct => None,
            Plan::Flac { rate, bits } => Some(("flac", "flac".to_string(), rate, bits)),
            Plan::Wav { rate, bits } => Some(("wav", format!("pcm_s{bits}le"), rate, bits)),
        };
        if let Some((container, codec, rate, bits)) = transcode {
            url = format!(
                "{}/Audio/{id}/stream.{container}?container={container}&audioCodec={codec}&audioSampleRate={rate}\
                 &mediaSourceId={source}&deviceId={}&playSessionId={}&ApiKey={}",
                s.server,
                client.device_id(),
                random_hex(8),
                s.token
            );
            format["sample_rate"] = rate.into();
            format["bits"] = bits.into();
            format["codec"] = container.into();
            eprintln!("{r}: {rate} Hz / {bits} bits {container} transcode for this output");
        }
        let mut res = json!({
            "url": url,
            "duration_ms": v["RunTimeTicks"].as_i64().map(|t| t / 10_000),
            "format": format,
            "live": false,
        });
        // Jellyfin 10.9+ measures loudness (EBU R128) and stores a gain.
        if let Some(g) = v["NormalizationGain"].as_f64() {
            res["replaygain"] = json!({ "track_gain": g });
        }
        if let Some(o) = res.as_object_mut() {
            o.retain(|_, v| !v.is_null());
        }
        Ok(res)
    }

    // ------------------------------------------------------------ reporting

    fn report(&self, method: &str, p: &Value) {
        if !self.settings.lock().unwrap().report {
            return;
        }
        let Ok((client, s)) = self.session() else {
            return;
        };
        let Some(("t", id)) = items::split_ref(p["ref"].as_str().unwrap_or("")) else {
            return;
        };
        let ticks = |ms: &Value| ms.as_i64().unwrap_or(0) * 10_000;
        let (path, body) = match method {
            "playback.started" => (
                "/Sessions/Playing",
                json!({"ItemId": id, "PositionTicks": 0, "CanSeek": true, "PlayMethod": "DirectPlay"}),
            ),
            "playback.progress" => (
                "/Sessions/Playing/Progress",
                json!({"ItemId": id, "PositionTicks": ticks(&p["pos_ms"]), "CanSeek": true,
                       "PlayMethod": "DirectPlay", "EventName": "TimeUpdate"}),
            ),
            "playback.ended" => {
                // A track played to its end counts as played in Jellyfin.
                let pos = if p["reason"] == "ended" {
                    self.fetch(&client, &s, id, "")
                        .ok()
                        .and_then(|v| v["RunTimeTicks"].as_i64())
                        .unwrap_or_else(|| ticks(&p["listened_ms"]))
                } else {
                    ticks(&p["listened_ms"])
                };
                (
                    "/Sessions/Playing/Stopped",
                    json!({"ItemId": id, "PositionTicks": pos}),
                )
            }
            _ => return,
        };
        if let Err(e) = client.post(&s, path, body) {
            eprintln!("{method}: {e}");
        }
    }

    // --------------------------------------------------------------- lyrics

    fn lyrics(&self, p: &Value) -> Reply {
        let (client, s) = self.session()?;
        let Some(("t", id)) = items::split_ref(p["ref"].as_str().unwrap_or("")) else {
            return Err(rpc_err(-32002, "not a track"));
        };
        // 10.9 and later; older servers answer 404 like a track without any.
        let v = client
            .get(&s, &format!("/Audio/{id}/Lyrics"), &[])
            .map_err(|e| self.fail(e))?;
        items::lyrics(&v).ok_or_else(|| rpc_err(-32002, "no lyrics"))
    }

    // ------------------------------------------------------ mixes, details

    /// Jellyfin's instant mix of an item (track, album, artist, playlist):
    /// the generic route, then the one of the item's type.
    fn instant_mix(
        &self,
        client: &Client,
        s: &Session,
        id: &str,
        limit: u64,
    ) -> Result<Value, RpcError> {
        let q = [
            ("userId", s.user_id.clone()),
            ("Limit", limit.to_string()),
            ("Fields", FIELDS.into()),
        ];
        match client.get(s, &format!("/Items/{id}/InstantMix"), &q) {
            Err(Error::NotFound) => {
                let v = self.fetch(client, s, id, "")?;
                let route = match v["Type"].as_str() {
                    Some("Audio") => "Songs",
                    Some("MusicAlbum") => "Albums",
                    Some("MusicArtist") => "Artists",
                    Some("Playlist") => "Playlists",
                    _ => return Err(rpc_err(-32002, "no instant mix for this item")),
                };
                client
                    .get(s, &format!("/{route}/{id}/InstantMix"), &q)
                    .map_err(|e| self.fail(e))
            }
            r => r.map_err(|e| self.fail(e)),
        }
    }

    /// Items like this one, of the same type (albums, artists).
    fn similar(
        &self,
        client: &Client,
        s: &Session,
        id: &str,
        limit: u64,
    ) -> Result<Value, RpcError> {
        let q = [
            ("userId", s.user_id.clone()),
            ("Limit", limit.to_string()),
            ("Fields", FIELDS.into()),
        ];
        client
            .get(s, &format!("/Items/{id}/Similar"), &q)
            .map_err(|e| self.fail(e))
    }

    /// `radio.next`: the instant mix of the seed, without what was played.
    fn radio(&self, p: &Value) -> Reply {
        let (client, s) = self.session()?;
        let fr = *self.french.lock().unwrap();
        let seed = p["seed"].as_str().unwrap_or("");
        let Some((k @ ("t" | "a" | "r" | "p"), id)) = items::split_ref(seed) else {
            return Err(rpc_err(-32002, "no radio for this item"));
        };
        let limit = p["limit"].as_u64().unwrap_or(25).clamp(1, PAGE);
        let mut exclude: Vec<&str> = p["exclude"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        if k == "t" {
            exclude.push(seed);
        }
        let v = self.instant_mix(&client, &s, id, (limit + exclude.len() as u64).min(PAGE))?;
        let tracks: Vec<Value> = items::items(&s.server, &v, fr)
            .into_iter()
            .filter(|it| it["kind"] == "track")
            .filter(|it| !exclude.contains(&it["ref"].as_str().unwrap_or("")))
            .take(limit as usize)
            .collect();
        Ok(json!({ "items": tracks }))
    }

    /// `item.details` of a track, album, artist or playlist: its overview,
    /// related shelves (artists and albums) and a few facts.
    fn details(&self, p: &Value) -> Reply {
        let (client, s) = self.session()?;
        let fr = *self.french.lock().unwrap();
        let t = |en: &'static str, f: &'static str| if fr { f } else { en };
        let Some(("t" | "a" | "r" | "p", id)) = items::split_ref(p["ref"].as_str().unwrap_or(""))
        else {
            return Err(rpc_err(-32002, "no details for this item"));
        };
        let v = self.fetch(&client, &s, id, "Overview,Genres,Studios,ProductionYear")?;
        let mut out = json!({});
        let bio = v["Overview"].as_str().map(items::plain_text);
        if let Some(text) = bio.filter(|b| !b.is_empty()) {
            out["biography"] = json!({ "text": text });
        }

        // Shelves: best effort, a failed one is left out.
        let mut related = Vec::new();
        let mut shelf = |title: String, r: Result<Value, RpcError>| match r {
            Ok(list) => {
                let found = items::items(&s.server, &list, fr);
                if !found.is_empty() {
                    related.push(json!({"title": title, "items": found}));
                }
            }
            Err(e) => eprintln!("details of {id}: {}", e.message),
        };
        let list = |q: &[(&str, String)]| {
            let mut q = q.to_vec();
            q.extend([
                ("userId", s.user_id.clone()),
                ("Recursive", "true".into()),
                ("Limit", SHELF.to_string()),
                ("Fields", FIELDS.into()),
            ]);
            client.get(&s, "/Items", &q).map_err(|e| self.fail(e))
        };
        match v["Type"].as_str() {
            Some("MusicArtist") => {
                shelf(
                    t("Most played", "Les plus écoutés").into(),
                    list(&[
                        ("ArtistIds", id.to_string()),
                        ("IncludeItemTypes", "Audio".into()),
                        ("Filters", "IsPlayed".into()),
                        ("SortBy", "PlayCount,SortName".into()),
                        ("SortOrder", "Descending,Ascending".into()),
                    ]),
                );
                shelf(
                    t("Similar artists", "Artistes similaires").into(),
                    self.similar(&client, &s, id, SHELF),
                );
            }
            Some("MusicAlbum") => {
                let artist = v["AlbumArtists"][0]["Id"].as_str();
                if let Some(artist) = artist {
                    let name = v["AlbumArtists"][0]["Name"].as_str().unwrap_or("?");
                    let title = if fr {
                        format!("Autres albums de {name}")
                    } else {
                        format!("More by {name}")
                    };
                    shelf(
                        title,
                        list(&[
                            ("AlbumArtistIds", artist.to_string()),
                            ("ExcludeItemIds", id.to_string()),
                            ("IncludeItemTypes", "MusicAlbum".into()),
                            ("SortBy", "ProductionYear,SortName".into()),
                            ("SortOrder", "Descending,Ascending".into()),
                        ]),
                    );
                }
                shelf(
                    t("Similar albums", "Albums similaires").into(),
                    self.similar(&client, &s, id, SHELF),
                );
            }
            _ => {}
        }
        if !related.is_empty() {
            out["related"] = related.into();
        }
        let facts = items::facts(&v, fr);
        if !facts.is_empty() {
            out["facts"] = facts.into();
        }
        Ok(out)
    }

    // ------------------------------------------------------------ playlists

    /// Whether the signed-in user may edit playlist `id` (10.9 and later:
    /// its owner, or a share with edit rights). Unknown means no: older
    /// servers do not say.
    fn can_edit(&self, client: &Client, s: &Session, id: &str) -> Result<bool, RpcError> {
        let r = client.get(s, &format!("/Playlists/{id}/Users/{}", s.user_id), &[]);
        let ok = match r {
            Ok(v) => v["CanEdit"].as_bool() == Some(true),
            // Not shared with this user (404, 403), or an older server.
            Err(Error::NotFound) | Err(Error::Status(400..=499, _)) => false,
            Err(e) => return Err(self.fail(e)),
        };
        self.editable.lock().unwrap().insert(id.to_string(), ok);
        Ok(ok)
    }

    /// Set `editable` on the playlists of `list`, from what is known unless
    /// `fresh`; the others are asked for, a few at a time.
    fn mark_editable(&self, client: &Client, s: &Session, list: &mut [Value], fresh: bool) {
        let ids: Vec<String> = list
            .iter()
            .filter(|it| it["kind"] == "playlist")
            .filter_map(|it| items::split_ref(it["ref"].as_str()?).map(|(_, id)| id.to_string()))
            .filter(|id| fresh || !self.editable.lock().unwrap().contains_key(id))
            .collect();
        for chunk in ids.chunks(8) {
            std::thread::scope(|sc| {
                for id in chunk {
                    sc.spawn(move || {
                        if let Err(e) = self.can_edit(client, s, id) {
                            eprintln!("rights on playlist {id}: {}", e.message);
                        }
                    });
                }
            });
        }
        let known = self.editable.lock().unwrap();
        for it in list.iter_mut().filter(|it| it["kind"] == "playlist") {
            let id = it["ref"]
                .as_str()
                .and_then(items::split_ref)
                .map(|(_, id)| id);
            it["editable"] = id
                .and_then(|id| known.get(id))
                .copied()
                .unwrap_or(false)
                .into();
        }
    }

    /// The Jellyfin id of an editable playlist `ref`, or why not.
    fn editable_playlist(
        &self,
        client: &Client,
        s: &Session,
        p: &Value,
    ) -> Result<String, RpcError> {
        let Some(("p", id)) = items::split_ref(p["ref"].as_str().unwrap_or("")) else {
            return Err(rpc_err(-32602, "not a playlist"));
        };
        if !self.can_edit(client, s, id)? {
            return Err(rpc_err(-32602, "this playlist cannot be edited"));
        }
        Ok(id.to_string())
    }

    /// An edit the server refused although the user may edit the playlist
    /// (its token works: the rights were just read with it).
    fn refused(&self, e: Error) -> RpcError {
        match e {
            Error::Auth | Error::Status(403, _) => {
                rpc_err(-32003, "the server does not allow this change")
            }
            e => self.fail(e),
        }
    }

    fn playlist_create(&self, p: &Value) -> Reply {
        let (client, s) = self.session()?;
        let name = p["name"].as_str().unwrap_or("").trim();
        if name.is_empty() {
            return Err(rpc_err(-32602, "a playlist needs a name"));
        }
        // Jellyfin keeps no description at creation.
        let mut body = json!({"Name": name, "Ids": [], "UserId": s.user_id, "MediaType": "Audio"});
        if let Some(public) = p["public"].as_bool() {
            body["IsPublic"] = public.into();
        }
        // No rights read first here: a 401 is the token, a 403 the server.
        let v = client.post(&s, "/Playlists", body).map_err(|e| match e {
            Error::Status(403, _) => self.refused(e),
            e => self.fail(e),
        })?;
        let id = v["Id"]
            .as_str()
            .filter(|id| items::split_ref(&format!("p/{id}")).is_some())
            .ok_or_else(|| rpc_err(-32603, "unexpected answer to the playlist creation"))?;
        self.editable.lock().unwrap().insert(id.to_string(), true);
        let fr = *self.french.lock().unwrap();
        let mut it = self
            .fetch(&client, &s, id, FIELDS)
            .ok()
            .and_then(|v| items::item(&s.server, &v, fr))
            .unwrap_or_else(|| {
                json!({"ref": format!("p/{id}"), "kind": "playlist", "title": name,
                       "track_count": 0, "browsable": true})
            });
        it["editable"] = true.into();
        Ok(it)
    }

    fn playlist_rename(&self, p: &Value) -> Reply {
        let (client, s) = self.session()?;
        let name = p["name"].as_str().unwrap_or("").trim();
        if name.is_empty() {
            return Err(rpc_err(-32602, "a playlist needs a name"));
        }
        let id = self.editable_playlist(&client, &s, p)?;
        // 10.9 and later; before, the generic item update (admins only).
        match client.post(&s, &format!("/Playlists/{id}"), json!({ "Name": name })) {
            Err(Error::NotFound) | Err(Error::Status(405, _)) => {
                let mut v = client
                    .get(&s, &format!("/Users/{}/Items/{id}", s.user_id), &[])
                    .map_err(|e| self.fail(e))?;
                v["Name"] = name.into();
                client.post(&s, &format!("/Items/{id}"), v)
            }
            r => r,
        }
        .map(|_| Value::Null)
        .map_err(|e| self.refused(e))
    }

    fn playlist_delete(&self, p: &Value) -> Reply {
        let (client, s) = self.session()?;
        let id = self.editable_playlist(&client, &s, p)?;
        client
            .delete(&s, &format!("/Items/{id}"), &[])
            .map_err(|e| self.refused(e))?;
        self.editable.lock().unwrap().remove(&id);
        Ok(Value::Null)
    }

    /// The Jellyfin ids of `key` (tracks or entries) in `p`, all valid.
    fn ids_of(p: &Value, key: &str, prefix: Option<&str>) -> Result<Vec<String>, RpcError> {
        let list = p[key].as_array().filter(|a| !a.is_empty());
        let list = list.ok_or_else(|| rpc_err(-32602, format!("no {key}")))?;
        list.iter()
            .map(|v| {
                let v = v.as_str().unwrap_or("");
                let id = match prefix {
                    Some(k) => items::split_ref(v)
                        .filter(|(p, _)| *p == k)
                        .map(|(_, id)| id),
                    None => items::split_ref(&format!("t/{v}")).map(|_| v),
                };
                id.map(str::to_string)
                    .ok_or_else(|| rpc_err(-32602, format!("bad {key}: {v}")))
            })
            .collect()
    }

    fn playlist_add(&self, p: &Value) -> Reply {
        let (client, s) = self.session()?;
        let tracks = Self::ids_of(p, "items", Some("t"))?;
        let id = self.editable_playlist(&client, &s, p)?;
        for chunk in tracks.chunks(BATCH) {
            let q = [("ids", chunk.join(",")), ("userId", s.user_id.clone())];
            client
                .post_empty(&s, &format!("/Playlists/{id}/Items"), &q)
                .map_err(|e| self.refused(e))?;
        }
        Ok(Value::Null)
    }

    fn playlist_remove(&self, p: &Value) -> Reply {
        let (client, s) = self.session()?;
        let entries = Self::ids_of(p, "entries", None)?;
        let id = self.editable_playlist(&client, &s, p)?;
        for chunk in entries.chunks(BATCH) {
            client
                .delete(
                    &s,
                    &format!("/Playlists/{id}/Items"),
                    &[("entryIds", chunk.join(","))],
                )
                .map_err(|e| self.refused(e))?;
        }
        Ok(Value::Null)
    }

    /// The entry goes to index `to` (Jellyfin takes it out, then inserts it
    /// there, like the protocol).
    fn playlist_move(&self, p: &Value) -> Reply {
        let (client, s) = self.session()?;
        let entry = p["entry"].as_str().unwrap_or("");
        if items::split_ref(&format!("t/{entry}")).is_none() {
            return Err(rpc_err(-32602, "bad entry"));
        }
        let to = p["to"]
            .as_u64()
            .ok_or_else(|| rpc_err(-32602, "bad position"))?;
        let id = self.editable_playlist(&client, &s, p)?;
        // Past the end means last, as in the protocol; Jellyfin fails on it.
        let to = if to > 0 {
            let q = [
                ("userId", s.user_id.clone()),
                ("Limit", "1".into()),
                ("Fields", String::new()),
            ];
            let v = client
                .get(&s, &format!("/Playlists/{id}/Items"), &q)
                .map_err(|e| self.fail(e))?;
            match v["TotalRecordCount"].as_u64() {
                Some(len) => to.min(len.saturating_sub(1)),
                None => to,
            }
        } else {
            to
        };
        client
            .post_empty(&s, &format!("/Playlists/{id}/Items/{entry}/Move/{to}"), &[])
            .map(|_| Value::Null)
            .map_err(|e| self.refused(e))
    }

    // ------------------------------------------------------------- dispatch

    fn handle(self: &Arc<Self>, method: &str, p: &Value) -> Reply {
        match method {
            "initialize" => self.initialize(p),
            "auth.status" => Ok(self.auth_status()),
            "auth.begin" => self.auth_begin(),
            "auth.complete" => self.auth_complete(p),
            "auth.sign_out" => self.sign_out(),
            "browse.root" => self.root(),
            "browse.list" => self.list(p),
            "search" => self.search(p),
            "item.get" => self.item_get(p),
            "favorites.set" => self.favorite(p),
            "library.albums" | "library.artists" | "library.tracks" | "library.playlists" => {
                self.library(method, p)
            }
            "track.resolve" => self.resolve(p),
            "lyrics.get" => self.lyrics(p),
            "item.details" => self.details(p),
            "radio.next" => self.radio(p),
            "playlists.create" => self.playlist_create(p),
            "playlists.rename" => self.playlist_rename(p),
            "playlists.delete" => self.playlist_delete(p),
            "playlists.add" => self.playlist_add(p),
            "playlists.remove" => self.playlist_remove(p),
            "playlists.move" => self.playlist_move(p),
            _ => Err(rpc_err(-32601, format!("method not found: {method}"))),
        }
    }
}

fn page(all: Vec<Value>, offset: u64, limit: u64) -> Value {
    let total = all.len() as u64;
    let items: Vec<Value> = all
        .into_iter()
        .skip(offset as usize)
        .take(limit as usize)
        .collect();
    json!({"items": items, "total": total, "has_more": offset + limit < total})
}

fn main() {
    let mut server_hint = String::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--server" => server_hint = args.next().unwrap_or_default(),
            "--version" => {
                println!("ricercar-jellyfin {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            _ => eprintln!("unknown option {a}"),
        }
    }
    let out = Arc::new(Out(Mutex::new(std::io::stdout())));
    let plugin = Arc::new(Plugin {
        out: out.clone(),
        server_hint,
        data_dir: Mutex::new(std::env::temp_dir()),
        french: Mutex::new(false),
        output: Mutex::new(Output::default()),
        settings: Mutex::new(Settings::from_json(&Value::Null)),
        editable: Mutex::new(HashMap::new()),
        client: Mutex::new(None),
        session: Mutex::new(None),
        expired: Mutex::new(false),
        login: Mutex::new(None),
    });

    // Playback reports in the order they came, off the reading loop: an
    // `ended` must not overtake the `started` it follows.
    let reports = {
        let (tx, rx) = mpsc::channel::<(String, Value)>();
        let plugin = plugin.clone();
        std::thread::spawn(move || {
            for (method, params) in rx {
                plugin.report(&method, &params);
            }
        });
        tx
    };

    for line in BufReader::new(std::io::stdin()).lines() {
        let Ok(line) = line else { break };
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(method) = msg["method"].as_str().map(str::to_string) else {
            continue; // an answer; this plugin sends no requests
        };
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let Some(id) = msg.get("id").cloned() else {
            // Notifications.
            match method.as_str() {
                "output.changed" => {
                    *plugin.output.lock().unwrap() = Output::from_json(&params["output"]);
                }
                "settings.changed" => {
                    let new = Settings::from_json(&params["settings"]);
                    *plugin.settings.lock().unwrap() = new;
                    eprintln!("settings: {new:?}");
                }
                // The interface language changed: labels follow, the
                // settings dialog too.
                "locale.changed" => {
                    let fr = is_french(&params["locale"]);
                    let was = std::mem::replace(&mut *plugin.french.lock().unwrap(), fr);
                    if was != fr {
                        out.notify(
                            "settings.declared",
                            json!({"settings": settings_schema(fr)}),
                        );
                    }
                }
                m if m.starts_with("playback.") => {
                    let _ = reports.send((method, params));
                }
                _ => {}
            }
            continue;
        };
        if method == "shutdown" {
            out.send(json!({"jsonrpc": "2.0", "id": id, "result": null}));
            return;
        }
        let first = method == "initialize";
        let run = {
            let plugin = plugin.clone();
            let out = out.clone();
            move || {
                let reply = match plugin.handle(&method, &params) {
                    Ok(v) => json!({"jsonrpc": "2.0", "id": id, "result": v}),
                    Err(e) => json!({"jsonrpc": "2.0", "id": id,
                                     "error": {"code": e.code, "message": e.message}}),
                };
                out.send(reply);
            }
        };
        // The handshake first, in order; everything else may overlap.
        if first {
            run();
        } else {
            std::thread::spawn(run);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_values() {
        let d = Settings::from_json(&Value::Null);
        assert_eq!(
            d,
            Settings {
                report: true,
                transcode: true
            }
        );
        let s =
            Settings::from_json(&json!({"report_playback": false, "transcode": "never", "old": 1}));
        assert!(!s.report && !s.transcode);
        // A value of the wrong type or unknown: the default.
        assert_eq!(
            Settings::from_json(&json!({"report_playback": "no", "transcode": 3})),
            d
        );
    }

    #[test]
    fn settings_declaration() {
        for fr in [false, true] {
            let schema = settings_schema(fr);
            let entries = schema.as_array().unwrap();
            let keys: Vec<&str> = entries.iter().map(|e| e["key"].as_str().unwrap()).collect();
            assert_eq!(keys, ["report_playback", "transcode"]);
            // Every default is what `Settings` falls back to.
            let defaults: serde_json::Map<String, Value> = entries
                .iter()
                .map(|e| (e["key"].as_str().unwrap().to_string(), e["default"].clone()))
                .collect();
            assert_eq!(
                Settings::from_json(&Value::Object(defaults)),
                Settings::from_json(&Value::Null)
            );
            let t = &entries[1];
            assert!(
                t["options"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|o| o["value"] == t["default"])
            );
            assert!(
                entries
                    .iter()
                    .all(|e| !e["label"].as_str().unwrap().is_empty())
            );
        }
        assert_eq!(settings_schema(true)[0]["label"], "Signaler les écoutes");
    }

    #[test]
    fn locales() {
        for l in ["fr", "fr-BE", "FR", "fr_FR"] {
            assert!(is_french(&json!(l)), "{l}");
        }
        for l in ["en-GB", "frr", "de", ""] {
            assert!(!is_french(&json!(l)), "{l}");
        }
        assert!(!is_french(&Value::Null));
    }
}
