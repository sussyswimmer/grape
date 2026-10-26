//! The one name validator every archive entry goes through, for extracting
//! and for converting alike (DECISIONS D27). Grape never uses the crates'
//! own helpers for this: zip 9.0.2's `enclosed_name` lets `\\?\C:\../b`
//! escape, strips absolute prefixes instead of refusing them, and accepts
//! Windows device names and alternate data streams.
//!
//! Windows' rules apply on every OS, so an archive is judged the same
//! everywhere and the tests are deterministic.

use std::collections::HashMap;

/// The characters no Windows file name may hold, besides the controls.
/// `:` also covers drive letters (`C:`) and alternate data streams.
const RESERVED_CHARS: [char; 7] = ['<', '>', ':', '"', '|', '?', '*'];

/// Windows device names, refused with or without an extension and in any
/// case (`NUL.txt` and `nul.tar.gz` are the device too). The digits include
/// 0 and the superscripts, as Microsoft's naming page lists them.
const DEVICES: [&str; 6] = ["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"];

/// The most characters of an entry name a user message shows.
const SHOWN_CHARS: usize = 120;

/// The longest entry name (or link target) Grape accepts, in UTF-8 bytes:
/// Linux's `PATH_MAX`. It also bounds the work one name costs.
const MAX_NAME_BYTES: usize = 4096;

/// The longest component, in UTF-16 units: what NTFS can hold. Counted in
/// UTF-16, not bytes, so long CJK names that Windows accepts aren't refused.
const MAX_COMPONENT_UNITS: usize = 255;

/// A name split into safe components.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Parsed {
    /// The components, `.` dropped. Empty for the archive's root (`./`).
    pub(crate) path: Vec<String>,
    /// The name ended in `/` or `\`, which marks a directory.
    pub(crate) trailing_slash: bool,
}

/// Splits an entry name on `/` and `\` and checks every component. `None`
/// for a name Grape refuses: empty; over [`MAX_NAME_BYTES`]; absolute (`/x`,
/// `\x`, which covers UNC `\\srv\x` and `\\?\`); with an empty component in
/// the middle (`a//b`); with a `..` component; or with a component
/// [`component_ok`] refuses.
pub(crate) fn parse(name: &str) -> Option<Parsed> {
    if name.is_empty() || name.len() > MAX_NAME_BYTES || name.starts_with(['/', '\\']) {
        return None;
    }
    let parts: Vec<&str> = name.split(['/', '\\']).collect();
    let last = parts.len() - 1;
    let mut path = Vec::with_capacity(parts.len());
    let mut trailing_slash = false;
    for (i, part) in parts.iter().enumerate() {
        match *part {
            "" if i == last => trailing_slash = true,
            "" | ".." => return None,
            "." => {}
            other if component_ok(other) => path.push(other.to_string()),
            _ => return None,
        }
    }
    Some(Parsed {
        path,
        trailing_slash,
    })
}

/// True for a component Windows can create and that means nothing
/// special: at most [`MAX_COMPONENT_UNITS`] long, no control characters,
/// none of `< > : " | ? *`, no trailing `.` or space, and not a device
/// name. `.` and `..` are refused here too; [`parse`] handles them before.
pub(crate) fn component_ok(part: &str) -> bool {
    !part.is_empty()
        && part.encode_utf16().nth(MAX_COMPONENT_UNITS).is_none()
        && !part
            .chars()
            .any(|c| c.is_control() || RESERVED_CHARS.contains(&c))
        && !part.ends_with(['.', ' '])
        && !is_device(part)
}

/// True for a Windows device name, with or without an extension.
fn is_device(part: &str) -> bool {
    let base = part.split('.').next().unwrap_or(part).trim_end_matches(' ');
    let upper = base.to_uppercase();
    if DEVICES.contains(&upper.as_str()) {
        return true;
    }
    let mut chars = upper.chars();
    let prefix: String = chars.by_ref().take(3).collect();
    let rest: Vec<char> = chars.collect();
    (prefix == "COM" || prefix == "LPT")
        && matches!(rest.as_slice(), [d] if d.is_ascii_digit() || matches!(d, '¹' | '²' | '³'))
}

