//! The tools' second `ffprobe` (D26): which tags a source has, for what
//! Remove Metadata reports and what the other tools set again.
//!
//! [`probe_args`] asks for every tag of the container and of each stream,
//! each stream's kind and codec tag, the cover-art flag and the chapters'
//! ids. [`read`] runs it and hands each line, as it's read, to a
//! [`Parser`], which keeps the tag *keys*, sorted into [`MetaField`]s, and
//! only these values: a stream's language (`[a-z]{2,3}`, not `und`), the
//! container's `creation_time` (an ISO 8601 date, the first of a `;` list)
//! and whether an attachment's MIME type is a font. Titles, comments and
//! locations are never kept, so nothing a file says can reach a log or an
//! argv from here, and a large tag (an editor's XMP history) costs nothing
//! to read: only [`MAX_OUTPUT`] and the probe's timeout bound it.
//!
//! The keys are `-of flat`'s: every character but an ASCII letter or digit
//! is `_` (`com.apple.quicktime.make` is `com_apple_quicktime_make`), so a
//! key never holds `=` or a line break, and every value is quoted with its
//! line breaks escaped ([`super::probe`] has the format). So each line is
//! one tag, whatever the file's tags say. Checked against FFmpeg 6.1.1 and
//! Grape's 9.0.2, whose output differs only in 9.0's `stream_groups.*`
//! lines (ignored) and 6.1's `vendor_id` on every MP4 track.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use super::args::{file_url, DEMUXERS};
use super::probe::unquote;
use super::run::{self, RunError};
use crate::{CancelToken, MetaField};

/// How much the tags probe may print. Far more than any real file's tags
/// need, and only reading time, since nothing but keys is kept: past it,
/// the probe is stopped and what it said so far is used ([`Tags::cut`]).
pub(crate) const MAX_OUTPUT: u64 = 64 * 1024 * 1024;

/// The fields `-show_entries` asks for: every container and stream tag,
/// and only what's needed to judge them.
const ENTRIES: &str = "format_tags\
    :stream=index,codec_type,codec_tag_string\
    :stream_tags\
    :stream_disposition=attached_pic\
    :chapter=id";

/// The tags probe's `ffprobe` argv for `src` (an absolute path).
/// `export_xmp` is for the MOV family (MP4, MOV, M4A): its demuxer then
/// gives an XMP packet as the `xmp` tag, which it otherwise drops unseen.
pub(crate) fn probe_args(src: &Path, export_xmp: bool) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "-hide_banner",
        "-v",
        "error",
        "-protocol_whitelist",
        "file",
        "-format_whitelist",
        DEMUXERS,
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    if export_xmp {
        args.extend(["-export_xmp", "1"].map(OsString::from));
    }
    args.extend(["-show_entries", ENTRIES, "-of", "flat", "-i"].map(OsString::from));
    args.push(file_url(src));
    args
}

/// What the tags probe found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Tags {
    /// What Remove Metadata would remove and report: every tag with a
    /// value, by its key; cover art, data tracks, attachments that aren't
    /// fonts, and chapters.
    pub(crate) fields: BTreeSet<MetaField>,
    /// Each stream's language by its index, when it names one.
    pub(crate) languages: BTreeMap<u32, String>,
    /// The container's `creation_time`, which FFmpeg drops on every remux.
    pub(crate) creation_time: Option<String>,
    /// The container has `com.apple.quicktime.*` keys (an iPhone's
    /// location, make, model and date).
    pub(crate) apple_keys: bool,
    /// The indexes of attachments that are fonts (MKV subtitles need them).
    pub(crate) font_streams: BTreeSet<u32>,
    /// How many chapters there are.
    pub(crate) chapters: u32,
}

impl Tags {
    /// What a probe stopped at [`MAX_OUTPUT`] says: what it printed before
    /// that, and that there was more, unseen.
    fn cut(mut self) -> Tags {
        self.fields.insert(MetaField::Other);
        self
    }
}

/// Runs the tags probe on `src_abs` (an absolute path; `export_xmp` for the
/// MOV family, see [`probe_args`]) and parses it as it's read. An output
/// past [`MAX_OUTPUT`] isn't an error: the tags it printed before count,
/// and [`MetaField::Other`] stands for the rest ([`Tags::cut`]).
pub(crate) fn read(
    ffprobe: &Path,
    src_abs: &Path,
    export_xmp: bool,
    cancel: &CancelToken,
    timeout: Duration,
) -> Result<Tags, RunError> {
    let mut parser = Parser::default();
    let args = probe_args(src_abs, export_xmp);
    let ran = run::probe_lines(
        ffprobe,
        &args,
        cancel,
        timeout,
        MAX_OUTPUT,
        &mut |line, cut| {
            parser.line(line, cut);
        },
    );
    match ran {
        Ok(()) => Ok(parser.finish()),
        Err(RunError::TooMuch { .. }) => Ok(parser.finish().cut()),
        Err(e) => Err(e),
    }
}

/// Parses all of a tags probe's `-of flat` output at once.
#[cfg(test)]
pub(crate) fn parse(flat: &str) -> Tags {
    let mut parser = Parser::default();
    for line in flat.lines() {
        parser.line(line, false);
    }
    parser.finish()
}

/// What a value too long for [`run::probe_lines`] to hand over whole stands
/// for: one that isn't empty, and none of the values kept (a language, a
/// date, a MIME type) or compared (`Lavf`), so it's judged by its key.
const LONG: &str = "\u{2026}";

