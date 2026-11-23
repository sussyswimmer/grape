//! Reads and writes `shortcuts.json` in the app data folder
//! (`docs/design/m10-shortcuts-wheel.md` §7). It is a file of its own, not
//! a key in `settings.json`, so a broken one never takes the settings with
//! it.
//!
//! - [`load_from`] is total. No file is the defaults. A file that can't be
//!   read or used at all is renamed to `shortcuts.unreadable.json`
//!   (` 2.json`, … when taken; never over a file) and the defaults are
//!   used. If that rename fails, the [`Store`] refuses every save, so
//!   nothing writes over the file.
//! - When a load left part of the file out, the file stays as it is until
//!   the first save, which first copies it to the next free
//!   `shortcuts.unreadable` name.
//! - [`save_to`] writes `shortcuts.json.tmp` (only the user can read it on
//!   Unix), syncs it and renames it over the file.
//! - Where the wheel isn't available, nothing is read, created or
//!   written.
//!
//! The file, its `.tmp` and every copy hold what the user typed, as plain
//! text. Log lines here carry counts, fixed words and the copy's own name,
//! never the folder or anything from the file.

use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use grape_core::Platform;

use super::model::{self, Config, MAX_LOAD_BYTES};

/// The file's name in the app data folder.
pub const FILE: &str = "shortcuts.json";
/// What a save writes before it renames it over the file.
const TMP_FILE: &str = "shortcuts.json.tmp";
/// A file that couldn't be used, or was only partly used, is kept as
/// `shortcuts.unreadable.json`.
const UNREADABLE_STEM: &str = "shortcuts.unreadable";
/// How many names [`free_names`] tries.
const UNREADABLE_NAMES: u32 = 999;
/// How often the rename is tried while Windows says the file is in use.
const RENAME_TRIES: u32 = 5;
/// The wait between two tries.
const RENAME_WAIT: Duration = Duration::from_millis(20);

/// Where the file is in the app data folder `dir`.
pub fn file_path(dir: &Path) -> PathBuf {
    dir.join(FILE)
}

/// Why a save didn't happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveError {
    /// The wheel isn't available on this OS, so nothing is written.
    Unavailable,
    /// A file that couldn't be read is still in place, and saving would
    /// write over it.
    Blocked,
    /// The config is past the file's size limit.
    TooLarge,
    /// The disk said no.
    Io(io::ErrorKind),
}

impl fmt::Display for SaveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SaveError::Unavailable => f.write_str("the wheel isn't available here"),
            SaveError::Blocked => f.write_str("an unreadable file is in the way"),
            SaveError::TooLarge => f.write_str("the file would be too large"),
            SaveError::Io(kind) => write!(f, "the disk refused ({kind:?})"),
        }
    }
}

impl std::error::Error for SaveError {}

impl From<io::Error> for SaveError {
    fn from(err: io::Error) -> Self {
        SaveError::Io(err.kind())
    }
}

/// What a save has to mind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Keep {
    /// Nothing: write the file.
    Nothing,
    /// The file holds more than was loaded: copy it aside first.
    CopyFirst,
    /// The file couldn't be read or moved: never write.
    Blocked,
    /// Not on this OS: never write.
    Unavailable,
}

/// The file after a load: where it is and what the next save has to
/// mind. `Debug` leaves the folder out.
pub struct Store {
    dir: PathBuf,
    keep: Keep,
}

impl fmt::Debug for Store {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Store")
            .field("keep", &self.keep)
            .finish_non_exhaustive()
    }
}

/// What [`load_from`] answers with.
#[derive(Debug)]
pub struct Loaded {
    /// The config to use.
    pub config: Config,
    /// The sentence for the first view's note, when the load left
    /// something out or started fresh.
    pub note: Option<&'static str>,
    /// What saves go through from now on.
    pub store: Store,
}

/// Loads the shortcuts from the app data folder `dir`. Total: see the
/// module's notes for what happens to a file that can't be used.
pub fn load_from(dir: &Path, platform: Platform) -> Loaded {
    load_with(dir, platform, |from, to| fs::rename(from, to))
}

