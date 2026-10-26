//! What every engine does around its own work: finding and, for Grape's
//! fallback folder only, creating the destination folder; giving the temp
//! file its final name without replacing anything; and the names that go
//! into user messages. Engines write their output to a [`TempFile`] in the
//! destination folder, then call [`finalize`].

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::Path;

use grape_core::{Category, Format, Job};

use crate::output::{place_no_clobber, TempFile};
use crate::{CancelToken, EngineCtx, ErrorKind, GrapeError, JobOutcome};

/// How many times a taken output name is re-planned before giving up.
const MAX_REPLANS: usize = 3;

/// The folder the planned output `dest` goes into.
pub(crate) fn dest_dir<'a>(dest: &'a Path, names: &Names) -> Result<&'a Path, GrapeError> {
    dest.parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .ok_or_else(|| names.write_failed(format!("output path has no folder: {}", dest.display())))
}

/// Creates `dir` if it is Grape's own fallback folder (Downloads\Grape):
/// for a job that falls back, or one whose output setting names that same
/// folder. Any other folder must still be there; one that vanished since
/// planning is a write error when the temp file can't be created, not
/// something to bring back.
pub(crate) fn ensure_dir(
    job: &Job,
    ctx: &EngineCtx,
    dir: &Path,
    names: &Names,
) -> Result<(), GrapeError> {
    if job.used_fallback || ctx.fallback_dir.as_deref() == Some(dir) {
        fs::create_dir_all(dir)
            .map_err(|e| names.write_failed(format!("create {}: {e}", dir.display())))?;
    }
    Ok(())
}

/// Whatever failed, a cancelled job reports that it was cancelled: the
/// failure may just be the cancelled reader, writer or child process.
pub(crate) fn or_cancelled(cancel: &CancelToken, src: &Path, e: GrapeError) -> GrapeError {
    if cancel.is_cancelled() {
        crate::cancelled(src)
    } else {
        e
    }
}

/// Gives the temp file its final name without replacing anything. If the
/// planned name was taken meanwhile, re-plans it against a fresh listing of
/// the folder plus every name tried so far, up to [`MAX_REPLANS`] times.
pub(crate) fn finalize(
    temp: &TempFile,
    job: &Job,
    src: &Path,
    dest: &Path,
    dir: &Path,
    names: &Names,
) -> Result<JobOutcome, GrapeError> {
    let source_name = file_name(src);
    let mut target = dest.to_path_buf();
    let mut attempted: HashSet<String> = HashSet::from([file_name(dest)]);
    for replans in 0..=MAX_REPLANS {
        match place_no_clobber(temp.path(), &target) {
            Ok(()) => {
                return Ok(JobOutcome {
                    outputs: vec![target],
                    renamed: replans > 0,
                    notes: Vec::new(),
                })
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                if replans == MAX_REPLANS {
                    break;
                }
                let mut taken = dir_names(dir);
                taken.extend(attempted.iter().cloned());
                let name = grape_core::output_name(&source_name, &job.op, &taken);
                attempted.insert(name.clone());
                target = dir.join(name);
            }
            Err(e) => {
                return Err(names.write_failed(format!(
                    "finalize {} as {}: {e}",
                    temp.path().display(),
                    target.display()
                )))
            }
        }
    }
    Err(GrapeError::new(
        ErrorKind::NameTaken,
        format!("A file named {} keeps appearing; try again.", names.planned),
        format!(
            "every name tried in {} was taken: {}",
            dir.display(),
            sorted(&attempted).join(", ")
        ),
    ))
}

/// The names in `dir` right now; empty if it can't be listed.
fn dir_names(dir: &Path) -> HashSet<String> {
    fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

fn sorted(set: &HashSet<String>) -> Vec<String> {
    let mut list: Vec<String> = set.iter().cloned().collect();
    list.sort();
    list
}

/// A path's file name as text, or the whole path if it has none.
pub(crate) fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// The names that go into user messages.
pub(crate) struct Names {
    /// The source's file name.
    pub(crate) source: String,
    /// The source format's display name.
    pub(crate) from: &'static str,
    /// What a file of the source format is called: "image", "video" or
    /// "file".
    noun: &'static str,
    /// The target format's display name.
    pub(crate) to: &'static str,
    /// The destination folder's name.
    pub(crate) folder: String,
    /// The planned output file name.
    pub(crate) planned: String,
}

impl Names {
    pub(crate) fn new(src: &Path, dest: &Path, from: Format, to: Format) -> Names {
        let folder = dest
            .parent()
            .map(|dir| match dir.file_name() {
                Some(name) => name.to_string_lossy().into_owned(),
                None => dir.display().to_string(),
            })
            .unwrap_or_default();
        Names {
            source: file_name(src),
            from: from.display_name(),
            noun: noun(from),
            to: to.display_name(),
            folder,
            planned: file_name(dest),
        }
    }

    pub(crate) fn read_failed(&self, detail: String) -> GrapeError {
        let message = format!("Grape couldn't read {}.", self.source);
        GrapeError::new(ErrorKind::ReadFailed, message, detail)
    }

    pub(crate) fn decode_failed(&self, detail: String) -> GrapeError {
        let message = format!(
            "{} isn't a readable {} {}.",
            self.source, self.from, self.noun
        );
        GrapeError::new(ErrorKind::DecodeFailed, message, detail)
    }

    /// Converting the file would need more memory than Grape allows.
    pub(crate) fn too_large(&self, detail: String) -> GrapeError {
        let message = format!("{} is too large to convert.", self.source);
        GrapeError::new(ErrorKind::DecodeFailed, message, detail)
    }

    pub(crate) fn encode_failed(&self, detail: String) -> GrapeError {
        let message = format!("Grape couldn't write a {} file.", self.to);
        GrapeError::new(ErrorKind::EncodeFailed, message, detail)
    }

    pub(crate) fn write_failed(&self, detail: String) -> GrapeError {
        let message = format!("Grape couldn't save into {}.", self.folder);
        GrapeError::new(ErrorKind::WriteFailed, message, detail)
    }

    /// A tool that would change nothing worth a new file (D26), saying
    /// why: "`source` `why`", such as "clip.mp4 has no sound to remove.".
    pub(crate) fn no_change(&self, why: &str, detail: String) -> GrapeError {
        let message = format!("{} {why}", self.source);
        GrapeError::new(ErrorKind::NoChange, message, detail)
    }
}

/// "image" for images and GIF, "video" for the other video formats, "file"
/// for everything else ("song.mp3 isn't a readable MP3 file.").
fn noun(from: Format) -> &'static str {
    match from.category() {
        _ if from == Format::Gif => "image",
        Category::Image => "image",
        Category::Video => "video",
        _ => "file",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_name_the_kind_of_file() {
        let names = |from| Names::new(Path::new("a"), Path::new("out/b"), from, Format::Png);
        let message = |from| names(from).decode_failed(String::new()).user_message;
        assert_eq!(message(Format::Jpg), "a isn't a readable JPG image.");
        assert_eq!(message(Format::Gif), "a isn't a readable GIF image.");
        assert_eq!(message(Format::Mov), "a isn't a readable MOV video.");
        assert_eq!(message(Format::Mp3), "a isn't a readable MP3 file.");
        assert_eq!(message(Format::Srt), "a isn't a readable SRT file.");
        assert_eq!(
            names(Format::Jpg).write_failed(String::new()).user_message,
            "Grape couldn't save into out."
        );
    }
}