/// True for an entry macOS adds to the archives it makes: anything under
/// `__MACOSX/`, and AppleDouble `._name` files.
pub(crate) fn is_mac_metadata(path: &[String]) -> bool {
    path.first()
        .is_some_and(|first| first.eq_ignore_ascii_case("__MACOSX"))
        || path.last().is_some_and(|last| last.starts_with("._"))
}

/// A symlink's target, checked as a name would be and resolved from the
/// link's folder: it must stay inside the archive. `..` is accepted only as
/// a leading run: after a normal component it could step out of a folder
/// that is itself a link (`up` → `../..`, then `up/../../x`), which the
/// OS follows before applying the `..`. Returns the target with `/`
/// separators and `.` dropped, or `None` for one Grape leaves out
/// (absolute, empty, over [`MAX_NAME_BYTES`], escaping, `..` after a
/// normal component, or with a component [`component_ok`] refuses).
pub(crate) fn link_target(link: &[String], target: &str) -> Option<String> {
    if target.is_empty() || target.len() > MAX_NAME_BYTES || target.starts_with(['/', '\\']) {
        return None;
    }
    let parts: Vec<&str> = target.split(['/', '\\']).collect();
    let last = parts.len() - 1;
    // How many folders the link's own folder lies below the archive's root;
    // the leading `..` run may climb at most that far.
    let mut depth = link.len().checked_sub(1)?;
    let mut descended = false;
    let mut out = Vec::with_capacity(parts.len());
    for (i, part) in parts.iter().enumerate() {
        match *part {
            "" if i == last => {}
            "" => return None,
            "." => {}
            ".." if descended => return None,
            ".." => {
                depth = depth.checked_sub(1)?;
                out.push("..");
            }
            other if component_ok(other) => {
                descended = true;
                out.push(other);
            }
            _ => return None,
        }
    }
    if out.is_empty() {
        return None;
    }
    Some(out.join("/"))
}

/// The collision key: names that differ only in case are the same name.
/// The same fold `grape_core` plans output names with (lower, upper,
/// lower), so `STRAẞE` and `strasse` collide. It can over-match, which only
/// ever refuses an archive, never lets two names share a file.
fn fold(part: &str) -> String {
    part.to_lowercase().to_uppercase().to_lowercase()
}

/// What a path in [`NameSet`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    /// A folder only implied by an entry inside it.
    Implied,
    /// A folder an entry named.
    Dir,
    /// Anything else: a file, a link, a special file.
    File,
}

/// One path in a [`NameSet`]: its id (its children's parent) and what it is.
#[derive(Debug, Clone, Copy)]
struct Node {
    id: usize,
    slot: Slot,
}

/// What [`NameSet::insert`] did with a path it accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Added {
    /// The path is new (or was a folder only implied until now).
    New,
    /// The path is a folder an earlier entry already named.
    FolderAgain,
}

/// Why [`NameSet::insert`] refused a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Clash {
    /// Two items with one name, or a file where a folder must be: the
    /// clashing path as the archive spells it.
    Duplicate(String),
    /// More paths, implied folders included, than the set may hold.
    TooMany,
}

/// The paths an archive writes, compared case-insensitively, to refuse an
/// archive with two items of the same name ("A.txt" and "a.txt", which
/// NTFS and APFS would make one file) and one with a file where a folder
/// must be ("a" and "a/b").
///
/// Each path is keyed by its parent's id and its own folded component, so
/// an insert costs time and memory in step with its name, however deep.
/// Every path counts against the set's bound, the folders that entries
/// only imply too.
#[derive(Debug)]
pub(crate) struct NameSet {
    nodes: HashMap<(usize, String), Node>,
    max_paths: u64,
}

impl NameSet {
    /// An empty set that holds at most `max_paths` paths.
    pub(crate) fn new(max_paths: u64) -> NameSet {
        NameSet {
            nodes: HashMap::new(),
            max_paths,
        }
    }

