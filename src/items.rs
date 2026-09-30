//! Jellyfin items → ricercar items, and the stream format choice.
//!
//! Refs are `<prefix>/<jellyfin id>`: `t` track, `a` album, `r` artist,
//! `p` playlist, `f` folder or library, `s` studio (label); `m` the instant
//! mix and `x` the similar items of an item. Top-level sections use bare
//! words (`albums`, `artists`…).

use serde_json::{Value, json};

/// Kind prefix and id of a ref, for refs that point at a Jellyfin item.
pub fn split_ref(r: &str) -> Option<(&str, &str)> {
    let (k, id) = r.split_once('/')?;
    let ok = matches!(k, "t" | "a" | "r" | "p" | "f" | "s" | "m" | "x")
        && !id.is_empty()
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    ok.then_some((k, id))
}

fn text(v: &Value, k: &str) -> Option<String> {
    v.get(k)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn num(v: &Value, k: &str) -> Option<i64> {
    v.get(k).and_then(Value::as_i64)
}

/// The primary image of the item, or of its album for tracks.
pub fn art(server: &str, v: &Value) -> Option<String> {
    let own = v["ImageTags"]["Primary"].as_str().zip(v["Id"].as_str());
    let album = v["AlbumPrimaryImageTag"]
        .as_str()
        .zip(v["AlbumId"].as_str());
    let (tag, id) = own.or(album)?;
    Some(image(server, id, tag))
}

/// The URL of the primary image `tag` of item `id`.
pub fn image(server: &str, id: &str, tag: &str) -> String {
    format!("{server}/Items/{id}/Images/Primary?maxHeight=600&quality=90&tag={tag}")
}

/// The first audio stream: from `MediaStreams`, or the first media source.
pub fn audio_stream(v: &Value) -> Option<&Value> {
    let streams = v["MediaStreams"]
        .as_array()
        .or_else(|| v["MediaSources"][0]["MediaStreams"].as_array())?;
    streams.iter().find(|s| s["Type"] == "Audio")
}

/// `{sample_rate, bits, channels, codec}` of the file as stored.
pub fn format(v: &Value) -> Option<Value> {
    let s = audio_stream(v)?;
    let mut f = serde_json::Map::new();
    if let Some(r) = num(s, "SampleRate") {
        f.insert("sample_rate".into(), r.into());
    }
    if let Some(b) = num(s, "BitDepth").filter(|b| *b > 0) {
        f.insert("bits".into(), b.into());
    }
    if let Some(c) = num(s, "Channels") {
        f.insert("channels".into(), c.into());
    }
    if let Some(c) = text(s, "Codec") {
        f.insert("codec".into(), c.to_lowercase().into());
    }
    (!f.is_empty()).then_some(Value::Object(f))
}

fn join(parts: &[Option<String>]) -> Option<String> {
    let v: Vec<&str> = parts.iter().flatten().map(String::as_str).collect();
    (!v.is_empty()).then(|| v.join(" · "))
}

fn artists(v: &Value) -> Option<String> {
    let list: Vec<&str> = v["Artists"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if list.is_empty() {
        text(v, "AlbumArtist")
    } else {
        Some(list.join(", "))
    }
}

/// The id of the first `{Name, Id}` of list `k` (`AlbumArtists`…).
fn first_id<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v[k].as_array()?
        .iter()
        .find_map(|x| x["Id"].as_str().filter(|id| !id.is_empty()))
}

/// Entries of the item's menu: its instant mix, and similar albums or
/// artists.
fn actions(kind: &str, id: &str, fr: bool) -> Value {
    let t = |en: &'static str, f: &'static str| if fr { f } else { en };
    let mut a = vec![
        json!({"id": "instant_mix", "label": t("Instant Mix", "Mix instantané"),
                            "ref": format!("m/{id}"), "kind": "play"}),
    ];
    let similar = match kind {
        "album" => Some(t("Similar albums", "Albums similaires")),
        "artist" => Some(t("Similar artists", "Artistes similaires")),
        _ => None,
    };
    if let Some(label) = similar {
        a.push(
            json!({"id": "similar", "label": label, "ref": format!("x/{id}"), "kind": "browse"}),
        );
    }
    Value::Array(a)
}

/// One ricercar item, or `None` for types a music player has no use for.
/// `fr`: labels in French.
pub fn item(server: &str, v: &Value, fr: bool) -> Option<Value> {
    let id = v["Id"].as_str()?;
    let title = text(v, "Name").unwrap_or_else(|| "?".into());
    let year = num(v, "ProductionYear");
    let mut it = match v["Type"].as_str()? {
        "Audio" => {
            let artist = artists(v);
            let album = text(v, "Album");
            json!({
                "ref": format!("t/{id}"),
                "kind": "track",
                "title": title,
                "subtitle": join(&[artist.clone(), album.clone()]),
                "artist": artist,
                "album": album,
                "album_artist": text(v, "AlbumArtist"),
                "track_no": num(v, "IndexNumber"),
                "disc_no": num(v, "ParentIndexNumber"),
                "year": year,
                "genre": v["Genres"][0].as_str(),
                "duration_ms": num(v, "RunTimeTicks").map(|t| t / 10_000),
                "format": format(v),
                "playable": true,
                "album_ref": v["AlbumId"].as_str().filter(|a| !a.is_empty()).map(|a| format!("a/{a}")),
                "artist_ref": first_id(v, "AlbumArtists").or_else(|| first_id(v, "ArtistItems"))
                    .map(|r| format!("r/{r}")),
                // Only in a playlist's listing.
                "entry_id": text(v, "PlaylistItemId"),
            })
        }
        "MusicAlbum" => {
            let artist = text(v, "AlbumArtist").or_else(|| artists(v));
            json!({
                "ref": format!("a/{id}"),
                "kind": "album",
                "title": title,
                "subtitle": join(&[artist.clone(), year.map(|y| y.to_string())]),
                "artist": artist,
                "album": text(v, "Name"),
                "year": year,
                "genre": v["Genres"][0].as_str(),
                "track_count": num(v, "ChildCount"),
                "browsable": true,
                "artist_ref": first_id(v, "AlbumArtists").or_else(|| first_id(v, "ArtistItems"))
                    .map(|r| format!("r/{r}")),
            })
        }
        "MusicArtist" => json!({
            "ref": format!("r/{id}"),
            "kind": "artist",
            "title": title,
            "artist": text(v, "Name"),
            "browsable": true,
        }),
        "Playlist" => json!({
            "ref": format!("p/{id}"),
            "kind": "playlist",
            "title": title,
            "subtitle": num(v, "ChildCount").map(|n| format!("{n} ♪")),
            "track_count": num(v, "ChildCount"),
            "browsable": true,
        }),
        "Folder" | "CollectionFolder" | "UserView" => json!({
            "ref": format!("f/{id}"),
            "kind": "folder",
            "title": title,
            "browsable": true,
        }),
        // A label: its albums.
        "Studio" => json!({
            "ref": format!("s/{id}"),
            "kind": "folder",
            "title": title,
            "browsable": true,
        }),
        _ => return None,
    };
    if let Some(a) = art(server, v) {
        it["art"] = a.into();
    }
    let kind = it["kind"].as_str().unwrap_or("").to_string();
    if kind != "folder" {
        if let Some(fav) = v["UserData"]["IsFavorite"].as_bool() {
            it["favorite"] = fav.into();
        }
        it["actions"] = actions(&kind, id, fr);
    }
    if matches!(kind.as_str(), "track" | "album") {
        if let Some(s) = first_id(v, "Studios") {
            it["label_ref"] = format!("s/{s}").into();
        }
    }
    // Leave optional fields out rather than send nulls.
    if let Some(o) = it.as_object_mut() {
        o.retain(|_, v| !v.is_null());
    }
    Some(it)
}

pub fn items(server: &str, list: &Value, fr: bool) -> Vec<Value> {
    list["Items"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| item(server, v, fr)).collect())
        .unwrap_or_default()
}