/// [`load_from`], renaming through `rename`.
fn load_with(
    dir: &Path,
    platform: Platform,
    rename: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> Loaded {
    let loaded = |config, note, keep| Loaded {
        config,
        note,
        store: Store {
            dir: dir.to_owned(),
            keep,
        },
    };
    if !model::available(platform) {
        return loaded(Config::defaults(platform), None, Keep::Unavailable);
    }
    let path = file_path(dir);
    let (config, problem) = match read_capped(&path) {
        Ok(bytes) => {
            let (config, report) = model::load(&bytes, platform);
            if !report.move_aside() {
                if report.dropped() {
                    log::warn!(
                        "shortcuts: {FILE} was read in part ({report}); it's copied aside before the first save"
                    );
                    return loaded(config, report.note(), Keep::CopyFirst);
                }
                return loaded(config, None, Keep::Nothing);
            }
            (config, report.to_string())
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return loaded(Config::defaults(platform), None, Keep::Nothing);
        }
        Err(err) => (
            Config::defaults(platform),
            format!("couldn't be read ({:?})", err.kind()),
        ),
    };
    let keep = match move_aside(&path, UNREADABLE_STEM, rename) {
        Ok(aside) => {
            let name = aside.file_name().unwrap_or_default().to_string_lossy();
            log::warn!(
                "shortcuts: {FILE} can't be used: {problem}; renamed it to {name} and using the defaults"
            );
            Keep::Nothing
        }
        Err(err) => {
            log::error!(
                "shortcuts: {FILE} can't be used: {problem}, and renaming it aside failed ({:?}); leaving it as it is and saving nothing",
                err.kind()
            );
            Keep::Blocked
        }
    };
    loaded(config, Some(model::STARTED_FRESH), keep)
}

/// The file's bytes, reading no more than one past what a load takes: a
/// longer file is refused by its length alone.
fn read_capped(path: &Path) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(MAX_LOAD_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// `{stem}.json` in `folder`, then `{stem} 2.json`, ` 3.json` and so on.
fn free_names<'a>(folder: &'a Path, stem: &'a str) -> impl Iterator<Item = PathBuf> + 'a {
    (1..=UNREADABLE_NAMES).map(move |number| {
        folder.join(if number == 1 {
            format!("{stem}.json")
        } else {
            format!("{stem} {number}.json")
        })
    })
}

fn names_taken() -> io::Error {
    io::Error::new(io::ErrorKind::AlreadyExists, "every name is taken")
}

/// Renames `path` to `{stem}.json` in its folder, or to `{stem} 2.json`,
/// ` 3.json` and so on when that's taken, and returns the new path. Never
/// a name that's taken: `rename` replaces an existing file. (Checking and
/// renaming aren't one step, but only one Grape runs, and nothing else
/// writes in its data folder.)
fn move_aside(
    path: &Path,
    stem: &str,
    rename: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> io::Result<PathBuf> {
    let folder = path.parent().unwrap_or_else(|| Path::new(""));
    for aside in free_names(folder, stem) {
        // `symlink_metadata`, so a link to nowhere takes its name too.
        match fs::symlink_metadata(&aside) {
            Ok(_) => continue,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                rename(path, &aside)?;
                return Ok(aside);
            }
            Err(err) => return Err(err),
        }
    }
    Err(names_taken())
}

/// Copies `path` to the first free `{stem}` name in its folder and
/// returns the copy's path, or `None` when there's no file to copy. The
/// copy is made with "create new", so it never replaces a file.
fn copy_aside(path: &Path, stem: &str) -> io::Result<Option<PathBuf>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let folder = path.parent().unwrap_or_else(|| Path::new(""));
    for aside in free_names(folder, stem) {
        match private_file(&aside) {
            Ok(mut file) => {
                let written = file.write_all(&bytes).and_then(|()| file.sync_all());
                if let Err(err) = written {
                    drop(file);
                    let _ = fs::remove_file(&aside);
                    return Err(err);
                }
                return Ok(Some(aside));
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
    Err(names_taken())
}

/// A new file only its owner can read (on Unix). Fails if `path` exists.
fn private_file(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

impl Store {
    /// Saves `config`. The first save after a load that left something
    /// out copies the file aside first; if that copy can't be made,
    /// nothing is written and the next save tries again.
    ///
    /// # Errors
    /// See [`SaveError`]. The caller's change still applies in memory.
    pub fn save(&mut self, config: &Config) -> Result<(), SaveError> {
        match self.keep {
            Keep::Unavailable => return Err(SaveError::Unavailable),
            Keep::Blocked => return Err(SaveError::Blocked),
            Keep::Nothing => {}
            Keep::CopyFirst => {
                // Checked first, so a refused save leaves no copy behind.
                model::stored_bytes(config).map_err(|_| SaveError::TooLarge)?;
                if let Some(aside) = copy_aside(&file_path(&self.dir), UNREADABLE_STEM)? {
                    let name = aside.file_name().unwrap_or_default().to_string_lossy();
                    log::info!("shortcuts: kept the file as it was read in {name}");
                }
                self.keep = Keep::Nothing;
            }
        }
        save_to(&self.dir, config)
    }
}

/// Writes `config` to `shortcuts.json` in `dir`, all of it or none: the
/// folder is created, the bytes go to `shortcuts.json.tmp`, are synced,
/// and the rename over the file is tried a few times while Windows says
/// the file is in use.
///
/// # Errors
/// [`SaveError::TooLarge`] past 1 MiB, with nothing written, and
/// [`SaveError::Io`] with the disk's reason.
pub fn save_to(dir: &Path, config: &Config) -> Result<(), SaveError> {
    let bytes = model::stored_bytes(config).map_err(|_| SaveError::TooLarge)?;
    write_whole(dir, &bytes, |from, to| fs::rename(from, to), RENAME_WAIT)
}

/// [`save_to`]'s write, renaming through `rename` and waiting `wait`
/// between tries.
fn write_whole(
    dir: &Path,
    bytes: &[u8],
    mut rename: impl FnMut(&Path, &Path) -> io::Result<()>,
    wait: Duration,
) -> Result<(), SaveError> {
    fs::create_dir_all(dir)?;
    let tmp = dir.join(TMP_FILE);
    // A leftover would keep its own permissions.
    match fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }
    let written = private_file(&tmp).and_then(|mut file| {
        file.write_all(bytes)?;
        file.sync_all()
    });
    let mut result = written;
    if result.is_ok() {
        let target = file_path(dir);
        for attempt in 1..=RENAME_TRIES {
            result = rename(&tmp, &target);
            match &result {
                Err(err)
                    if err.kind() == io::ErrorKind::PermissionDenied && attempt < RENAME_TRIES =>
                {
                    thread::sleep(wait);
                }
                _ => break,
            }
        }
    }
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    Ok(result?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shortcuts::model::{
        add_item, add_step, set_enabled, set_step, NewItem, StepEdit, StepKindId, LEFT_OUT,
        MAX_SCRIPT, STARTED_FRESH,
    };

    const MAC: Platform = Platform::MacOs;
    const WIN: Platform = Platform::Windows;
    /// Stands in for what the user typed.
    const MARKER: &str = "zq-marker";

    /// A fresh, empty folder for one test, removed when dropped.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("grape-shortcuts-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let path = self.0.join(name);
            fs::write(&path, bytes).unwrap();
            path
        }

        fn read(&self, name: &str) -> Vec<u8> {
            fs::read(self.0.join(name)).unwrap()
        }

        /// The folder's entries, sorted.
        fn names(&self) -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(&self.0)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn no_rename(_: &Path, _: &Path) -> io::Result<()> {
        panic!("no rename")
    }

    /// A usable file with one shortcut that loads and one that doesn't.
    fn partly_good() -> Vec<u8> {
        format!(
            concat!(
                r#"{{"version":1,"menus":[{{"scope":{{"kind":"everywhere"}},"withEverywhere":false,"items":["#,
                r#"{{"name":"Kept","icon":{{"kind":"none"}},"kind":"steps","steps":[]}},"#,
                r#"{{"name":"{}","icon":{{"kind":"none"}},"kind":"steps","steps":[{{"kind":"teleport"}}]}}"#,
                r#"]}}]}}"#
            ),
            MARKER
        )
        .into_bytes()
    }

    /// The defaults with one more shortcut, so a save differs from them.
    fn changed() -> Config {
        let mut config = Config::defaults(MAC);
        let menu = config.menus[0].id;
        add_item(&mut config, menu, None, NewItem::Steps).unwrap();
        set_enabled(&mut config, true).unwrap();
        config
    }

    fn names_in(config: &Config) -> Vec<&str> {
        config
            .everywhere()
            .iter()
            .map(|item| item.name.as_str())
            .collect()
    }

    #[test]
    fn no_file_is_the_defaults_and_nothing_is_written() {
        let scratch = Scratch::new("missing");
        let loaded = load_with(&scratch.0, MAC, no_rename);
        assert!(loaded.config == Config::defaults(MAC));
        assert_eq!(loaded.note, None);
        assert_eq!(loaded.store.keep, Keep::Nothing);
        assert!(scratch.names().is_empty());
        assert_eq!(file_path(&scratch.0), scratch.0.join("shortcuts.json"));
    }

    #[test]
    fn the_first_save_writes_the_file_and_it_loads_back() {
        let scratch = Scratch::new("first-save");
        // The data folder itself may not exist yet.
        let dir = scratch.0.join("app").join("data");
        let mut loaded = load_from(&dir, MAC);
        assert!(!dir.exists());
        let config = changed();
        loaded.store.save(&config).unwrap();
        assert_eq!(
            fs::read(dir.join(FILE)).unwrap(),
            model::stored_bytes(&config).unwrap()
        );
        let mut names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.sort();
        assert_eq!(names, [FILE]);

        let again = load_with(&dir, MAC, no_rename);
        assert_eq!(again.note, None);
        assert_eq!(again.store.keep, Keep::Nothing);
        assert!(again.config.enabled);
        assert_eq!(names_in(&again.config), names_in(&config));
        assert_eq!(
            model::stored_bytes(&again.config).unwrap(),
            model::stored_bytes(&config).unwrap()
        );
    }

    #[test]
    fn where_the_wheel_is_not_available_nothing_is_read_or_written() {
        let scratch = Scratch::new("unavailable");
        // Not even a file that would be moved aside on a Mac.
        scratch.write(FILE, b"not json");
        let mut loaded = load_with(&scratch.0, WIN, no_rename);
        assert!(loaded.config == Config::defaults(WIN));
        assert_eq!(loaded.note, None);
        assert_eq!(
            loaded.store.save(&Config::defaults(WIN)),
            Err(SaveError::Unavailable)
        );
        assert_eq!(scratch.names(), [FILE]);
        assert_eq!(scratch.read(FILE), b"not json");

        let gone = scratch.0.join("never-made");
        let mut loaded = load_from(&gone, WIN);
        assert_eq!(
            loaded.store.save(&loaded.config),
            Err(SaveError::Unavailable)
        );
        assert!(!gone.exists());
    }

    #[test]
    fn a_file_that_cannot_be_used_is_renamed_aside() {
        let newer = br#"{"version":2,"menus":[]}"#;
        let mut too_large = model::stored_bytes(&Config::defaults(MAC)).unwrap();
        too_large.resize(MAX_LOAD_BYTES + 1, b' ');
        let files: [&[u8]; 7] = [
            b"",
            b"not json",
            br#"{"version":1,"menus":["#,
            br#""a secret""#,
            br#"{"version":1}"#,
            newer,
            &too_large,
        ];
        for (n, bytes) in files.into_iter().enumerate() {
            let scratch = Scratch::new(&format!("unusable-{n}"));
            scratch.write(FILE, bytes);
            let mut loaded = load_from(&scratch.0, MAC);
            assert!(loaded.config == Config::defaults(MAC), "file {n}");
            assert_eq!(loaded.note, Some(STARTED_FRESH));
            assert_eq!(scratch.names(), ["shortcuts.unreadable.json"]);
            assert!(
                scratch.read("shortcuts.unreadable.json") == bytes,
                "file {n}"
            );
            // The next save writes a fresh file beside it.
            loaded.store.save(&changed()).unwrap();
            assert_eq!(scratch.names(), [FILE, "shortcuts.unreadable.json"]);
            assert!(
                scratch.read("shortcuts.unreadable.json") == bytes,
                "file {n}"
            );
        }
    }

    #[test]
    fn renaming_aside_never_takes_a_name_in_use() {
        let scratch = Scratch::new("numbered");
        scratch.write("shortcuts.unreadable.json", b"first");
        scratch.write("shortcuts.unreadable 2.json", b"second");
        // The settings file's copies are someone else's names.
        scratch.write("settings.unreadable.json", b"settings");
        scratch.write(FILE, b"third");
        let loaded = load_from(&scratch.0, MAC);
        assert_eq!(loaded.note, Some(STARTED_FRESH));
        assert_eq!(scratch.read("shortcuts.unreadable 3.json"), b"third");
        assert_eq!(scratch.read("shortcuts.unreadable.json"), b"first");
        assert_eq!(scratch.read("shortcuts.unreadable 2.json"), b"second");
        assert_eq!(
            scratch.names(),
            [
                "settings.unreadable.json",
                "shortcuts.unreadable 2.json",
                "shortcuts.unreadable 3.json",
                "shortcuts.unreadable.json",
            ]
        );
    }

    #[test]
    fn the_stem_names_the_copy() {
        let scratch = Scratch::new("stem");
        let path = scratch.write("any.json", b"x");
        scratch.write("kept.json", b"taken");
        let aside = move_aside(&path, "kept", |from, to| fs::rename(from, to)).unwrap();
        assert_eq!(aside, scratch.0.join("kept 2.json"));
        assert_eq!(scratch.names(), ["kept 2.json", "kept.json"]);
        assert_eq!(scratch.read("kept.json"), b"taken");
        // A failed rename is the answer, and nothing moved.
        let path = scratch.write("any.json", b"y");
        let failed = move_aside(&path, "kept", |_, _| {
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "test"))
        });
        assert_eq!(failed.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(scratch.names(), ["any.json", "kept 2.json", "kept.json"]);
    }

    #[test]
    fn a_file_that_cannot_be_read_is_renamed_aside() {
        // A folder in its place can't be read as a file on any OS.
        let scratch = Scratch::new("unreadable");
        fs::create_dir(scratch.0.join(FILE)).unwrap();
        let loaded = load_from(&scratch.0, MAC);
        assert!(loaded.config == Config::defaults(MAC));
        assert_eq!(loaded.note, Some(STARTED_FRESH));
        assert_eq!(loaded.store.keep, Keep::Nothing);
        assert_eq!(scratch.names(), ["shortcuts.unreadable.json"]);
        assert!(scratch.0.join("shortcuts.unreadable.json").is_dir());
    }

    #[test]
    fn a_failed_rename_leaves_the_file_and_blocks_every_save() {
        let scratch = Scratch::new("stuck");
        scratch.write(FILE, b"not json");
        let mut loaded = load_with(&scratch.0, MAC, |_, _| {
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "test"))
        });
        assert!(loaded.config == Config::defaults(MAC));
        assert_eq!(loaded.note, Some(STARTED_FRESH));
        for _ in 0..2 {
            assert_eq!(loaded.store.save(&changed()), Err(SaveError::Blocked));
        }
        assert_eq!(scratch.names(), [FILE]);
        assert_eq!(scratch.read(FILE), b"not json");
    }

    #[test]
    fn a_file_read_in_part_is_copied_aside_before_the_first_save() {
        let scratch = Scratch::new("partly");
        let original = partly_good();
        scratch.write(FILE, &original);
        scratch.write("shortcuts.unreadable.json", b"older");
        let mut loaded = load_with(&scratch.0, MAC, no_rename);
        assert_eq!(names_in(&loaded.config), ["Kept"]);
        assert_eq!(loaded.note, Some(LEFT_OUT));
        // Loading alone touches nothing.
        assert_eq!(scratch.names(), [FILE, "shortcuts.unreadable.json"]);
        assert_eq!(scratch.read(FILE), original);

        loaded.store.save(&loaded.config).unwrap();
        assert_eq!(
            scratch.names(),
            [
                FILE,
                "shortcuts.unreadable 2.json",
                "shortcuts.unreadable.json"
            ]
        );
        assert_eq!(scratch.read("shortcuts.unreadable 2.json"), original);
        assert_eq!(scratch.read("shortcuts.unreadable.json"), b"older");
        let saved = scratch.read(FILE);
        assert_eq!(saved, model::stored_bytes(&loaded.config).unwrap());
        assert!(!String::from_utf8(saved).unwrap().contains(MARKER));

        // Only the first save copies.
        loaded.store.save(&changed()).unwrap();
        assert_eq!(scratch.names().len(), 3);
        // And the saved file loads whole.
        let again = load_with(&scratch.0, MAC, no_rename);
        assert_eq!(again.note, None);
        assert_eq!(again.store.keep, Keep::Nothing);
    }

    #[test]
    fn a_top_value_that_fell_back_is_kept_too_but_needs_no_note() {
        let scratch = Scratch::new("settings-only");
        let original = br#"{"version":1,"opensAt":"corner","menus":[]}"#;
        scratch.write(FILE, original);
        let mut loaded = load_with(&scratch.0, MAC, no_rename);
        assert_eq!(loaded.note, None);
        assert_eq!(loaded.store.keep, Keep::CopyFirst);
        loaded.store.save(&loaded.config).unwrap();
        assert_eq!(scratch.read("shortcuts.unreadable.json"), original);
    }

    #[test]
    fn a_copy_that_cannot_be_made_stops_the_save_and_is_tried_again() {
        let scratch = Scratch::new("copy-fails");
        let original = partly_good();
        scratch.write(FILE, &original);
        let mut loaded = load_with(&scratch.0, MAC, no_rename);
        // A save that's refused for its size leaves no copy behind.
        assert_eq!(loaded.store.save(&huge()), Err(SaveError::TooLarge));
        assert_eq!(scratch.names(), [FILE]);
        assert_eq!(loaded.store.keep, Keep::CopyFirst);
        // The file turned into something that can't be read as one.
        fs::remove_file(scratch.0.join(FILE)).unwrap();
        fs::create_dir(scratch.0.join(FILE)).unwrap();
        assert!(matches!(
            loaded.store.save(&loaded.config),
            Err(SaveError::Io(_))
        ));
        assert_eq!(loaded.store.keep, Keep::CopyFirst);
        assert_eq!(scratch.names(), [FILE]);
        // Back again: the copy is made, then the save.
        fs::remove_dir(scratch.0.join(FILE)).unwrap();
        scratch.write(FILE, &original);
        loaded.store.save(&loaded.config).unwrap();
        assert_eq!(scratch.read("shortcuts.unreadable.json"), original);
        // And a file that's gone by then has nothing to keep.
        let scratch = Scratch::new("copy-gone");
        scratch.write(FILE, &original);
        let mut loaded = load_with(&scratch.0, MAC, no_rename);
        fs::remove_file(scratch.0.join(FILE)).unwrap();
        loaded.store.save(&loaded.config).unwrap();
        assert_eq!(scratch.names(), [FILE]);
    }

    /// More scripts than a file may hold.
    fn huge() -> Config {
        let mut config = Config::defaults(MAC);
        let menu = config.menus[0].id;
        let source = "a".repeat(MAX_SCRIPT);
        for _ in 0..4 {
            let shortcut = add_item(&mut config, menu, None, NewItem::Steps).unwrap();
            for _ in 0..16 {
                let step = add_step(&mut config, shortcut, StepKindId::Shell).unwrap();
                let edit = StepEdit::Shell {
                    source: source.clone(),
                };
                set_step(&mut config, step, &edit).unwrap();
            }
        }
        config
    }

    #[test]
    fn a_config_past_the_limit_is_not_saved() {
        let scratch = Scratch::new("too-large");
        let dir = scratch.0.join("data");
        assert_eq!(save_to(&dir, &huge()), Err(SaveError::TooLarge));
        assert!(!dir.exists(), "nothing is made for a refused save");
        // And what was there stays.
        save_to(&dir, &changed()).unwrap();
        let before = fs::read(dir.join(FILE)).unwrap();
        assert_eq!(save_to(&dir, &huge()), Err(SaveError::TooLarge));
        assert_eq!(fs::read(dir.join(FILE)).unwrap(), before);
    }

    #[test]
    fn a_save_replaces_the_file_whole_and_leaves_no_tmp() {
        let scratch = Scratch::new("replace");
        scratch.write(FILE, &[b'x'; 100_000]);
        // A leftover from a save that was cut short.
        scratch.write(TMP_FILE, b"half a file");
        let config = changed();
        save_to(&scratch.0, &config).unwrap();
        assert_eq!(scratch.names(), [FILE]);
        assert_eq!(scratch.read(FILE), model::stored_bytes(&config).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn only_the_user_can_read_what_is_written() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: PathBuf| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        let scratch = Scratch::new("mode");
        // A leftover with wider permissions doesn't pass them on.
        let tmp = scratch.write(TMP_FILE, b"old");
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o644)).unwrap();
        save_to(&scratch.0, &changed()).unwrap();
        assert_eq!(mode(scratch.0.join(FILE)), 0o600);
        // The copy of a file read in part, too.
        let scratch = Scratch::new("mode-copy");
        let file = scratch.write(FILE, &partly_good());
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        let mut loaded = load_with(&scratch.0, MAC, no_rename);
        loaded.store.save(&loaded.config).unwrap();
        assert_eq!(mode(scratch.0.join("shortcuts.unreadable.json")), 0o600);
        assert_eq!(mode(scratch.0.join(FILE)), 0o600);
    }

    #[test]
    fn the_rename_is_tried_again_while_the_file_is_in_use() {
        let denied = || Err(io::Error::new(io::ErrorKind::PermissionDenied, "test"));
        let scratch = Scratch::new("retry");
        scratch.write(FILE, b"before");
        let mut calls = 0;
        let result = write_whole(
            &scratch.0,
            b"after",
            |from, to| {
                calls += 1;
                if calls < RENAME_TRIES {
                    return denied();
                }
                fs::rename(from, to)
            },
            Duration::ZERO,
        );
        assert_eq!(result, Ok(()));
        assert_eq!(calls, RENAME_TRIES);
        assert_eq!(scratch.read(FILE), b"after");
        assert_eq!(scratch.names(), [FILE]);

        // Still in use after the last try: the old file stays, whole.
        let mut calls = 0;
        let result = write_whole(
            &scratch.0,
            b"never",
            |_, _| {
                calls += 1;
                denied()
            },
            Duration::ZERO,
        );
        assert_eq!(result, Err(SaveError::Io(io::ErrorKind::PermissionDenied)));
        assert_eq!(calls, RENAME_TRIES);
        assert_eq!(scratch.read(FILE), b"after");
        assert_eq!(scratch.names(), [FILE]);

        // Any other failure isn't tried again.
        let mut calls = 0;
        let result = write_whole(
            &scratch.0,
            b"never",
            |_, _| {
                calls += 1;
                Err(io::Error::new(io::ErrorKind::StorageFull, "test"))
            },
            Duration::ZERO,
        );
        assert_eq!(result, Err(SaveError::Io(io::ErrorKind::StorageFull)));
        assert_eq!(calls, 1);
        assert_eq!(scratch.names(), [FILE]);
    }

    #[test]
    fn a_folder_that_cannot_be_made_fails_the_save() {
        let scratch = Scratch::new("no-folder");
        // A file where the data folder should be.
        let dir = scratch.write("data", b"a file");
        assert!(matches!(save_to(&dir, &changed()), Err(SaveError::Io(_))));
        assert_eq!(scratch.read("data"), b"a file");
        assert_eq!(scratch.names(), ["data"]);
    }

    #[test]
    fn what_is_printed_names_no_folder_and_nothing_typed() {
        let scratch = Scratch::new(MARKER);
        scratch.write(FILE, &partly_good());
        let loaded = load_with(&scratch.0, MAC, no_rename);
        let printed = format!("{loaded:?} {loaded:#?} {:?}", loaded.store);
        assert!(!printed.contains(MARKER), "{printed}");
        assert!(
            printed.contains("Store { keep: CopyFirst, .. }"),
            "{printed}"
        );
        for err in [
            SaveError::Unavailable,
            SaveError::Blocked,
            SaveError::TooLarge,
            SaveError::Io(io::ErrorKind::PermissionDenied),
        ] {
            let said = err.to_string();
            assert!(
                !said.is_empty() && said.is_ascii() && !said.contains('/'),
                "{said}"
            );
        }
        // An OS error's own text may name the file; only its kind is kept.
        let err = fs::read(scratch.0.join("missing")).unwrap_err();
        assert_eq!(SaveError::from(err), SaveError::Io(io::ErrorKind::NotFound));
    }
}