    /// Adds `path` and every folder above it. A folder named twice is
    /// accepted ([`Added::FolderAgain`]); any other repeat, and a file where
    /// a folder must be, is a [`Clash::Duplicate`].
    pub(crate) fn insert(&mut self, path: &[String], is_dir: bool) -> Result<Added, Clash> {
        // The archive's root is 0; paths are numbered from 1.
        let mut parent = 0;
        for (i, part) in path.iter().enumerate() {
            let last = i + 1 == path.len();
            let clash = || Clash::Duplicate(path[..=i].join("/"));
            let key = (parent, fold(part));
            match self.nodes.get_mut(&key) {
                None => {
                    if self.nodes.len() as u64 >= self.max_paths {
                        return Err(Clash::TooMany);
                    }
                    let id = self.nodes.len() + 1;
                    let slot = match (last, is_dir) {
                        (false, _) => Slot::Implied,
                        (true, true) => Slot::Dir,
                        (true, false) => Slot::File,
                    };
                    self.nodes.insert(key, Node { id, slot });
                    parent = id;
                }
                Some(node) => match (node.slot, last) {
                    (Slot::File, _) => return Err(clash()),
                    (_, false) => parent = node.id,
                    (_, true) if !is_dir => return Err(clash()),
                    (Slot::Implied, true) => {
                        node.slot = Slot::Dir;
                        return Ok(Added::New);
                    }
                    (Slot::Dir, true) => return Ok(Added::FolderAgain),
                },
            }
        }
        Ok(Added::New)
    }
}

/// `name` as a user message may show it: control characters replaced and
/// at most [`SHOWN_CHARS`] characters.
pub(crate) fn shown(name: &str) -> String {
    let mut out: String = name
        .chars()
        .take(SHOWN_CHARS)
        .map(|c| if c.is_control() { '\u{FFFD}' } else { c })
        .collect();
    if name.chars().count() > SHOWN_CHARS {
        out.push('…');
    }
    out
}

/// Windows-1252's characters for the bytes 0x80 to 0x9F, where Latin-1 has
/// C1 control characters. Its five unassigned bytes read as U+FFFD.
const CP1252_HIGH: [char; 32] = [
    '€', '\u{FFFD}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{FFFD}', 'Ž',
    '\u{FFFD}', '\u{FFFD}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{FFFD}',
    'ž', 'Ÿ',
];