/// `facts` of `item.details`: genres, label, year, length, rating, plays.
pub fn facts(v: &Value, fr: bool) -> Vec<Value> {
    let t = |en: &'static str, f: &'static str| if fr { f } else { en };
    let names = |k: &str, name: Option<&str>| -> Vec<String> {
        v[k].as_array()
            .into_iter()
            .flatten()
            .filter_map(|x| match name {
                Some(n) => x[n].as_str(),
                None => x.as_str(),
            })
            .map(str::trim)
            .filter(|x| !x.is_empty())
            .map(str::to_string)
            .collect()
    };
    let mut out = Vec::new();
    let mut fact = |label: &str, value: String| out.push(json!({"label": label, "value": value}));
    let genres = names("Genres", None);
    if !genres.is_empty() {
        let label = if genres.len() == 1 { "Genre" } else { "Genres" };
        fact(label, genres.join(", "));
    }
    let labels = names("Studios", Some("Name"));
    if !labels.is_empty() {
        fact("Label", labels.join(", "));
    }
    let kind = v["Type"].as_str().unwrap_or("");
    if kind != "MusicArtist" {
        if let Some(y) = num(v, "ProductionYear") {
            fact(t("Year", "Année"), y.to_string());
        }
    }
    if matches!(kind, "MusicAlbum" | "Playlist") {
        let min = num(v, "RunTimeTicks").unwrap_or(0) / 600_000_000;
        if min > 0 {
            let d = if min >= 60 {
                format!("{} h {:02} min", min / 60, min % 60)
            } else {
                format!("{min} min")
            };
            fact(t("Length", "Durée"), d);
        }
    }
    if let Some(r) = v["CommunityRating"].as_f64().filter(|r| *r > 0.0) {
        let r = format!("{r:.1}");
        let r = r.trim_end_matches(".0");
        fact(t("Rating", "Note"), format!("{r}/10"));
    }
    if let Some(n) = num(&v["UserData"], "PlayCount").filter(|n| *n > 0) {
        fact(t("Plays", "Écoutes"), n.to_string());
    }
    out
}