/// The tags probe's `-of flat` output, one line at a time. Never fails: a
/// line it can't read counts as [`MetaField::Other`].
#[derive(Debug, Default)]
pub(crate) struct Parser {
    tags: Tags,
    /// Each `streams.stream.N.` group by its N.
    streams: BTreeMap<u32, Stream>,
}

impl Parser {
    /// One line, without its line end; `cut` when only its start came
    /// ([`run::probe_lines`]).
    pub(crate) fn line(&mut self, line: &str, cut: bool) {
        if line.is_empty() {
            return;
        }
        let read = line.split_once('=').and_then(|(key, raw)| {
            let value = if cut { LONG.to_owned() } else { unquote(raw)? };
            Some((key, value))
        });
        let Some((key, value)) = read else {
            self.tags.fields.insert(MetaField::Other);
            return;
        };
        if let Some(name) = key.strip_prefix("format.tags.") {
            format_tag(&mut self.tags, name, &value);
        } else if let Some(rest) = key.strip_prefix("streams.stream.") {
            let Some((position, field)) = rest.split_once('.') else {
                return;
            };
            if let Ok(position) = position.parse::<u32>() {
                self.streams.entry(position).or_default().set(field, &value);
            }
        } else if let Some(rest) = key.strip_prefix("chapters.chapter.") {
            // One `id` line per chapter: counted, never listed.
            if let Some((Ok(_), "id")) = rest.split_once('.').map(|(n, f)| (n.parse::<u32>(), f)) {
                self.tags.chapters = self.tags.chapters.saturating_add(1);
            }
        }
        // Anything else (9.0's stream_groups.*) repeats the streams.
    }

    /// The tags of every line so far.
    pub(crate) fn finish(self) -> Tags {
        let mut tags = self.tags;
        for (position, stream) in self.streams {
            stream.into_tags(position, &mut tags);
        }
        if tags.chapters > 0 {
            tags.fields.insert(MetaField::Chapters);
        }
        tags
    }
}

/// One `format.tags.` line.
fn format_tag(tags: &mut Tags, name: &str, value: &str) {
    let key = name.to_ascii_lowercase();
    if key.starts_with(APPLE) {
        tags.apple_keys = true;
    }
    if key == "creation_time" && tags.creation_time.is_none() {
        tags.creation_time = iso_date(value);
    }
    if let Some(field) = field_of(&key, value) {
        tags.fields.insert(field);
    }
}

/// The prefix of an iPhone's QuickTime keys, as flat prints it.
const APPLE: &str = "com_apple_quicktime_";

/// The fields of one `streams.stream.N.` group.
#[derive(Debug, Default)]
struct Stream {
    index: Option<u32>,
    codec_type: String,
    codec_tag: String,
    attached_pic: bool,
    /// What its tags would report, but for `mimetype` and `filename`.
    fields: BTreeSet<MetaField>,
    language: Option<String>,
    /// From `mimetype`: a font, an image, or something else.
    attachment: Option<Attachment>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Attachment {
    Font,
    Image,
    Other,
}

impl Stream {
    fn set(&mut self, field: &str, value: &str) {
        match field {
            "index" => self.index = value.parse().ok(),
            "codec_type" => self.codec_type = value.to_owned(),
            "codec_tag_string" => self.codec_tag = value.to_ascii_lowercase(),
            "disposition.attached_pic" => self.attached_pic = value == "1",
            _ => {
                let Some(name) = field.strip_prefix("tags.") else {
                    return;
                };
                let key = name.to_ascii_lowercase();
                match key.as_str() {
                    "language" => self.language = language(value),
                    "mimetype" if !value.is_empty() => {
                        self.attachment = Some(attachment(value));
                    }
                    "mimetype" | "filename" => {}
                    _ => {
                        if let Some(field) = field_of(&key, value) {
                            self.fields.insert(field);
                        }
                    }
                }
            }
        }
    }