/// An entry name's bytes as text: UTF-8 when they are valid UTF-8,
/// otherwise Windows-1252 (old tars, gzip's FNAME). Read as Latin-1, as RFC
/// 1952 says for FNAME, the bytes 0x80 to 0x9F would be control characters
/// and refuse the whole job; real names with them come from Windows, and
/// Shift-JIS lead bytes fall there too (those come out garbled, not
/// refused).
pub(crate) fn decode(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        Err(_) => bytes
            .iter()
            .map(|&b| match b {
                0x80..=0x9F => CP1252_HIGH[usize::from(b - 0x80)],
                _ => char::from(b),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn path(name: &str) -> Vec<String> {
        parse(name).unwrap().path
    }

    #[test]
    fn keeps_ordinary_names() {
        assert_eq!(path("a.txt"), ["a.txt"]);
        assert_eq!(path("dir/sub/ảnh chụp.jpg"), ["dir", "sub", "ảnh chụp.jpg"]);
        assert_eq!(path("dir\\win\\x.txt"), ["dir", "win", "x.txt"]);
        assert_eq!(path("./a/./b"), ["a", "b"]);
        assert_eq!(path(".hidden"), [".hidden"]);
        assert_eq!(path("x.TGZ"), ["x.TGZ"]);
        assert_eq!(path("CONFIG.sys"), ["CONFIG.sys"]);
        assert_eq!(path("COM10"), ["COM10"]);
        assert_eq!(path("nulls.txt"), ["nulls.txt"]);
        let dir = parse("empty/").unwrap();
        assert_eq!(dir.path, ["empty"]);
        assert!(dir.trailing_slash);
        assert!(!parse("a").unwrap().trailing_slash);
        // The archive's own root.
        assert!(parse("./").unwrap().path.is_empty());
    }

    #[test]
    fn refuses_traversal_and_absolute_names() {
        for name in [
            "",
            "../x",
            "/etc/x",
            "\\etc\\x",
            "C:\\x",
            "C:x",
            "c:/x",
            "\\\\srv\\share\\x",
            "//srv/share/x",
            "\\\\?\\C:\\../b",
            "\\\\?\\C:\\a/../../b",
            "a/../../x",
            "a/..",
            "a\\..\\..\\b.txt",
            "a//b",
            "..",
        ] {
            assert_eq!(parse(name), None, "{name:?}");
        }
    }

    #[test]
    fn refuses_names_windows_cannot_hold() {
        for name in [
            "CON",
            "con",
            "Con.txt",
            "x/NUL.tar.gz",
            "PRN.",
            "aux.c",
            "COM1",
            "com9.log",
            "LPT1",
            "lpt0",
            "COM¹",
            "CONIN$",
            "conout$.txt",
            "CON .txt",
            "trail.",
            "trail. ",
            "space ",
            "dir./x",
            "a.txt:stream",
            "a\0b",
            "line\nbreak",
            "tab\tname",
            "q?.txt",
            "star*",
            "pipe|",
            "<x>",
            "quote\"",
        ] {
            assert_eq!(parse(name), None, "{name:?}");
        }
    }

    #[test]
    fn spots_mac_metadata() {
        assert!(is_mac_metadata(&path("__MACOSX/a/._b")));
        assert!(is_mac_metadata(&path("__macosx/x")));
        assert!(is_mac_metadata(&path("dir/._photo.jpg")));
        assert!(!is_mac_metadata(&path("dir/.photo.jpg")));
        assert!(!is_mac_metadata(&path("x/__MACOSX")));
    }

    #[test]
    fn refuses_names_too_long_for_a_volume() {
        // A component: at most 255 UTF-16 units, so 255 CJK characters
        // (765 UTF-8 bytes) are fine and one more is not.
        assert!(parse(&"x".repeat(255)).is_some());
        assert_eq!(parse(&"x".repeat(256)), None);
        assert!(parse(&"漢".repeat(255)).is_some());
        assert_eq!(parse(&format!("a/{}", "漢".repeat(256))), None);
        // A character outside the BMP is two UTF-16 units.
        assert!(parse(&"😀".repeat(127)).is_some());
        assert_eq!(parse(&"😀".repeat(128)), None);
        // The whole name: at most 4096 bytes, however it is split.
        let deep = vec!["a"; 2048].join("/");
        assert_eq!(deep.len(), 4095);
        assert_eq!(parse(&deep).unwrap().path.len(), 2048);
        assert_eq!(parse(&format!("{deep}/b")), None);
    }

    #[test]
    fn link_targets_must_stay_inside() {
        let link = path("a/b/link");
        assert_eq!(link_target(&link, "c.txt").as_deref(), Some("c.txt"));
        assert_eq!(link_target(&link, "../x").as_deref(), Some("../x"));
        assert_eq!(link_target(&link, "../../x").as_deref(), Some("../../x"));
        assert_eq!(
            link_target(&link, "../../x/y").as_deref(),
            Some("../../x/y")
        );
        assert_eq!(link_target(&link, "./../d/./e").as_deref(), Some("../d/e"));
        assert_eq!(link_target(&link, "sub\\f").as_deref(), Some("sub/f"));
        assert_eq!(link_target(&link, "../../../x"), None);
        assert_eq!(link_target(&path("top"), "../x"), None);
        assert_eq!(link_target(&link, "/etc/passwd"), None);
        assert_eq!(link_target(&link, "C:\\x"), None);
        assert_eq!(link_target(&link, ""), None);
        assert_eq!(link_target(&link, "."), None);
        assert_eq!(link_target(&link, "a//b"), None);
        assert_eq!(link_target(&link, "NUL"), None);
        assert_eq!(link_target(&link, &"x".repeat(MAX_NAME_BYTES + 1)), None);
    }

    #[test]
    fn link_targets_climb_only_before_they_descend() {
        // `up` is a link to the root; `esc` would follow it, then climb out.
        let link = path("a/b/esc");
        assert_eq!(
            link_target(&path("a/b/up"), "../..").as_deref(),
            Some("../..")
        );
        assert_eq!(link_target(&link, "up/../../../etc/passwd"), None);
        assert_eq!(link_target(&link, "up/.."), None);
        assert_eq!(link_target(&link, "./d/../e"), None);
        assert_eq!(link_target(&link, "../d/../e"), None);
        // Down through a link is fine: the link itself stays inside.
        assert_eq!(link_target(&link, "up/x").as_deref(), Some("up/x"));
    }

    #[test]
    fn name_set_refuses_case_duplicates_and_file_folder_clashes() {
        let mut set = NameSet::new(100);
        assert_eq!(set.insert(&path("A.txt"), false), Ok(Added::New));
        assert_eq!(
            set.insert(&path("a.TXT"), false),
            Err(Clash::Duplicate("a.TXT".to_string()))
        );
        // Folders may repeat, named or implied; a named one is new once.
        set.insert(&path("dir/x"), false).unwrap();
        assert_eq!(set.insert(&path("DIR/"), true), Ok(Added::New));
        assert_eq!(set.insert(&path("dir/"), true), Ok(Added::FolderAgain));
        assert_eq!(set.insert(&path("dir/y"), false), Ok(Added::New));
        // A file where a folder is, and a folder where a file is.
        assert_eq!(
            set.insert(&path("Dir"), false),
            Err(Clash::Duplicate("Dir".to_string()))
        );
        assert_eq!(
            set.insert(&path("a.txt/b"), false),
            Err(Clash::Duplicate("a.txt".to_string()))
        );
        assert_eq!(
            set.insert(&path("a.txt/"), true),
            Err(Clash::Duplicate("a.txt".to_string()))
        );
        // Unicode case.
        set.insert(&path("STRAẞE"), false).unwrap();
        assert!(set.insert(&path("strasse"), false).is_err());
        // The same name in two folders is two paths.
        set.insert(&path("p/same"), false).unwrap();
        set.insert(&path("q/same"), false).unwrap();
    }

    #[test]
    fn name_set_counts_implied_folders_against_its_bound() {
        let mut set = NameSet::new(5);
        // "a", "a/b", "a/b/c", "a/b/c/d.txt": four paths from one entry.
        set.insert(&path("a/b/c/d.txt"), false).unwrap();
        set.insert(&path("a/b/e.txt"), false).unwrap();
        assert_eq!(set.insert(&path("a/f/g.txt"), false), Err(Clash::TooMany));
        // Paths already in the set cost nothing.
        assert_eq!(set.insert(&path("a/b/"), true), Ok(Added::New));
    }

    #[test]
    fn name_set_inserts_deep_names_in_linear_time() {
        // 2048 levels, 100 times: quadratic keys would hold about 400 MB.
        let mut set = NameSet::new(u64::MAX);
        for i in 0..100 {
            let deep = parse(&format!("{i}/{}", vec!["a"; 2040].join("/"))).unwrap();
            set.insert(&deep.path, false).unwrap();
        }
        assert_eq!(set.nodes.len(), 100 * 2041);
    }

    #[test]
    fn shown_names_are_safe_for_a_toast() {
        assert_eq!(shown("a\nb"), "a\u{FFFD}b");
        let long = "x".repeat(200);
        let out = shown(&long);
        assert_eq!(out.chars().count(), SHOWN_CHARS + 1);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn decodes_utf8_or_windows_1252() {
        assert_eq!(decode("ü.txt".as_bytes()), "ü.txt");
        assert_eq!(decode(&[0xFC, b'.', b't']), "ü.t");
        // 0x80 to 0x9F are characters, not C1 controls, so the name passes.
        let name = decode(b"\x93caf\xE9\x94 \x80 \x81\x9F.txt");
        assert_eq!(name, "“café” € \u{FFFD}Ÿ.txt");
        assert!(parse(&name).is_some());
        let high: String = (0x80..=0x9Fu8).map(|b| decode(&[b, 0xFF])).collect();
        assert!(!high.chars().any(char::is_control), "{high:?}");
    }
}