/// `lyrics.get` from Jellyfin's `{Lyrics: [{Text, Start?}]}`: synced when
/// the lines have a start (in ticks of 100 ns), plain otherwise. `None`
/// when there is no text at all.
pub fn lyrics(v: &Value) -> Option<Value> {
    let lines = v["Lyrics"].as_array()?;
    let line = |l: &Value| l["Text"].as_str().unwrap_or("").trim().to_string();
    let synced: Vec<Value> = lines
        .iter()
        .filter_map(|l| {
            let start = l["Start"].as_i64()?;
            Some(json!({"time_ms": start.max(0) / 10_000, "text": line(l)}))
        })
        .collect();
    if synced.iter().any(|l| l["text"] != "") {
        return Some(json!({ "synced": synced }));
    }
    let plain = lines.iter().map(line).collect::<Vec<_>>().join("\n");
    let plain = plain.trim_matches('\n');
    (!plain.trim().is_empty()).then(|| json!({ "plain": plain }))
}

/// Plain text from the HTML of an overview: tags dropped, entities decoded,
/// a line break for `<br>` and a blank line between paragraphs.
pub fn plain_text(html: &str) -> String {
    let mut out = String::new();
    let mut rest = html;
    while let Some(i) = rest.find('<') {
        out.push_str(&rest[..i]);
        let tail = &rest[i + 1..];
        // `a < b` is text, not a tag.
        if !tail.starts_with(|c: char| c.is_ascii_alphabetic() || c == '/' || c == '!') {
            out.push('<');
            rest = tail;
            continue;
        }
        let Some(end) = tail.find('>') else {
            rest = "";
            break;
        };
        let closing = tail.starts_with('/');
        let name: String = tail
            .trim_start_matches('/')
            .chars()
            .take_while(char::is_ascii_alphanumeric)
            .collect::<String>()
            .to_ascii_lowercase();
        rest = &tail[end + 1..];
        match name.as_str() {
            "br" => out.push('\n'),
            "p" | "div" | "li" | "ul" | "ol" | "tr" | "blockquote" | "h1" | "h2" | "h3" | "h4"
            | "h5" | "h6" => out.push_str("\n\n"),
            // Their content is not text.
            "script" | "style" if !closing => {
                let close = format!("</{name}");
                rest = match rest.to_ascii_lowercase().find(&close) {
                    Some(j) => &rest[j..],
                    None => "",
                };
            }
            _ => {}
        }
    }
    out.push_str(rest);
    let text = entities(&out);
    // One space between words, at most one blank line in a row.
    let mut lines: Vec<String> = Vec::new();
    for l in text.lines() {
        let l = l.split_whitespace().collect::<Vec<_>>().join(" ");
        if !l.is_empty() || lines.last().is_some_and(|p| !p.is_empty()) {
            lines.push(l);
        }
    }
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines.join("\n")
}

