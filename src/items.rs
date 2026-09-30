//! Jellyfin items → ricercar items, and the stream format choice.
//!
//! Refs are `<prefix>/<jellyfin id>`: `t` track, `a` album, `r` artist,
//! `p` playlist, `f` folder or library. Top-level sections use bare words
//! (`albums`, `artists`…).

use serde_json::{Value, json};

/// Kind prefix and id of a ref, for refs that point at a Jellyfin item.
pub fn split_ref(r: &str) -> Option<(&str, &str)> {
    let (k, id) = r.split_once('/')?;
    let ok = matches!(k, "t" | "a" | "r" | "p" | "f")
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

/// One ricercar item, or `None` for types a music player has no use for.
pub fn item(server: &str, v: &Value) -> Option<Value> {
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
        _ => return None,
    };
    if let Some(a) = art(server, v) {
        it["art"] = a.into();
    }
    // Leave optional fields out rather than send nulls.
    if let Some(o) = it.as_object_mut() {
        o.retain(|_, v| !v.is_null());
    }
    Some(it)
}

pub fn items(server: &str, list: &Value) -> Vec<Value> {
    list["Items"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| item(server, v)).collect())
        .unwrap_or_default()
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
        assert_eq!(split_ref("x/abc"), None);
        assert_eq!(split_ref("t/../x"), None);
        assert_eq!(split_ref("albums"), None);
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
        let it = item(SERVER, &v).unwrap();
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
        let it = item(SERVER, &v).unwrap();
        assert_eq!(it["subtitle"], "Ensemble · 2021");
        assert_eq!(it["track_count"], 9);
        assert_eq!(it["browsable"], true);
        assert!(it.get("art").is_none());
        assert!(item(SERVER, &json!({"Id": "m", "Type": "Movie", "Name": "x"})).is_none());
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