    /// Adds this stream (at `position` in the output) to `tags`. A cover
    /// art stream's own tags (an MP3 picture's "Cover (front)") are part
    /// of the cover art, not more fields.
    fn into_tags(self, position: u32, tags: &mut Tags) {
        let index = self.index.unwrap_or(position);
        if let Some(language) = self.language {
            tags.languages.insert(index, language);
        }
        if self.attached_pic {
            tags.fields.insert(MetaField::CoverArt);
            return;
        }
        tags.fields.extend(&self.fields);
        match self.codec_type.as_str() {
            "attachment" => match self.attachment {
                Some(Attachment::Font) => {
                    tags.font_streams.insert(index);
                }
                Some(Attachment::Image) => {
                    tags.fields.insert(MetaField::CoverArt);
                }
                Some(Attachment::Other) | None => {
                    tags.fields.insert(MetaField::Other);
                }
            },
            // Camera telemetry: GoPro's GPMF and the camera motion track
            // hold the position; a timecode track the time of day. A
            // QuickTime text track is the chapter track FFmpeg, iTunes and
            // HandBrake write in MP4 and M4A (M4B), whose chapters ffprobe
            // lists too.
            "data" => {
                tags.fields.insert(match self.codec_tag.as_str() {
                    "gpmd" | "camm" => MetaField::Location,
                    "tmcd" => MetaField::CreationDate,
                    "text" => MetaField::Chapters,
                    _ => MetaField::Other,
                });
            }
            _ => {}
        }
    }
}

/// `value` when it's a language code a track can keep: two or three
/// lowercase letters, not `und`.
fn language(value: &str) -> Option<String> {
    let code = (2..=3).contains(&value.len())
        && value.bytes().all(|b| b.is_ascii_lowercase())
        && value != "und";
    code.then(|| value.to_owned())
}

/// The first date of a `creation_time` (9.0 joins the `mvhd` date and an
/// iPhone's key with `;`), when it looks like ISO 8601 and nothing else.
fn iso_date(value: &str) -> Option<String> {
    let first = value.split(';').next().unwrap_or_default();
    let iso = !first.is_empty()
        && first.len() <= 40
        && first.starts_with(|c: char| c.is_ascii_digit())
        && first
            .bytes()
            .all(|b| b.is_ascii_digit() || b"T:.+-Z".contains(&b));
    iso.then(|| first.to_owned())
}

/// Whether an attachment's MIME type is a font, an image or other.
fn attachment(mime: &str) -> Attachment {
    let mime = mime.to_ascii_lowercase();
    let font = mime.starts_with("font/")
        || mime.starts_with("application/x-font-")
        || matches!(
            mime.as_str(),
            "application/x-truetype-font" | "application/vnd.ms-opentype" | "application/font-sfnt"
        );
    if font {
        Attachment::Font
    } else if mime.starts_with("image/") {
        Attachment::Image
    } else {
        Attachment::Other
    }
}

/// What a tag with this key (lowercase, as flat prints it) and value would
/// report, or `None` for one Remove Metadata doesn't report: an empty
/// value, or a key that only describes the file's structure (brands,
/// handler names, languages, Matroska's statistics) or says FFmpeg wrote it
/// (a bare `Lavf` or `Lavc`).
fn field_of(key: &str, value: &str) -> Option<MetaField> {
    if value.is_empty() {
        return None;
    }
    let key = key
        .strip_prefix(APPLE)
        .or_else(|| key.strip_prefix("com_android_"))
        .unwrap_or(key);
    match named(key, value) {
        // QuickTime and Matroska add a language to a key (`title-eng`):
        // the key without it says what it is.
        Some(MetaField::Other) => match key.rsplit_once('_') {
            Some((stem, lang)) if lang.len() == 3 && language(lang).is_some() => named(stem, value),
            _ => Some(MetaField::Other),
        },
        other => other,
    }
}

/// [`field_of`]'s table (D26, ENGINES.md → Metadata removal).
fn named(key: &str, value: &str) -> Option<MetaField> {
    use MetaField::*;
    let field = match key {
        "major_brand" | "minor_version" | "compatible_brands" | "handler_name" | "vendor_id"
        | "language" | "duration" | "bps" | "number_of_frames" | "number_of_bytes"
        | "_statistics_tags" => return None,
        "encoder" if matches!(value, "Lavf" | "Lavc") => return None,
        "title" => Title,
        "keywords" => Keywords,
        "copyright" => Copyright,
        "xmp" => Xmp,
        "artist" | "album_artist" | "author" | "composer" | "performer" => Author,
        "comment" | "description" | "synopsis" => Comment,
        "encoder"
        | "encoded_by"
        | "software"
        | "version"
        | "coding_history"
        | "_statistics_writing_app" => Software,
        "creation_time"
        | "date"
        | "year"
        | "timecode"
        | "time_reference"
        | "_statistics_writing_date_utc" => CreationDate,
        "make" | "model" | "manufacturer" | "firmware" => Camera,
        "xyz" => Location,
        _ if key.starts_with("location") || key.ends_with("iso6709") => Location,
        _ if key.ends_with("_make")
            || key.ends_with("_model")
            || key.starts_with("lens")
            || key.starts_with("camera")
            || key.contains("serial") =>
        {
            Camera
        }
        _ if key.starts_with("lyrics") => Comment,
        // Matroska's DATE_RECORDED, DATE_RELEASED, ...; BWF's
        // origination date and time.
        _ if key.ends_with("creationdate")
            || key.starts_with("date_")
            || key.starts_with("origination_") =>
        {
            CreationDate
        }
        _ => Other,
    };
    Some(field)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::media::run::tests::{script, Fake};
    use crate::media::run::MAX_LINE;
    use MetaField::*;

    // Golden text: the tags probe's output for files made with FFmpeg
    // 6.1.1 (lavfi sources), read by Grape's 9.0.2 ffprobe. 6.1.1's own
    // differs only where a test says so.

    /// `-metadata` title (with quotes, a backslash, `$`, backticks and
    /// `=`), artist, a comment with line breaks that spell out a fake tag
    /// line, creation_time, location, copyright and description, and track
    /// languages `und` and `eng`.
    const MP4_META: &str = r#"streams.stream.0.index=0
streams.stream.0.codec_type="video"
streams.stream.0.codec_tag_string="avc1"
streams.stream.0.disposition.attached_pic=0
streams.stream.0.tags.creation_time="2024-05-01T10:20:30.000000Z"
streams.stream.0.tags.language="und"
streams.stream.0.tags.handler_name="VideoHandler"
streams.stream.0.tags.encoder="Lavc60.31.102 libx264"
streams.stream.1.index=1
streams.stream.1.codec_type="audio"
streams.stream.1.codec_tag_string="mp4a"
streams.stream.1.disposition.attached_pic=0
streams.stream.1.tags.creation_time="2024-05-01T10:20:30.000000Z"
streams.stream.1.tags.language="eng"
streams.stream.1.tags.handler_name="SoundHandler"
format.tags.major_brand="isom"
format.tags.minor_version="512"
format.tags.compatible_brands="isomiso2avc1mp41"
format.tags.creation_time="2024-05-01T10:20:30.000000Z"
format.tags.title="Trip \"day 1\" \\ \$HOME \`x\` a=b"
format.tags.artist="Ann"
format.tags.encoder="Lavf60.16.100"
format.tags.comment="line1\nline2\r\nstreams.stream.0.tags.title=\"fake\""
format.tags.copyright="(c) Ann"
format.tags.description="A trip"
format.tags.location_eng="+48.8577+002.2950/"
format.tags.location="+48.8577+002.2950/"
"#;

    /// An iPhone-style MOV: `-movflags use_metadata_tags` with
    /// `com.apple.quicktime.` make, model, location, creation date and
    /// software, creation_time, and `-timecode` (a `tmcd` track).
    const IPHONE_MOV: &str = r#"stream_groups.stream_group.0.streams.stream.0.index=0
stream_groups.stream_group.0.streams.stream.0.codec_type="video"
stream_groups.stream_group.0.streams.stream.0.codec_tag_string="avc1"
stream_groups.stream_group.0.streams.stream.1.index=2
stream_groups.stream_group.0.streams.stream.1.codec_type="data"
stream_groups.stream_group.0.streams.stream.1.codec_tag_string="tmcd"
streams.stream.0.index=0
streams.stream.0.codec_type="video"
streams.stream.0.codec_tag_string="avc1"
streams.stream.0.disposition.attached_pic=0
streams.stream.0.tags.creation_time="2024-05-01T10:20:30.000000Z"
streams.stream.0.tags.handler_name="VideoHandler"
streams.stream.0.tags.vendor_id="FFMP"
streams.stream.0.tags.encoder="Lavc60.31.102 libx264"
streams.stream.0.tags.timecode="01:00:00:0"
streams.stream.1.index=1
streams.stream.1.codec_type="audio"
streams.stream.1.codec_tag_string="mp4a"
streams.stream.1.disposition.attached_pic=0
streams.stream.1.tags.creation_time="2024-05-01T10:20:30.000000Z"
streams.stream.1.tags.handler_name="SoundHandler"
streams.stream.1.tags.vendor_id="[0][0][0][0]"
streams.stream.2.index=2
streams.stream.2.codec_type="data"
streams.stream.2.codec_tag_string="tmcd"
streams.stream.2.disposition.attached_pic=0
streams.stream.2.tags.creation_time="2024-05-01T10:20:30.000000Z"
streams.stream.2.tags.language="eng"
streams.stream.2.tags.handler_name="TimeCodeHandler"
streams.stream.2.tags.timecode="01:00:00:0"
format.tags.major_brand="qt  "
format.tags.minor_version="512"
format.tags.compatible_brands="qt  "
format.tags.com_apple_quicktime_software="17.4.1"
format.tags.com_apple_quicktime_make="Apple"
format.tags.com_apple_quicktime_model="iPhone 15 Pro"
format.tags.com_apple_quicktime_location_ISO6709="+48.8577+002.2950+035.000/"
format.tags.com_apple_quicktime_creationdate="2024-05-01T12:20:30+0200"
format.tags.creation_time="2024-05-01T10:20:30.000000Z;2024-05-01T10:20:30.000000Z"
format.tags.timecode="01:00:00:00"
format.tags.encoder="Lavf60.16.100"
"#;

    /// ID3v2.3 with title, artist, album, date, genre, track, comment,
    /// TLAN and encoded_by, and a PNG cover (`attached_pic`) with its own
    /// title and comment. 6.1.1 prints the same.
    const SONG_MP3: &str = r#"streams.stream.0.index=0
streams.stream.0.codec_type="audio"
streams.stream.0.codec_tag_string="[0][0][0][0]"
streams.stream.0.disposition.attached_pic=0
streams.stream.1.index=1
streams.stream.1.codec_type="video"
streams.stream.1.codec_tag_string="[0][0][0][0]"
streams.stream.1.disposition.attached_pic=1
streams.stream.1.tags.title="Album cover"
streams.stream.1.tags.comment="Cover (front)"
format.tags.title="Song"
format.tags.artist="Band"
format.tags.album="LP"
format.tags.encoder="Lavf60.16.100"
format.tags.genre="Rock"
format.tags.track="3"
format.tags.comment="Nice"
format.tags.language="eng"
format.tags.encoded_by="Me"
format.tags.date="2023"
"#;

    /// Vorbis comments TITLE, ARTIST, DESCRIPTION (read as `comment`),
    /// REPLAYGAIN_TRACK_GAIN and LYRICS, and a PNG picture. 6.1.1 prints
    /// the same.
    const SONG_FLAC: &str = r#"streams.stream.0.index=0
streams.stream.0.codec_type="audio"
streams.stream.0.codec_tag_string="[0][0][0][0]"
streams.stream.0.disposition.attached_pic=0
streams.stream.1.index=1
streams.stream.1.codec_type="video"
streams.stream.1.codec_tag_string="[0][0][0][0]"
streams.stream.1.disposition.attached_pic=1
streams.stream.1.tags.comment="Cover (front)"
format.tags.TITLE="Song"
format.tags.ARTIST="Band"
format.tags.comment="Desc"
format.tags.REPLAYGAIN_TRACK_GAIN="-3.2 dB"
format.tags.LYRICS="la la"
format.tags.encoder="Lavf60.16.100"
"#;

    /// Video, English and French audio (the French one titled), English
    /// SRT subtitles, two chapters, global title, DATE_RECORDED and
    /// ARTIST, and two attachments: a TrueType font and a JPEG, which the
    /// Matroska demuxer turns into cover art. 6.1.1 prints the same.
    const MOVIE_MKV: &str = r#"streams.stream.0.index=0
streams.stream.0.codec_type="video"
streams.stream.0.codec_tag_string="[0][0][0][0]"
streams.stream.0.disposition.attached_pic=0
streams.stream.0.tags.ENCODER="Lavc60.31.102 libx264"
streams.stream.0.tags.DURATION="00:00:01.000000000"
streams.stream.1.index=1
streams.stream.1.codec_type="audio"
streams.stream.1.codec_tag_string="[0][0][0][0]"
streams.stream.1.disposition.attached_pic=0
streams.stream.1.tags.language="eng"
streams.stream.1.tags.ENCODER="Lavc60.31.102 aac"
streams.stream.1.tags.DURATION="00:00:01.023000000"
streams.stream.2.index=2
streams.stream.2.codec_type="audio"
streams.stream.2.codec_tag_string="[0][0][0][0]"
streams.stream.2.disposition.attached_pic=0
streams.stream.2.tags.language="fra"
streams.stream.2.tags.title="Commentary"
streams.stream.2.tags.ENCODER="Lavc60.31.102 aac"
streams.stream.2.tags.DURATION="00:00:01.023000000"
streams.stream.3.index=3
streams.stream.3.codec_type="subtitle"
streams.stream.3.codec_tag_string="[0][0][0][0]"
streams.stream.3.disposition.attached_pic=0
streams.stream.3.tags.language="eng"
streams.stream.3.tags.ENCODER="Lavc60.31.102 srt"
streams.stream.3.tags.DURATION="00:00:00.900000000"
streams.stream.4.index=4
streams.stream.4.codec_type="attachment"
streams.stream.4.codec_tag_string="[0][0][0][0]"
streams.stream.4.disposition.attached_pic=0
streams.stream.4.tags.filename="font.ttf"
streams.stream.4.tags.mimetype="application/x-truetype-font"
streams.stream.5.index=5
streams.stream.5.codec_type="video"
streams.stream.5.codec_tag_string="[0][0][0][0]"
streams.stream.5.disposition.attached_pic=1
streams.stream.5.tags.filename="cover.jpg"
streams.stream.5.tags.mimetype="image/jpeg"
chapters.chapter.0.id=1
chapters.chapter.1.id=2
format.tags.title="Movie night"
format.tags.DATE_RECORDED="2024"
format.tags.ARTIST="Director"
format.tags.ENCODER="Lavf60.16.100"
"#;

    /// H.264 and AAC with two chapters and a title (`-map_chapters` from an
    /// FFmpeg metadata file): the MP4 muxer writes the chapters as a
    /// QuickTime text track too. 6.1.1 adds a `vendor_id` to each track
    /// but the text track.
    const CHAPTERS_MP4: &str = r#"streams.stream.0.index=0
streams.stream.0.codec_type="video"
streams.stream.0.codec_tag_string="avc1"
streams.stream.0.disposition.attached_pic=0
streams.stream.0.tags.language="und"
streams.stream.0.tags.handler_name="VideoHandler"
streams.stream.0.tags.encoder="Lavc60.31.102 libx264"
streams.stream.1.index=1
streams.stream.1.codec_type="audio"
streams.stream.1.codec_tag_string="mp4a"
streams.stream.1.disposition.attached_pic=0
streams.stream.1.tags.language="und"
streams.stream.1.tags.handler_name="SoundHandler"
streams.stream.2.index=2
streams.stream.2.codec_type="data"
streams.stream.2.codec_tag_string="text"
streams.stream.2.disposition.attached_pic=0
streams.stream.2.tags.language="eng"
streams.stream.2.tags.handler_name="SubtitleHandler"
chapters.chapter.0.id=0
chapters.chapter.1.id=1
format.tags.major_brand="isom"
format.tags.minor_version="512"
format.tags.compatible_brands="isomiso2avc1mp41"
format.tags.title="Book"
format.tags.encoder="Lavf60.16.100"
"#;

    /// The same audio and chapters as an M4A (`-f ipod`), an audiobook's
    /// layout.
    const CHAPTERS_M4A: &str = r#"streams.stream.0.index=0
streams.stream.0.codec_type="audio"
streams.stream.0.codec_tag_string="mp4a"
streams.stream.0.disposition.attached_pic=0
streams.stream.0.tags.language="und"
streams.stream.0.tags.handler_name="SoundHandler"
streams.stream.1.index=1
streams.stream.1.codec_type="data"
streams.stream.1.codec_tag_string="text"
streams.stream.1.disposition.attached_pic=0
streams.stream.1.tags.language="eng"
streams.stream.1.tags.handler_name="SubtitleHandler"
chapters.chapter.0.id=0
chapters.chapter.1.id=1
format.tags.major_brand="M4A "
format.tags.minor_version="512"
format.tags.compatible_brands="M4A isomiso2"
format.tags.title="Book"
format.tags.encoder="Lavf60.16.100"
"#;

    /// [`MP4_META`] with a top-level XMP `uuid` box appended, read with
    /// `-export_xmp 1`: the format tags end with the packet.
    const XMP_TAG: &str = "format.tags.xmp=\"<x:xmpmeta xmlns:x=\\\"adobe:ns:meta/\\\">\
        <rdf:RDF xmlns:rdf=\\\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\\\">\
        <rdf:Description xmlns:exif=\\\"http://ns.adobe.com/exif/1.0/\\\" \
        exif:GPSLatitude=\\\"48,51.46N\\\"/></rdf:RDF></x:xmpmeta>\"\n";

    fn set<const N: usize>(fields: [MetaField; N]) -> BTreeSet<MetaField> {
        BTreeSet::from(fields)
    }

    #[test]
    fn probe_args_are_exact() {
        let src = Path::new(if cfg!(windows) {
            r"C:\Users\me\clip.mov"
        } else {
            "/home/me/clip.mov"
        });
        let mut url = OsString::from("file:");
        url.push(src);
        let argv = |xmp: &[&str]| {
            let mut expected: Vec<OsString> = [
                "-hide_banner",
                "-v",
                "error",
                "-protocol_whitelist",
                "file",
                "-format_whitelist",
                "mov,matroska,avi,asf,mp3,flac,ogg,wav,aiff,gif",
            ]
            .iter()
            .chain(xmp)
            .chain(&[
                "-show_entries",
                "format_tags:stream=index,codec_type,codec_tag_string:stream_tags\
                 :stream_disposition=attached_pic:chapter=id",
                "-of",
                "flat",
                "-i",
            ])
            .map(OsString::from)
            .collect();
            expected.push(url.clone());
            expected
        };
        assert_eq!(probe_args(src, false), argv(&[]));
        assert_eq!(probe_args(src, true), argv(&["-export_xmp", "1"]));
        assert!(!ENTRIES.contains(' '));
    }

    #[test]
    fn mp4_tags_by_key() {
        let tags = parse(MP4_META);
        assert_eq!(
            tags,
            Tags {
                fields: set([
                    Location,
                    Author,
                    Title,
                    Comment,
                    Copyright,
                    Software,
                    CreationDate
                ]),
                languages: BTreeMap::from([(1, "eng".to_owned())]),
                creation_time: Some("2024-05-01T10:20:30.000000Z".to_owned()),
                apple_keys: false,
                font_streams: BTreeSet::new(),
                chapters: 0,
            }
        );
        // 6.1.1 adds a vendor_id to each track, which isn't reported.
        let older = MP4_META.replace(
            "streams.stream.1.tags.handler_name=\"SoundHandler\"\n",
            "streams.stream.1.tags.handler_name=\"SoundHandler\"\n\
             streams.stream.1.tags.vendor_id=\"[0][0][0][0]\"\n",
        );
        assert_eq!(parse(&older), tags);
        // The line breaks in the comment stay inside its value: no fake
        // stream tag, no line that isn't one.
        assert!(!tags.fields.contains(&Other));
    }

    #[test]
    fn iphone_keys_are_found_and_reported() {
        let tags = parse(IPHONE_MOV);
        assert_eq!(tags.fields, set([Location, Camera, Software, CreationDate]));
        assert!(tags.apple_keys);
        // 9.0 joins the two creation dates with ';': the first is kept.
        assert_eq!(
            tags.creation_time.as_deref(),
            Some("2024-05-01T10:20:30.000000Z")
        );
        // The timecode track's language; the stream_groups lines add
        // nothing.
        assert_eq!(tags.languages, BTreeMap::from([(2, "eng".to_owned())]));
        let without_groups: String = IPHONE_MOV
            .lines()
            .filter(|l| !l.starts_with("stream_groups."))
            .map(|l| format!("{l}\n"))
            .collect();
        assert_eq!(parse(&without_groups), tags);
    }

    #[test]
    fn mp3_cover_art_counts_once_without_its_own_tags() {
        let tags = parse(SONG_MP3);
        // The picture's title and comment are the cover art's; the
        // album's comment is the file's.
        assert_eq!(
            tags.fields,
            set([
                Author,
                Title,
                Comment,
                Software,
                CreationDate,
                CoverArt,
                Other
            ])
        );
        // TLAN is a container tag, not a track's language.
        assert!(tags.languages.is_empty());
        let no_album = SONG_MP3
            .replace("format.tags.album=\"LP\"\n", "")
            .replace("format.tags.genre=\"Rock\"\n", "")
            .replace("format.tags.track=\"3\"\n", "");
        assert!(!parse(&no_album).fields.contains(&Other));
        let picture_only = "streams.stream.1.codec_type=\"video\"\n\
                            streams.stream.1.disposition.attached_pic=1\n\
                            streams.stream.1.tags.comment=\"Cover (front)\"\n\
                            format.tags.encoder=\"Lavf\"\n";
        assert_eq!(parse(picture_only).fields, set([CoverArt]));
    }

    #[test]
    fn flac_vorbis_comments_in_any_case() {
        let tags = parse(SONG_FLAC);
        // ReplayGain is "other fields".
        assert_eq!(
            tags.fields,
            set([Author, Title, Comment, Software, CoverArt, Other])
        );
        assert_eq!(tags.creation_time, None);
        assert!(!tags.apple_keys);
    }

    #[test]
    fn mkv_chapters_languages_fonts_and_cover() {
        let tags = parse(MOVIE_MKV);
        assert_eq!(
            tags,
            Tags {
                fields: set([Author, Title, Software, CreationDate, CoverArt, Chapters]),
                languages: BTreeMap::from([
                    (1, "eng".to_owned()),
                    (2, "fra".to_owned()),
                    (3, "eng".to_owned())
                ]),
                creation_time: None,
                apple_keys: false,
                font_streams: BTreeSet::from([4]),
                chapters: 2,
            }
        );
        // An attachment that's neither a font nor an image, or has no type.
        for mime in ["application/pdf", ""] {
            let other = MOVIE_MKV.replace("application/x-truetype-font", mime);
            let tags = parse(&other);
            assert!(tags.fields.contains(&Other), "{mime:?}");
            assert!(tags.font_streams.is_empty(), "{mime:?}");
        }
        // One that stays an attachment although it's an image.
        let image = MOVIE_MKV.replace("application/x-truetype-font", "image/webp");
        let tags = parse(&image);
        assert!(tags.font_streams.is_empty());
        assert!(!tags.fields.contains(&Other));
        for font in [
            "font/otf",
            "FONT/TTF",
            "application/vnd.ms-opentype",
            "application/font-sfnt",
            "application/x-font-ttf",
            "application/x-font-otf",
        ] {
            let tags = parse(&MOVIE_MKV.replace("application/x-truetype-font", font));
            assert_eq!(tags.font_streams, BTreeSet::from([4]), "{font}");
        }
    }

    #[test]
    fn an_mp4_chapter_track_is_its_chapters() {
        for flat in [CHAPTERS_MP4, CHAPTERS_M4A] {
            let tags = parse(flat);
            // Not "other fields": the text track is the chapters.
            assert_eq!(tags.fields, set([Title, Software, Chapters]));
            assert_eq!(tags.chapters, 2);
            let older = flat.replace(
                "tags.handler_name=\"SoundHandler\"\n",
                "tags.handler_name=\"SoundHandler\"\n\
                 streams.stream.1.tags.vendor_id=\"[0][0][0][0]\"\n",
            );
            assert_eq!(parse(&older), tags);
        }
        // The track's language is a stream's like any other.
        let tags = parse(CHAPTERS_MP4);
        assert_eq!(tags.languages, BTreeMap::from([(2, "eng".to_owned())]));
    }

    #[test]
    fn exported_xmp_is_reported() {
        let tags = parse(&format!("{MP4_META}{XMP_TAG}"));
        assert!(tags.fields.contains(&Xmp));
        assert!(!tags.fields.contains(&Other));
    }

    #[test]
    fn data_tracks_say_what_they_hold() {
        let data = |tag: &str| {
            format!(
                "streams.stream.0.index=0\n\
                 streams.stream.0.codec_type=\"data\"\n\
                 streams.stream.0.codec_tag_string=\"{tag}\"\n\
                 streams.stream.0.tags.handler_name=\"DataHandler\"\n"
            )
        };
        assert_eq!(parse(&data("gpmd")).fields, set([Location]));
        assert_eq!(parse(&data("camm")).fields, set([Location]));
        assert_eq!(parse(&data("tmcd")).fields, set([CreationDate]));
        assert_eq!(parse(&data("text")).fields, set([Chapters]));
        assert_eq!(parse(&data("mebx")).fields, set([Other]));
        assert_eq!(parse(&data("[0][0][0][0]")).fields, set([Other]));
    }

    #[test]
    fn keys_sort_into_fields() {
        let cases = [
            ("location", Some(Location)),
            ("location_accuracy_horizontal", Some(Location)),
            ("com_apple_quicktime_location_iso6709", Some(Location)),
            ("xyz", Some(Location)),
            ("make", Some(Camera)),
            ("model", Some(Camera)),
            ("com_apple_quicktime_camera_identifier", Some(Camera)),
            ("com_android_manufacturer", Some(Camera)),
            ("lens_model", Some(Camera)),
            ("camera_serial_number", Some(Camera)),
            ("firmware", Some(Camera)),
            ("artist", Some(Author)),
            ("album_artist", Some(Author)),
            ("composer", Some(Author)),
            ("performer", Some(Author)),
            ("author", Some(Author)),
            ("title", Some(Title)),
            ("title_eng", Some(Title)),
            ("keywords", Some(Keywords)),
            ("synopsis", Some(Comment)),
            ("lyrics_eng", Some(Comment)),
            ("copyright", Some(Copyright)),
            ("encoded_by", Some(Software)),
            ("com_android_version", Some(Software)),
            ("coding_history", Some(Software)),
            ("_statistics_writing_app", Some(Software)),
            ("date", Some(CreationDate)),
            ("year", Some(CreationDate)),
            ("date_released", Some(CreationDate)),
            ("origination_date", Some(CreationDate)),
            ("time_reference", Some(CreationDate)),
            ("_statistics_writing_date_utc", Some(CreationDate)),
            ("xmp", Some(Xmp)),
            ("album", Some(Other)),
            ("genre", Some(Other)),
            ("replaygain_album_peak", Some(Other)),
            ("itunnorm", Some(Other)),
            ("umid", Some(Other)),
            ("originator", Some(Other)),
            ("com_apple_quicktime_content_identifier", Some(Other)),
            ("album_xyz1", Some(Other)),
            ("major_brand", None),
            ("compatible_brands", None),
            ("handler_name", None),
            ("vendor_id", None),
            ("language", None),
            ("duration", None),
            ("bps", None),
            ("number_of_frames", None),
            ("number_of_bytes", None),
            ("_statistics_tags", None),
        ];
        for (key, field) in cases {
            assert_eq!(field_of(key, "x"), field, "{key}");
        }
        // FFmpeg's bare name isn't reported; with a version it's software.
        assert_eq!(field_of("encoder", "Lavf"), None);
        assert_eq!(field_of("encoder", "Lavc"), None);
        assert_eq!(field_of("encoder", "Lavf60.16.100"), Some(Software));
        assert_eq!(field_of("encoder", "lavf"), Some(Software));
        // An empty value has nothing to remove.
        assert_eq!(field_of("title", ""), None);
    }

    #[test]
    fn only_codes_and_dates_are_kept() {
        for (value, kept) in [
            ("eng", true),
            ("de", true),
            ("und", false),
            ("ENG", false),
            ("en-US", false),
            ("e", false),
            ("engl", false),
            ("\u{e9}n", false),
        ] {
            assert_eq!(language(value).is_some(), kept, "{value:?}");
        }
        for (value, kept) in [
            (
                "2024-05-01T10:20:30.000000Z",
                Some("2024-05-01T10:20:30.000000Z"),
            ),
            ("2024-05-01T12:20:30+0200", Some("2024-05-01T12:20:30+0200")),
            (
                "2024-05-01T10:20:30Z;2024-05-01T10:20:30Z",
                Some("2024-05-01T10:20:30Z"),
            ),
            ("", None),
            (";2024", None),
            ("now", None),
            ("2024-05-01 10:20:30", None),
            ("2024-05-01T10:20:30Z -metadata x=y", None),
            ("-1", None),
        ] {
            assert_eq!(iso_date(value).as_deref(), kept, "{value:?}");
        }
        let long = "1".repeat(41);
        assert_eq!(iso_date(&long), None);
    }

    #[test]
    fn hostile_output_is_read_line_by_line() {
        // Lines that aren't key=value, or whose quoting is broken, are
        // something Grape can't name.
        let broken = "format.tags.title=\"unterminated\nformat.tags.x\n";
        assert_eq!(parse(broken).fields, set([Other]));
        // Huge or odd stream and chapter numbers are ignored or counted,
        // never trusted as sizes.
        let odd = "streams.stream.99999999999.tags.title=\"x\"\n\
                   streams.stream.4294967295.index=4294967295\n\
                   streams.stream.4294967295.tags.language=\"deu\"\n\
                   streams.stream.7.index=-1\n\
                   streams.stream.7.tags.title=\"y\"\n\
                   chapters.chapter.4294967295.id=1\n\
                   chapters.chapter.x.id=2\n\
                   streams.stream=\"no field\"\n";
        let tags = parse(odd);
        assert_eq!(tags.fields, set([Title, Chapters]));
        assert_eq!(
            tags.languages,
            BTreeMap::from([(u32::MAX, "deu".to_owned())])
        );
        assert_eq!(tags.chapters, 1);
        // Many chapters count up without a list per chapter.
        let many: String = (0..10_000)
            .map(|n| format!("chapters.chapter.{n}.id={n}\n"))
            .collect();
        assert_eq!(parse(&many).chapters, 10_000);
    }

    /// [`read`] on a source, with its ffprobe run scripted.
    fn read_scripted(fake: Fake) -> Result<Tags, RunError> {
        script([fake]);
        let src = std::env::temp_dir().join("grape-tags-unit.mkv");
        let ffprobe = std::env::temp_dir().join("grape-no-ffprobe");
        read(
            &ffprobe,
            &src,
            false,
            &CancelToken::new(),
            Duration::from_secs(20),
        )
    }

    #[test]
    fn the_probe_is_parsed_as_it_is_read() {
        let tags = read_scripted(Fake::Prints(MOVIE_MKV.to_owned())).unwrap();
        assert_eq!(tags, parse(MOVIE_MKV));
        assert_eq!(parse(""), Tags::default());
        // A value too long to hand over whole is judged by its key.
        let long = "x".repeat(MAX_LINE * 3);
        let flat = format!(
            "streams.stream.0.index=0\n\
             streams.stream.0.codec_type=\"audio\"\n\
             streams.stream.0.tags.language=\"e{long}\"\n\
             streams.stream.1.index=1\n\
             streams.stream.1.codec_type=\"attachment\"\n\
             streams.stream.1.tags.mimetype=\"font/{long}\"\n\
             format.tags.creation_time=\"2024{long}\"\n\
             format.tags.encoder=\"Lavf{long}\"\n\
             format.tags.xmp=\"<x:xmpmeta {long}\"\n"
        );
        let tags = read_scripted(Fake::Prints(flat)).unwrap();
        assert_eq!(tags.fields, set([Software, CreationDate, Xmp, Other]));
        assert!(tags.languages.is_empty());
        assert!(tags.font_streams.is_empty());
        assert_eq!(tags.creation_time, None);
        // A long line without '=' in its start is something Grape can't name.
        let mut parser = Parser::default();
        parser.line(&long[..MAX_LINE], true);
        assert_eq!(parser.finish().fields, set([Other]));
    }

    #[test]
    fn a_probe_past_its_limit_keeps_what_it_read_and_says_other() {
        // A child that prints a stream, then the same tag until it's
        // stopped. (The test harness's own lines around a fake child's
        // aren't tags either.)
        let started = std::time::Instant::now();
        let tags = read_scripted(Fake::Child("tags-flood", String::new())).unwrap();
        assert!(started.elapsed() < Duration::from_secs(15));
        assert_eq!(tags.languages, BTreeMap::from([(0, "deu".to_owned())]));
        assert_eq!(tags.fields, set([Title, Other]));
        // Any other failure is the caller's to judge.
        let limit = RunError::TooMuch { limit: MAX_OUTPUT };
        assert_eq!(
            read_scripted(Fake::Fails(limit)).unwrap().fields,
            set([Other])
        );
        let err = read_scripted(Fake::Fails(RunError::TimedOut)).unwrap_err();
        assert!(matches!(err, RunError::TimedOut), "{err:?}");
    }
}