/// `&amp;`, `&#233;`, `&#xe9;`… decoded; unknown ones kept as they are.
fn entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let tail = &rest[i + 1..];
        let decoded = tail.find(';').filter(|j| *j <= 10).and_then(|j| {
            let name = &tail[..j];
            let c = match name {
                "amp" => '&',
                "lt" => '<',
                "gt" => '>',
                "quot" => '"',
                "apos" => '\'',
                "nbsp" => ' ',
                _ => {
                    let n = name.strip_prefix('#')?;
                    let code = match n.strip_prefix(['x', 'X']) {
                        Some(h) => u32::from_str_radix(h, 16).ok()?,
                        None => n.parse().ok()?,
                    };
                    char::from_u32(code)?
                }
            };
            Some((c, j))
        });
        match decoded {
            Some((c, j)) => {
                out.push(c);
                rest = &tail[j + 1..];
            }
            None => {
                out.push('&');
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    out
}

/// What the DAC takes natively, from `initialize` / `output.changed`.
#[derive(Clone, Debug, Default)]
pub struct Output {
    pub bit_perfect: bool,
    pub max_rate: Option<u32>,
    pub max_bits: Option<u8>,
    pub rates: Vec<u32>,
}

impl Output {
    pub fn from_json(v: &Value) -> Output {
        Output {
            bit_perfect: v["bit_perfect"].as_bool().unwrap_or(false),
            max_rate: v["max_rate"].as_u64().map(|r| r as u32),
            max_bits: v["max_bits"].as_u64().map(|b| b as u8),
            rates: v["rates"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|r| r.as_u64())
                        .map(|r| r as u32)
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    fn takes_rate(&self, rate: u32) -> bool {
        if self.rates.is_empty() {
            self.max_rate.is_none_or(|m| rate <= m)
        } else {
            self.rates.contains(&rate)
        }
    }
}

/// How to fetch a track so that the engine plays it without converting it.
#[derive(Debug, PartialEq)]
pub enum Plan {
    /// The original file, byte for byte.
    Direct,
    /// A FLAC transcode at a rate the DAC accepts, same depth as the file.
    Flac { rate: u32, bits: u8 },
    /// A WAV transcode at a lower depth. Jellyfin keeps the source depth
    /// when it encodes FLAC, whatever the request asks; only PCM honours it.
    Wav { rate: u32, bits: u8 },
}

/// Keep the original unless the DAC cannot take its rate or depth; then
/// ask Jellyfin for FLAC at the closest rate the DAC takes, preferring the
/// same family (44.1 kHz or 48 kHz multiples) and never going up. A file
/// deeper than the DAC comes as WAV, the only format Jellyfin reduces.
pub fn plan(out: &Output, rate: Option<u32>, bits: Option<u8>) -> Plan {
    let Some(rate) = rate else {
        return Plan::Direct;
    };
    let bits_ok = |b: u8| out.max_bits.is_none_or(|m| b <= m);
    if !out.bit_perfect || (out.takes_rate(rate) && bits.is_none_or(bits_ok)) {
        return Plan::Direct;
    }
    let mut candidates: Vec<u32> = if out.rates.is_empty() {
        [
            44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000,
        ]
        .into_iter()
        .filter(|r| out.takes_rate(*r))
        .collect()
    } else {
        out.rates.clone()
    };
    candidates.sort_unstable();
    let family = |r: u32| r % 11_025 == 0;
    let target = candidates
        .iter()
        .rev()
        .find(|r| **r <= rate && family(**r) == family(rate))
        .or_else(|| candidates.iter().rev().find(|r| **r <= rate))
        .or_else(|| candidates.first())
        .copied()
        .unwrap_or(rate);
    let depth = bits.unwrap_or(16).min(out.max_bits.unwrap_or(24)).max(16);
    if bits.is_some_and(|b| depth < b) {
        Plan::Wav {
            rate: target,
            bits: depth,
        }
    } else {
        Plan::Flac {
            rate: target,
            bits: depth,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERVER: &str = "http://jf";

    fn dac(rates: &[u32], bits: u8) -> Output {
        Output {
            bit_perfect: true,
            max_rate: rates.iter().max().copied(),
            max_bits: Some(bits),
            rates: rates.to_vec(),
        }
    }

    #[test]
    fn refs() {
        assert_eq!(split_ref("t/abc123"), Some(("t", "abc123")));
        assert_eq!(split_ref("z/abc"), None);
        assert_eq!(split_ref("t/../x"), None);
        assert_eq!(split_ref("albums"), None);
        for k in ["s", "m", "x"] {
            assert_eq!(split_ref(&format!("{k}/ab-1")), Some((k, "ab-1")));
        }
    }

    #[test]
    fn links_actions_and_favourite() {
        let v = json!({
            "Id": "e1", "Type": "Audio", "Name": "Coda", "AlbumId": "a1",
            "ArtistItems": [{"Name": "Guest", "Id": "g1"}],
            "AlbumArtists": [{"Name": "Ensemble", "Id": "r1"}],
            "Studios": [{"Name": "North Label", "Id": "s1"}],
            "UserData": {"IsFavorite": true, "PlayCount": 2},
            "PlaylistItemId": "pe7"
        });
        let it = item(SERVER, &v, false).unwrap();
        assert_eq!(it["album_ref"], "a/a1");
        assert_eq!(it["artist_ref"], "r/r1");
        assert_eq!(it["label_ref"], "s/s1");
        assert_eq!(it["favorite"], true);
        assert_eq!(it["entry_id"], "pe7");
        assert_eq!(
            it["actions"],
            json!([{"id": "instant_mix", "label": "Instant Mix", "ref": "m/e1", "kind": "play"}])
        );
        // No album artist: the track's own artist; no user data: unknown.
        let v = json!({"Id": "e2", "Type": "Audio", "Name": "x", "AlbumArtists": [],
                       "ArtistItems": [{"Name": "Guest", "Id": "g1"}], "Studios": []});
        let it = item(SERVER, &v, true).unwrap();
        assert_eq!(it["artist_ref"], "r/g1");
        for k in ["album_ref", "label_ref", "favorite", "entry_id"] {
            assert!(it.get(k).is_none(), "{k}");
        }
        assert_eq!(it["actions"][0]["label"], "Mix instantané");

        let album = json!({"Id": "a1", "Type": "MusicAlbum", "Name": "Sessions",
                           "AlbumArtists": [{"Name": "Ensemble", "Id": "r1"}],
                           "UserData": {"IsFavorite": false}});
        let it = item(SERVER, &album, true).unwrap();
        assert_eq!(it["artist_ref"], "r/r1");
        assert_eq!(it["favorite"], false);
        let a = it["actions"].as_array().unwrap();
        assert_eq!(a.len(), 2);
        assert_eq!(
            a[1],
            json!({"id": "similar", "label": "Albums similaires", "ref": "x/a1", "kind": "browse"})
        );
        let artist = item(
            SERVER,
            &json!({"Id": "r1", "Type": "MusicArtist", "Name": "E"}),
            false,
        );
        assert_eq!(artist.unwrap()["actions"][1]["label"], "Similar artists");
        let pl = item(
            SERVER,
            &json!({"Id": "p1", "Type": "Playlist", "Name": "P"}),
            false,
        );
        assert_eq!(pl.unwrap()["actions"].as_array().unwrap().len(), 1);

        // Labels are folders of albums, with no menu of their own.
        let st = item(
            SERVER,
            &json!({"Id": "s1", "Type": "Studio", "Name": "North Label"}),
            false,
        )
        .unwrap();
        assert_eq!(st["ref"], "s/s1");
        assert_eq!(st["kind"], "folder");
        assert!(st.get("actions").is_none());
    }

    #[test]
    fn lyrics_mapping() {
        let synced = json!({"Metadata": {}, "Lyrics": [
            {"Text": "First line", "Start": 15_000_000i64, "Cues": []},
            {"Text": " Second ", "Start": 40_000_000i64},
            {"Text": "Third", "Start": 622_500_000i64}
        ]});
        assert_eq!(
            lyrics(&synced).unwrap(),
            json!({"synced": [
                {"time_ms": 1500, "text": "First line"},
                {"time_ms": 4000, "text": "Second"},
                {"time_ms": 62250, "text": "Third"}
            ]})
        );
        let plain = json!({"Lyrics": [{"Text": ""}, {"Text": "Plain one"}, {"Text": "Plain two "}, {"Text": ""}]});
        assert_eq!(
            lyrics(&plain).unwrap(),
            json!({"plain": "Plain one\nPlain two"})
        );
        assert_eq!(lyrics(&json!({"Lyrics": []})), None);
        assert_eq!(lyrics(&json!({"Lyrics": [{"Text": " "}]})), None);
        assert_eq!(lyrics(&json!({})), None);
    }

    #[test]
    fn html_to_text() {
        assert_eq!(
            plain_text("<p>Recorded <b>live</b>  in one take.</p><p>Second &amp; last.</p>"),
            "Recorded live in one take.\n\nSecond & last."
        );
        assert_eq!(plain_text("a<br>b<br/>c"), "a\nb\nc");
        assert_eq!(
            plain_text("x < y &lt;tag&gt; &#233;t&#xE9; &nbsp;&bogus; & z"),
            "x < y <tag> été &bogus; & z"
        );
        assert_eq!(
            plain_text("<style>p{color:red}</style>Text<script>alert(1)</script>!"),
            "Text!"
        );
        assert_eq!(
            plain_text("<div>\n\n\n<p>One</p>\n\n\n<p>Two</p></div>"),
            "One\n\nTwo"
        );
        assert_eq!(plain_text("Plain\nlines"), "Plain\nlines");
        assert_eq!(plain_text("cut <a href"), "cut");
    }

    #[test]
    fn details_facts() {
        let album = json!({"Type": "MusicAlbum", "Genres": ["Jazz", "Blues"],
                           "Studios": [{"Name": "North Label", "Id": "s1"}], "ProductionYear": 2021,
                           "RunTimeTicks": 39_000_000_000i64, "CommunityRating": 8.0});
        assert_eq!(
            facts(&album, false),
            json!([
                {"label": "Genres", "value": "Jazz, Blues"},
                {"label": "Label", "value": "North Label"},
                {"label": "Year", "value": "2021"},
                {"label": "Length", "value": "1 h 05 min"},
                {"label": "Rating", "value": "8/10"}
            ])
            .as_array()
            .unwrap()
            .clone()
        );
        let track = json!({"Type": "Audio", "Genres": ["Jazz"], "ProductionYear": 2021,
                           "RunTimeTicks": 2_450_000_000i64, "CommunityRating": 7.25,
                           "UserData": {"PlayCount": 3}});
        let f = facts(&track, true);
        let pairs: Vec<(&str, &str)> = f
            .iter()
            .map(|x| (x["label"].as_str().unwrap(), x["value"].as_str().unwrap()))
            .collect();
        assert_eq!(
            pairs,
            [
                ("Genre", "Jazz"),
                ("Année", "2021"),
                ("Note", "7.2/10"),
                ("Écoutes", "3")
            ]
        );
        assert!(
            facts(
                &json!({"Type": "MusicArtist", "ProductionYear": 1990}),
                false
            )
            .is_empty()
        );
    }

    #[test]
    fn track_mapping() {
        let v = json!({
            "Id": "e1", "Type": "Audio", "Name": "Coda", "Album": "Sessions",
            "AlbumArtist": "Ensemble", "Artists": ["Ensemble", "Guest"],
            "IndexNumber": 3, "ParentIndexNumber": 1, "ProductionYear": 2021,
            "RunTimeTicks": 2_450_000_000i64, "Genres": ["Jazz"],
            "AlbumId": "a1", "AlbumPrimaryImageTag": "tag9",
            "MediaStreams": [{"Type": "Audio", "Codec": "FLAC", "SampleRate": 96000, "BitDepth": 24, "Channels": 2}]
        });
        let it = item(SERVER, &v, false).unwrap();
        assert_eq!(it["ref"], "t/e1");
        assert_eq!(it["kind"], "track");
        assert_eq!(it["artist"], "Ensemble, Guest");
        assert_eq!(it["subtitle"], "Ensemble, Guest · Sessions");
        assert_eq!(it["duration_ms"], 245_000);
        assert_eq!(it["track_no"], 3);
        assert_eq!(it["genre"], "Jazz");
        assert_eq!(
            it["format"],
            json!({"sample_rate": 96000, "bits": 24, "channels": 2, "codec": "flac"})
        );
        assert_eq!(
            it["art"],
            "http://jf/Items/a1/Images/Primary?maxHeight=600&quality=90&tag=tag9"
        );
    }

    #[test]
    fn album_and_unknown() {
        let v = json!({"Id": "a1", "Type": "MusicAlbum", "Name": "Sessions",
                       "AlbumArtist": "Ensemble", "ProductionYear": 2021, "ChildCount": 9});
        let it = item(SERVER, &v, false).unwrap();
        assert_eq!(it["subtitle"], "Ensemble · 2021");
        assert_eq!(it["track_count"], 9);
        assert_eq!(it["browsable"], true);
        assert!(it.get("art").is_none());
        assert!(
            item(
                SERVER,
                &json!({"Id": "m", "Type": "Movie", "Name": "x"}),
                false
            )
            .is_none()
        );
    }

    #[test]
    fn plans() {
        let usb = dac(&[44_100, 48_000, 88_200, 96_000], 24);
        assert_eq!(plan(&usb, Some(96_000), Some(24)), Plan::Direct);
        assert_eq!(plan(&usb, Some(44_100), Some(16)), Plan::Direct);
        assert_eq!(
            plan(&usb, Some(176_400), Some(24)),
            Plan::Flac {
                rate: 88_200,
                bits: 24
            }
        );
        assert_eq!(
            plan(&usb, Some(192_000), Some(32)),
            Plan::Wav {
                rate: 96_000,
                bits: 24
            }
        );
        let cd = dac(&[44_100], 16);
        assert_eq!(
            plan(&cd, Some(48_000), Some(24)),
            Plan::Wav {
                rate: 44_100,
                bits: 16
            }
        );
        assert_eq!(
            plan(&cd, Some(48_000), Some(16)),
            Plan::Flac {
                rate: 44_100,
                bits: 16
            }
        );
        // Right rate, too deep: only the depth changes.
        assert_eq!(
            plan(&cd, Some(44_100), Some(24)),
            Plan::Wav {
                rate: 44_100,
                bits: 16
            }
        );
        // The null sink, a PipeWire default: nothing to match.
        assert_eq!(
            plan(&Output::default(), Some(192_000), Some(24)),
            Plan::Direct
        );
        // Lossy files have no depth.
        assert_eq!(plan(&usb, Some(44_100), None), Plan::Direct);
    }
}
