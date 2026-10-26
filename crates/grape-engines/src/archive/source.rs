//! Reading archives as a list of entries, each checked by [`Walk`] before
//! a [`Sink`] sees it.
//!
//! - ZIP is read in two passes over the central directory: the first
//!   checks every entry (encryption, compression method, names, links,
//!   declared sizes) before anything is written; the second writes. Before
//!   the zip crate parses the directory, Grape reads the end records'
//!   entry counts itself, and after, it reads the directory again for
//!   names the zip crate would merge.
//! - tar is read as a stream, so each entry is checked as it comes. Every
//!   entry's data is read to its end before the next header, so the bytes
//!   between two entries' data are headers only (see `MAX_HEADER_BYTES`).
//!   A [`Tap`] sees every entry and its data on the way (RAR: bsdtar's tar
//!   is held to the RAR's own headers, `super::rar::tap`).

use std::collections::HashSet;
use std::fs::File;
use std::io::{self, BufReader, Cursor, Read, Seek, SeekFrom};
use std::path::Path;

use zip::read::ZipFile;
use zip::{CompressionMethod, ExtraField, ZipArchive, SUPPORTED_COMPRESSION_METHODS};

use super::gz::{GzInfo, GzMembers};
use super::names::{self, Added, Clash, NameSet};
use super::rar::RarFailure;
use super::sink::Sink;
use super::sniff::{self, Container, EMPTY_TAR_MAX, HEAD};
use super::stream::{read_prefix, Counting, Exact, Metered, Pump, Run, Side};
use super::{time, Failure, LeftOut, Limits, Over};
use crate::cancel::Cancellable;

/// The read buffer on the source file.
const SOURCE_BUF: usize = 64 * 1024;

/// The longest symlink target Grape carries (Linux's `PATH_MAX`); a longer
/// one is left out.
const MAX_LINK_TARGET: u64 = 4096;

/// How many ZIP directory entries are checked between cancel checks.
const CANCEL_EVERY: usize = 64;

/// A ZIP end record without its comment.
const EOCD_LEN: usize = 22;

/// How far from a ZIP's end its end record is looked for: the record and
/// the longest comment, which is where the format puts it.
const ZIP_TAIL: u64 = EOCD_LEN as u64 + 0xFFFF;

/// A ZIP64 end record locator, which sits right before the end record.
const LOCATOR_LEN: u64 = 20;

/// A ZIP64 end record without its extensible data.
const EOCD64_LEN: usize = 56;

/// How far before a ZIP64 locator its end record is looked for.
const EOCD64_WINDOW: u64 = 64 * 1024;

/// A central directory record without its name, extra field and comment.
const CENTRAL_LEN: usize = 46;

/// An entry as the archive describes it, before any check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Raw {
    /// The name, decoded but unchecked.
    pub(crate) name: String,
    /// What the entry is.
    pub(crate) kind: RawKind,
    /// Modification time in Unix seconds, if the archive has one.
    pub(crate) mtime: Option<i64>,
    /// Unix mode bits, if the archive has them.
    pub(crate) mode: Option<u32>,
}

/// What an entry is, as the archive says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RawKind {
    /// A regular file of this many bytes (unknown for a plain `.gz` until
    /// it is read).
    File { size: Option<u64> },
    /// A folder.
    Dir,
    /// A symbolic link to this target (`None` if it can't be read).
    Symlink(Option<String>),
    /// A tar hard link.
    HardLink,
    /// A RAR5 file-copy reference, which bsdtar writes as an empty file
    /// (`super::rar::tap`); left out like a hard link.
    FileCopy,
    /// A device, FIFO or socket, or a tar type Grape doesn't know.
    Special,
    /// Not an item at all: a PAX global header or a GNU volume label.
    Ignore,
}

/// An entry that passed every check and will be written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    /// Its safe path components.
    pub(crate) path: Vec<String>,
    /// What it is.
    pub(crate) kind: Kind,
    /// Modification time in Unix seconds.
    pub(crate) mtime: Option<i64>,
    /// Permission bits, masked to `0o755`.
    pub(crate) mode: u32,
}

impl Entry {
    /// The path with `/` separators, as archives store it.
    pub(crate) fn name(&self) -> String {
        self.path.join("/")
    }
}

/// What a written entry is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Kind {
    /// A regular file.
    File { size: Option<u64> },
    /// A folder.
    Dir,
    /// A symbolic link to this checked target (only when converting).
    Symlink(String),
}

/// What happens to the entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Extract Archive: regular files and folders only.
    Extract,
    /// A conversion that re-packs the entries: links are carried too.
    Repack,
    /// A conversion that keeps the bytes (tar ⇄ tar.gz): every entry is
    /// carried as it is, so the names are checked (PAX records included)
    /// and nothing is left out.
    Check,
}

/// Checks each entry and decides whether it is written.
#[derive(Debug)]
pub(crate) struct Walk {
    mode: Mode,
    names: NameSet,
    /// What was left out so far.
    pub(crate) left_out: LeftOut,
}

impl Walk {
    /// A walk under `limits`: the names it remembers, the folders that
    /// entries only imply included, are held to twice the entries.
    pub(crate) fn new(mode: Mode, limits: &Limits) -> Walk {
        Walk {
            mode,
            names: NameSet::new(limits.max_entries.saturating_mul(2)),
            left_out: LeftOut::default(),
        }
    }

    /// Checks `raw`. `Ok(Some)` to write it, `Ok(None)` to skip it (left out
    /// and counted, a folder already written, or nothing to write), `Err` to
    /// refuse the archive: too many entries, an unsafe name, or two items
    /// with the same name.
    pub(crate) fn admit(&mut self, run: &Run<'_>, raw: Raw) -> Result<Option<Entry>, Failure> {
        self.admit_as(run, raw, true)
    }

    /// [`Walk::admit`] for a plain `.gz`'s one file: the macOS-metadata
    /// rule is for the archives macOS makes, so `._notes.txt.gz` (or a
    /// gzip header naming `._notes.txt`) still gives its file.
    pub(crate) fn admit_single(
        &mut self,
        run: &Run<'_>,
        raw: Raw,
    ) -> Result<Option<Entry>, Failure> {
        self.admit_as(run, raw, false)
    }

    fn admit_as(
        &mut self,
        run: &Run<'_>,
        raw: Raw,
        skip_mac_metadata: bool,
    ) -> Result<Option<Entry>, Failure> {
        run.count_entry()?;
        if raw.kind == RawKind::Ignore {
            return Ok(None);
        }
        let parsed =
            names::parse(&raw.name).ok_or_else(|| Failure::UnsafeName(names::shown(&raw.name)))?;
        let is_dir = match raw.kind {
            RawKind::Dir => true,
            // Old tars mark folders with a trailing slash only.
            RawKind::File { .. } => parsed.trailing_slash,
            _ => false,
        };
        if parsed.path.is_empty() {
            // `./`: the archive's own root.
            return if is_dir {
                Ok(None)
            } else {
                Err(Failure::UnsafeName(names::shown(&raw.name)))
            };
        }
        if self.mode == Mode::Check {
            self.remember(&parsed.path, is_dir)?;
            return Ok(None);
        }
        if skip_mac_metadata && names::is_mac_metadata(&parsed.path) {
            if !is_dir {
                bump(&mut self.left_out.mac_metadata);
            }
            return Ok(None);
        }
        let kind = match raw.kind {
            _ if is_dir => Kind::Dir,
            RawKind::File { size } => Kind::File { size },
            RawKind::Symlink(target) => {
                let target = match self.mode {
                    Mode::Extract => None,
                    _ => target.and_then(|t| names::link_target(&parsed.path, &t)),
                };
                match target {
                    Some(target) => Kind::Symlink(target),
                    None => {
                        bump(&mut self.left_out.links);
                        return Ok(None);
                    }
                }
            }
            RawKind::HardLink => {
                bump(&mut self.left_out.links);
                return Ok(None);
            }
            RawKind::FileCopy => {
                bump(&mut self.left_out.copies);
                return Ok(None);
            }
            RawKind::Special => {
                bump(&mut self.left_out.specials);
                return Ok(None);
            }
            RawKind::Dir | RawKind::Ignore => Kind::Dir,
        };
        let is_dir = kind == Kind::Dir;
        if self.remember(&parsed.path, is_dir)? == Added::FolderAgain {
            // Written once already: a second ZIP folder entry of the same
            // name would fail, and the folder is the same.
            return Ok(None);
        }
        let default = if is_dir { 0o755 } else { 0o644 };
        Ok(Some(Entry {
            path: parsed.path,
            kind,
            mtime: raw.mtime,
            // No setuid, setgid or sticky bit, and no group or other write.
            mode: raw.mode.map_or(default, |mode| mode & 0o755),
        }))
    }

    /// Adds `path` to the names seen, refusing a clash or too many names.
    fn remember(&mut self, path: &[String], is_dir: bool) -> Result<Added, Failure> {
        self.names
            .insert(path, is_dir)
            .map_err(|clash| match clash {
                Clash::Duplicate(name) => Failure::Duplicate(names::shown(&name)),
                Clash::TooMany => Failure::TooBig(Over::Entries),
            })
    }
}

fn bump(count: &mut u32) {
    *count = count.saturating_add(1);
}

/// The source file, buffered, counted and cancellable.
pub(crate) type Source<'r, 'a> = BufReader<Cancellable<Counting<'r, 'a, File>>>;

pub(crate) fn source<'r, 'a>(run: &'r Run<'a>, file: File) -> Source<'r, 'a> {
    BufReader::with_capacity(
        SOURCE_BUF,
        Cancellable::new(Counting::new(run, file), run.token()),
    )
}

/// A buffered reader whose seeks keep the buffer when they land inside it.
/// The zip crate seeks to each entry's local header and then to its data,
/// and asks for its position at every directory record; `BufReader` drops
/// its buffer on each of those, so every entry would read (and count, for
/// progress) its buffer again: progress reached 99 % after a few files.
pub(crate) struct KeepBuf<R>(BufReader<R>);

impl<R: Read + Seek> Read for KeepBuf<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl<R: Read + Seek> Seek for KeepBuf<R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let by = match pos {
            SeekFrom::Current(by) => Some(by),
            SeekFrom::Start(to) => {
                let here = self.0.stream_position()?;
                i64::try_from(to)
                    .ok()
                    .zip(i64::try_from(here).ok())
                    .and_then(|(to, here)| to.checked_sub(here))
            }
            SeekFrom::End(_) => None,
        };
        match by {
            Some(by) => {
                self.0.seek_relative(by)?;
                self.0.stream_position()
            }
            None => self.0.seek(pos),
        }
    }

    fn stream_position(&mut self) -> io::Result<u64> {
        self.0.stream_position()
    }
}

/// A decompressed stream.
pub(crate) type Stream<'r> = Box<dyn Read + 'r>;

/// A source, opened by its real container.
pub(crate) enum Opened<'r> {
    /// A ZIP, still to be opened by the zip crate.
    Zip(File),
    /// A tar stream: a `.tar`, or the content of a `.tar.gz`.
    Tar(Stream<'r>),
    /// A `.gz` of one plain file, with its header.
    Plain(Stream<'r>, Option<GzInfo>),
}

/// Opens `file`, a `container`, for reading; a gzip file's first block is
/// sniffed to tell a tarball from a plain file.
pub(crate) fn open<'r>(
    run: &'r Run<'_>,
    file: File,
    container: Container,
) -> Result<Opened<'r>, Failure> {
    match container {
        Container::Zip => Ok(Opened::Zip(file)),
        Container::Tar => Ok(Opened::Tar(Box::new(Metered::new(run, source(run, file))))),
        // Read through bsdtar before this is reached (`super::rar::walk`).
        Container::Rar => Err(Failure::Rar(RarFailure::NoHelper)),
        Container::Gzip => {
            let (members, info) = GzMembers::new(source(run, file));
            let mut stream = Metered::new(run, members);
            let failed = |e: io::Error| run.failure(&e, Side::Read, "gzip data");
            let mut prefix = read_prefix(&mut stream, HEAD).map_err(failed)?;
            if prefix.len() == HEAD && sniff::is_zeros(&prefix) {
                // Maybe an empty tar: read just past the longest one.
                let more = read_prefix(&mut stream, EMPTY_TAR_MAX + 1 - HEAD).map_err(failed)?;
                prefix.extend(more);
            }
            let is_tar = sniff::is_tar_header(&prefix) || sniff::is_empty_tar(&prefix);
            let stream: Stream<'r> = Box::new(Cursor::new(prefix).chain(stream));
            Ok(if is_tar {
                Opened::Tar(stream)
            } else {
                Opened::Plain(stream, info)
            })
        }
    }
}

/// Reads `reader` to its end, charging it as data; how a skipped entry's
/// data and the bytes after a tar's end are checked.
pub(crate) fn drain<R: Read>(run: &Run<'_>, reader: &mut R, what: &str) -> Result<u64, Failure> {
    io::copy(&mut Pump::new(run, reader), &mut io::sink())
        .map_err(|e| run.failure(&e, Side::Read, what))
}

/// The name of the compression `method` for a user message; `None` for
/// one Grape doesn't know by name.
fn method_name(method: CompressionMethod) -> Option<&'static str> {
    let named = [
        (CompressionMethod::BZIP2, "bzip2"),
        (CompressionMethod::LZMA, "LZMA"),
        (CompressionMethod::ZSTD, "Zstandard"),
        (CompressionMethod::ZSTD_DEPRECATED, "Zstandard"),
        (CompressionMethod::XZ, "XZ"),
        (CompressionMethod::PPMD, "PPMd"),
        (CompressionMethod::AES, "AES"),
        (CompressionMethod::SHRINK, "Shrink"),
        (CompressionMethod::REDUCE_1, "Reduce"),
        (CompressionMethod::REDUCE_2, "Reduce"),
        (CompressionMethod::REDUCE_3, "Reduce"),
        (CompressionMethod::REDUCE_4, "Reduce"),
        (CompressionMethod::IMPLODE, "Implode"),
        (CompressionMethod::PKWARE_IMPLODE, "Implode"),
    ];
    named
        .iter()
        .find(|(m, _)| *m == method)
        .map(|(_, name)| *name)
}

/// What a ZIP entry is, from its Unix mode bits (the type bits exactly:
/// zip's own `is_symlink` also matches other types) and its name's
/// trailing slash.
fn zip_kind(mode: Option<u32>, is_dir: bool) -> RawKind {
    match mode.map(|m| m & 0o170_000) {
        _ if is_dir => RawKind::Dir,
        Some(0o040_000) => RawKind::Dir,
        Some(0o120_000) => RawKind::Symlink(None),
        Some(0o010_000 | 0o020_000 | 0o060_000 | 0o140_000) => RawKind::Special,
        _ => RawKind::File { size: None },
    }
}

/// A ZIP entry's modification time: the extended timestamp (UTC) if it has
/// one, then the NTFS one, then the DOS date and time (read as UTC).
pub(crate) fn zip_mtime<R: Read>(file: &ZipFile<'_, R>) -> Option<i64> {
    let fields = || file.extra_data_fields();
    fields()
        .find_map(|field| match field {
            ExtraField::ExtendedTimestamp(ts) => ts.mod_time().map(i64::from),
            _ => None,
        })
        .or_else(|| {
            fields().find_map(|field| match field {
                ExtraField::Ntfs(ntfs) => Some(time::from_filetime(ntfs.mtime())),
                _ => None,
            })
        })
        .or_else(|| file.last_modified().map(time::from_dos))
}

/// Walks a ZIP: checks every entry first, then writes the admitted ones to
/// `sink` in the central directory's order. `src` is the source's path,
/// for reading the directory a second time.
pub(crate) fn walk_zip(
    run: &Run<'_>,
    file: File,
    src: &Path,
    walk: &mut Walk,
    sink: &mut dyn Sink,
) -> Result<(), Failure> {
    let read_zip = |e| run.zip_failure(e, Side::Read, "zip entry");
    let read = |e: io::Error| Failure::Read(format!("zip end records: {:?}", e.kind()));
    // Before the zip crate reserves memory for, and parses, as many
    // directory records as the end record claims.
    match zip_claimed_entries(&file).map_err(read)? {
        None => {
            return Err(Failure::Corrupt(
                "zip directory: no end record near the end".into(),
            ))
        }
        Some(claimed) if claimed > run.limits.max_entries => {
            return Err(Failure::TooBig(Over::Entries))
        }
        Some(_) => {}
    }
    let mut zip = ZipArchive::new(KeepBuf(source(run, file)))
        .map_err(|e| run.zip_failure(e, Side::Read, "zip directory"))?;
    // Progress counts from the first entry; the directory came from the end.
    run.reset_consumed();
    if zip.len() as u64 > run.limits.max_entries {
        return Err(Failure::TooBig(Over::Entries));
    }
    if let Some(raw) = repeated_name(run, src, zip.central_directory_start())? {
        let name = (0..zip.len())
            .filter_map(|index| zip.by_index_data(index).ok())
            .find(|entry| entry.name_raw() == raw.as_slice())
            .and_then(|entry| entry.name().ok().map(|name| name.into_owned()))
            .unwrap_or_else(|| String::from_utf8_lossy(&raw).into_owned());
        return Err(Failure::Duplicate(names::shown(&name)));
    }
    let mut planned = Vec::new();
    let mut declared: u64 = 0;
    for index in 0..zip.len() {
        if index % CANCEL_EVERY == 0 {
            run.check()?;
        }
        let mut raw = {
            let entry = zip.by_index_data(index).map_err(read_zip)?;
            if entry.encrypted() {
                return Err(Failure::Encrypted);
            }
            let method = entry.compression();
            if !SUPPORTED_COMPRESSION_METHODS.contains(&method) {
                return Err(Failure::Unsupported(method_name(method)));
            }
            let name = entry.name().map_err(read_zip)?.into_owned();
            let mut kind = zip_kind(entry.unix_mode(), entry.is_dir());
            if let RawKind::File { size } = &mut kind {
                *size = Some(entry.size());
            }
            Raw {
                name,
                kind,
                mtime: None,
                mode: entry.unix_mode(),
            }
        };
        if raw.kind == RawKind::Symlink(None) && walk.mode != Mode::Extract {
            raw.kind = RawKind::Symlink(zip_link_target(run, &mut zip, index)?);
        }
        if let Some(entry) = walk.admit(run, raw)? {
            if let Kind::File { size: Some(size) } = entry.kind {
                declared = declared.saturating_add(size);
            }
            planned.push((index, entry));
        }
    }
    // The zip crate fails an entry that inflates past its declared size,
    // so the sum is a hard bound.
    run.precheck(declared)?;
    // Progress counts the writing pass; links were read in the first one.
    run.reset_consumed();
    for (index, mut entry) in planned {
        run.check()?;
        let mut file = zip.by_index(index).map_err(read_zip)?;
        entry.mtime = zip_mtime(&file);
        match &entry.kind {
            Kind::Dir => sink.dir(run, &entry)?,
            Kind::Symlink(target) => sink.symlink(run, &entry, target)?,
            Kind::File { size } => {
                let size = size.unwrap_or(0);
                let mut data = Pump::new(run, Exact::new(&mut file, size));
                sink.file(run, &entry, Some(size), &mut data)?;
            }
        }
    }
    Ok(())
}

/// `len` bytes of `file` from `at` (fewer at its end).
fn read_at(mut file: &File, at: u64, len: u64) -> io::Result<Vec<u8>> {
    file.seek(SeekFrom::Start(at))?;
    let mut bytes = Vec::new();
    file.take(len).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Where `needle` starts in `haystack`, every time.
fn find_all<'h>(haystack: &'h [u8], needle: &'h [u8]) -> impl Iterator<Item = usize> + 'h {
    haystack
        .windows(needle.len())
        .enumerate()
        .filter(move |(_, window)| *window == needle)
        .map(|(at, _)| at)
}

/// The little-endian number of `N` bytes at `at` in `bytes` (0 past the end).
fn le<const N: usize>(bytes: &[u8], at: usize) -> u64 {
    let mut out = [0u8; 8];
    if let Some(field) = bytes.get(at..at + N) {
        out[..N].copy_from_slice(field);
    }
    u64::from_le_bytes(out)
}

/// The most entries any end record near the end of a ZIP claims, ZIP64
/// ones included: the zip crate tries each from the back, and reserves
/// memory for, and parses, as many directory records as the one it tries
/// claims (a ZIP64 record needs only 46 bytes of file per record). `None`
/// if there is no end record in the last [`ZIP_TAIL`] bytes, where the
/// format puts it; Grape then refuses the file rather than let the zip
/// crate search the whole file for one. The zip crate tries a record
/// further back only once every one near the end has failed.
fn zip_claimed_entries(file: &File) -> io::Result<Option<u64>> {
    let len = file.metadata()?.len();
    let tail_start = len.saturating_sub(ZIP_TAIL);
    let tail = read_at(file, tail_start, len - tail_start)?;
    let mut claimed: Option<u64> = None;
    for at in find_all(&tail, b"PK\x05\x06") {
        let Some(eocd) = tail.get(at..at + EOCD_LEN) else {
            continue;
        };
        let (on_disk, total) = (le::<2>(eocd, 8), le::<2>(eocd, 10));
        let (size, offset) = (le::<4>(eocd, 12), le::<4>(eocd, 16));
        let mut most = on_disk.max(total);
        // What makes the zip crate look for a ZIP64 record.
        if total == 0xFFFF || size == 0xFFFF_FFFF || offset == 0xFFFF_FFFF {
            most = most.max(zip64_claim(file, tail_start + at as u64)?);
        }
        claimed = Some(claimed.unwrap_or(0).max(most));
    }
    Ok(claimed)
}

/// What the ZIP64 end record for the end record at `eocd` claims. The zip
/// crate takes only a record that ends exactly at the locator before the
/// end record. If none is found within [`EOCD64_WINDOW`] before it, the
/// most the zip crate would accept from any record: one entry per 46 bytes
/// before the locator.
fn zip64_claim(file: &File, eocd: u64) -> io::Result<u64> {
    let Some(locator) = eocd.checked_sub(LOCATOR_LEN) else {
        return Ok(0);
    };
    if !read_at(file, locator, LOCATOR_LEN)?.starts_with(b"PK\x06\x07") {
        // No locator: the zip crate reads the end record as a plain one.
        return Ok(0);
    }
    let start = locator.saturating_sub(EOCD64_WINDOW);
    let window = read_at(file, start, locator - start)?;
    let found = find_all(&window, b"PK\x06\x06")
        .filter(|&at| window.len() - at >= EOCD64_LEN)
        // The record's size field counts all but its first 12 bytes.
        .filter(|&at| le::<8>(&window, at + 4).checked_add(12) == Some((window.len() - at) as u64))
        .map(|at| le::<8>(&window, at + 24).max(le::<8>(&window, at + 32)))
        .max();
    Ok(found.unwrap_or(locator / CENTRAL_LEN as u64))
}

/// The raw name of the first file the ZIP's central directory (from
/// `start`) names twice, byte for byte, if any. The zip crate keeps one
/// entry per name: the later record replaces the earlier one in its
/// place, so the archive would be written with one of the two, silently.
/// A folder named twice is fine, as everywhere (they are the same folder).
fn repeated_name(run: &Run<'_>, src: &Path, start: u64) -> Result<Option<Vec<u8>>, Failure> {
    let failed = |e: io::Error| Failure::Read(format!("zip directory again: {:?}", e.kind()));
    let file = File::open(src).map_err(failed)?;
    let mut reader = BufReader::with_capacity(SOURCE_BUF, Cancellable::new(file, run.token()));
    reader.seek(SeekFrom::Start(start)).map_err(failed)?;
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    // The zip crate parsed at most this many records.
    for index in 0..=run.limits.max_entries {
        if index % CANCEL_EVERY as u64 == 0 {
            run.check()?;
        }
        let record = read_prefix(&mut reader, CENTRAL_LEN).map_err(failed)?;
        if record.len() < CENTRAL_LEN || !record.starts_with(b"PK\x01\x02") {
            // The end records follow the last one.
            break;
        }
        let name_len = le::<2>(&record, 28);
        let skip = le::<2>(&record, 30) + le::<2>(&record, 32);
        let name = read_prefix(&mut reader, name_len as usize).map_err(failed)?;
        reader.seek_relative(skip as i64).map_err(failed)?;
        if !name.ends_with(b"/") && !seen.insert(name.clone()) {
            return Ok(Some(name));
        }
    }
    Ok(None)
}

/// A ZIP symlink's target (its data), or `None` if it is too long or
/// isn't text.
fn zip_link_target<R: Read + io::Seek>(
    run: &Run<'_>,
    zip: &mut ZipArchive<R>,
    index: usize,
) -> Result<Option<String>, Failure> {
    let mut file = zip
        .by_index(index)
        .map_err(|e| run.zip_failure(e, Side::Read, "zip link"))?;
    let size = file.size();
    if size > MAX_LINK_TARGET {
        return Ok(None);
    }
    let mut target = Vec::new();
    Pump::new(run, Exact::new(&mut file, size))
        .read_to_end(&mut target)
        .map_err(|e| run.failure(&e, Side::Read, "zip link"))?;
    Ok(String::from_utf8(target).ok())
}

/// What a tar entry is.
fn tar_raw<R: Read>(entry: &tar::Entry<'_, R>) -> Raw {
    let header = entry.header();
    let kind = header.entry_type();
    let raw_kind = if kind.is_file() || kind.is_contiguous() || kind.is_gnu_sparse() {
        // A sparse file reads back expanded, at its full size.
        RawKind::File {
            size: Some(entry.size()),
        }
    } else if kind.is_dir() {
        RawKind::Dir
    } else if kind.is_symlink() {
        RawKind::Symlink(entry.link_name_bytes().map(|target| names::decode(&target)))
    } else if kind.is_hard_link() {
        RawKind::HardLink
    } else if kind.is_pax_global_extensions() || kind.as_byte() == b'V' {
        RawKind::Ignore
    } else {
        // Devices, FIFOs, and types Grape doesn't know.
        RawKind::Special
    };
    Raw {
        name: names::decode(&entry.path_bytes()),
        kind: raw_kind,
        // A negative base-256 time (before 1970, as bsdtar writes one)
        // reads as a huge number: no time, rather than the year 2107.
        mtime: header.mtime().ok().and_then(|t| i64::try_from(t).ok()),
        mode: header.mode().ok(),
    }
}

/// What a [`Tap`] learns about a tar entry beyond its [`Raw`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TarFacts {
    /// A regular file (or a contiguous one).
    pub(crate) regular: bool,
    /// Its data's size, PAX `size` included.
    pub(crate) size: u64,
    /// Its PAX records say `hdrcharset=BINARY`: the name isn't UTF-8.
    pub(crate) binary_name: bool,
}

/// Watches a tar walk: every entry before it is checked, every byte of its
/// data, and its end.
pub(crate) trait Tap {
    /// False for [`NoTap`]: facts aren't computed, so plain tars behave
    /// exactly as before taps existed.
    const ACTIVE: bool;
    /// Before `Walk::admit`. May rewrite `raw` or refuse the archive.
    fn entry(&mut self, facts: &TarFacts, raw: &mut Raw) -> Result<(), Failure>;
    /// Every data byte of the current entry, whether written or drained.
    fn data(&mut self, bytes: &[u8]);
    /// After the entry's data was read to its end.
    fn done(&mut self) -> Result<(), Failure>;
}

/// The tap of a plain tar: sees nothing, refuses nothing.
pub(crate) struct NoTap;

impl Tap for NoTap {
    const ACTIVE: bool = false;

    fn entry(&mut self, _facts: &TarFacts, _raw: &mut Raw) -> Result<(), Failure> {
        Ok(())
    }

    fn data(&mut self, _bytes: &[u8]) {}

    fn done(&mut self) -> Result<(), Failure> {
        Ok(())
    }
}

/// An entry's data, shown to a [`Tap`] as it is read.
struct Tapped<'t, R, T> {
    inner: R,
    tap: &'t mut T,
}

impl<R: Read, T: Tap> Read for Tapped<'_, R, T> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.tap.data(&buf[..n]);
        Ok(n)
    }
}

/// The [`TarFacts`] of `entry`.
fn tar_facts<R: Read>(entry: &mut tar::Entry<'_, R>) -> Result<TarFacts, Failure> {
    let kind = entry.header().entry_type();
    let regular = kind.is_file() || kind.is_contiguous();
    let size = entry.size();
    let malformed = || Failure::Corrupt("tar: a malformed PAX record".into());
    let mut binary_name = false;
    if let Some(records) = entry.pax_extensions().map_err(|_| malformed())? {
        for record in records {
            let record = record.map_err(|_| malformed())?;
            if record.key_bytes() == b"hdrcharset" && record.value_bytes() == b"BINARY" {
                binary_name = true;
            }
        }
    }
    Ok(TarFacts {
        regular,
        size,
        binary_name,
    })
}

/// Walks a tar stream, writing admitted entries to `sink` (none in
/// [`Mode::Check`]). Returns the stream, positioned after the archive's
/// end marker, so the caller can read what follows.
pub(crate) fn walk_tar<R: Read>(
    run: &Run<'_>,
    stream: R,
    walk: &mut Walk,
    sink: Option<&mut dyn Sink>,
) -> Result<R, Failure> {
    walk_tar_with(run, stream, walk, sink, &mut NoTap)
}

/// [`walk_tar`], with `tap` shown every entry before it is checked and
/// every byte of its data ([`Mode::Check`] reads no data, so a tap sees
/// only the entries there).
pub(crate) fn walk_tar_with<R: Read, T: Tap>(
    run: &Run<'_>,
    stream: R,
    walk: &mut Walk,
    mut sink: Option<&mut dyn Sink>,
    tap: &mut T,
) -> Result<R, Failure> {
    let mut archive = tar::Archive::new(stream);
    {
        run.start_entry();
        let entries = archive
            .entries()
            .map_err(|e| run.failure(&e, Side::Read, "tar"))?;
        for item in entries {
            run.check()?;
            let mut entry = item.map_err(|e| run.failure(&e, Side::Read, "tar header"))?;
            let mut raw = tar_raw(&entry);
            if T::ACTIVE {
                let facts = tar_facts(&mut entry)?;
                tap.entry(&facts, &mut raw)?;
            }
            let admitted = walk.admit(run, raw)?;
            if walk.mode == Mode::Check {
                // The data isn't read: the tar reader skips it on its way
                // to the next header, and those bytes count as data. Read
                // here, a sparse entry's holes would count at full size.
                let skipped = check_pax(&mut entry)?;
                run.start_entry_after(skipped);
                continue;
            }
            let mut entry = Tapped {
                inner: &mut entry,
                tap: &mut *tap,
            };
            if let (Some(admitted), Some(sink)) = (admitted, sink.as_deref_mut()) {
                match &admitted.kind {
                    Kind::Dir => sink.dir(run, &admitted)?,
                    Kind::Symlink(target) => sink.symlink(run, &admitted, target)?,
                    Kind::File { size } => {
                        let size = size.unwrap_or(0);
                        let mut data = Pump::new(run, Exact::new(&mut entry, size));
                        sink.file(run, &admitted, Some(size), &mut data)?;
                    }
                }
            }
            drain(run, &mut entry, "tar entry")?;
            tap.done()?;
            run.start_entry();
        }
    }
    Ok(archive.into_inner())
}

/// For [`Mode::Check`], where the tar's bytes are kept: other tools must
/// read the name Grape checked. The tar crate takes a GNU long name over a
/// PAX `path`, and the first of two `path` records, where other readers
/// take the PAX one, or the last (Python's `tarfile` and GNU tar read
/// `../../evil.txt` where the tar crate reads `ok.txt`). So an entry's PAX
/// records may hold at most one `path`, equal to the name checked, and a
/// global header none. Returns the bytes of data, padding included, the
/// tar reader skips to reach the next header: the stored size, or PAX
/// `size` as the tar crate reads it (none for a global header, whose
/// records were just read).
fn check_pax<R: Read>(entry: &mut tar::Entry<'_, R>) -> Result<u64, Failure> {
    let name = entry.path_bytes().into_owned();
    let global = entry.header().entry_type().is_pax_global_extensions();
    let stored = entry
        .header()
        .entry_size()
        .map_err(|e| Failure::Corrupt(format!("tar size: {:?}", e.kind())))?;
    let malformed = || Failure::Corrupt("tar: a malformed PAX record".into());
    let Some(records) = entry.pax_extensions().map_err(|_| malformed())? else {
        return Ok(stored.div_ceil(512).saturating_mul(512));
    };
    let mut paths = 0;
    let mut size: Option<Option<u64>> = None;
    for record in records {
        let record = record.map_err(|_| malformed())?;
        match record.key_bytes() {
            b"path" => {
                paths += 1;
                if global || paths > 1 || record.value_bytes() != name.as_slice() {
                    let other = names::decode(record.value_bytes());
                    return Err(Failure::UnsafeName(names::shown(&other)));
                }
            }
            // The tar crate takes the first `size`, if it reads as a number.
            b"size" if size.is_none() => {
                size = Some(record.value().ok().and_then(|v| v.parse::<u64>().ok()));
            }
            _ => {}
        }
    }
    let data = match (global, size) {
        (true, _) => 0,
        (false, Some(Some(size))) => size,
        (false, _) => stored,
    };
    Ok(data.div_ceil(512).saturating_mul(512))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::CancelToken;

    fn raw(name: &str, kind: RawKind) -> Raw {
        Raw {
            name: name.to_string(),
            kind,
            mtime: Some(1),
            mode: Some(0o4777),
        }
    }

    fn file() -> RawKind {
        RawKind::File { size: Some(1) }
    }

    #[test]
    fn walk_decides_per_mode() {
        let cancel = CancelToken::new();
        let mut progress = |_: f32| {};
        let run = Run::new(&cancel, super::super::Limits::default(), 1, &mut progress);

        let limits = super::super::Limits::default();
        let mut extract = Walk::new(Mode::Extract, &limits);
        let written = extract
            .admit(&run, raw("a/b.txt", file()))
            .unwrap()
            .unwrap();
        assert_eq!(written.path, ["a", "b.txt"]);
        // setuid, sticky and group/other write are dropped.
        assert_eq!(written.mode, 0o755);
        let link = RawKind::Symlink(Some("b.txt".into()));
        assert_eq!(
            extract.admit(&run, raw("a/link", link.clone())).unwrap(),
            None
        );
        assert_eq!(
            extract.admit(&run, raw("h", RawKind::HardLink)).unwrap(),
            None
        );
        assert_eq!(
            extract.admit(&run, raw("fifo", RawKind::Special)).unwrap(),
            None
        );
        assert_eq!(
            extract
                .admit(&run, raw("__MACOSX/a/._b.txt", file()))
                .unwrap(),
            None
        );
        assert_eq!(
            extract.admit(&run, raw("__MACOSX/", RawKind::Dir)).unwrap(),
            None
        );
        assert_eq!(extract.admit(&run, raw("._x", file())).unwrap(), None);
        assert_eq!(extract.admit(&run, raw("./", RawKind::Dir)).unwrap(), None);
        assert_eq!(
            extract
                .admit(&run, raw("/tmp/GlobalHead.1", RawKind::Ignore))
                .unwrap(),
            None
        );
        assert_eq!(
            extract.left_out,
            LeftOut {
                links: 2,
                specials: 1,
                mac_metadata: 2,
                copies: 0,
            }
        );
        assert_eq!(
            extract
                .admit(&run, raw("copy.txt", RawKind::FileCopy))
                .unwrap(),
            None
        );
        assert_eq!(extract.left_out.copies, 1);
        let dir = extract.admit(&run, raw("old/", file())).unwrap().unwrap();
        assert_eq!(dir.kind, Kind::Dir);

        let mut repack = Walk::new(Mode::Repack, &limits);
        let kept = repack.admit(&run, raw("a/link", link)).unwrap().unwrap();
        assert_eq!(kept.kind, Kind::Symlink("b.txt".into()));
        let escaping = RawKind::Symlink(Some("../../etc/passwd".into()));
        assert_eq!(repack.admit(&run, raw("a/evil", escaping)).unwrap(), None);
        assert_eq!(repack.left_out.links, 1);

        // Check mode leaves nothing out but still refuses clashes.
        let mut check = Walk::new(Mode::Check, &limits);
        assert_eq!(
            check.admit(&run, raw("dev", RawKind::Special)).unwrap(),
            None
        );
        assert_eq!(check.left_out, LeftOut::default());
        assert!(matches!(
            check.admit(&run, raw("DEV", file())),
            Err(Failure::Duplicate(_))
        ));
    }

    #[test]
    fn walk_refuses_unsafe_names_even_for_entries_left_out() {
        let cancel = CancelToken::new();
        let mut progress = |_: f32| {};
        let run = Run::new(&cancel, super::super::Limits::default(), 1, &mut progress);
        let mut walk = Walk::new(Mode::Extract, &super::super::Limits::default());
        for (name, kind) in [
            ("../x", file()),
            ("__MACOSX/../../x", file()),
            ("CON", RawKind::Special),
            (".", file()),
        ] {
            assert!(
                matches!(
                    walk.admit(&run, raw(name, kind)),
                    Err(Failure::UnsafeName(_))
                ),
                "{name}"
            );
        }
    }

    #[test]
    fn zip_kinds_come_from_the_type_bits() {
        assert_eq!(
            zip_kind(Some(0o100_644), false),
            RawKind::File { size: None }
        );
        assert_eq!(zip_kind(None, false), RawKind::File { size: None });
        assert_eq!(zip_kind(Some(0o100_644), true), RawKind::Dir);
        assert_eq!(zip_kind(Some(0o040_755), false), RawKind::Dir);
        assert_eq!(zip_kind(Some(0o120_777), false), RawKind::Symlink(None));
        // zip's own `is_symlink` would call these links.
        assert_eq!(
            zip_kind(Some(0o160_000), false),
            RawKind::File { size: None }
        );
        assert_eq!(zip_kind(Some(0o010_644), false), RawKind::Special);
        assert_eq!(zip_kind(Some(0o140_755), false), RawKind::Special);
        assert_eq!(method_name(CompressionMethod::BZIP2), Some("bzip2"));
        assert_eq!(method_name(CompressionMethod::Stored), None);
    }

    #[test]
    fn a_tar_time_before_1970_is_no_time_not_the_year_2107() {
        // bsdtar writes a pre-1970 RAR5 time in base-256: -1 is all 0xFF.
        let mut header = tar::Header::new_ustar();
        header.set_path("old.txt").unwrap();
        header.set_size(0);
        header.as_old_mut().mtime = [0xFF; 12];
        header.set_cksum();
        let mut bytes = header.as_bytes().to_vec();
        bytes.extend([0u8; 1024]);
        let mut archive = tar::Archive::new(&bytes[..]);
        let entry = archive.entries().unwrap().next().unwrap().unwrap();
        assert_eq!(entry.header().mtime().unwrap(), u64::MAX);
        assert_eq!(tar_raw(&entry).mtime, None);

        // An ordinary time is unchanged.
        let mut header = tar::Header::new_ustar();
        header.set_path("new.txt").unwrap();
        header.set_size(0);
        header.set_mtime(1_700_000_000);
        header.set_cksum();
        let mut bytes = header.as_bytes().to_vec();
        bytes.extend([0u8; 1024]);
        let mut archive = tar::Archive::new(&bytes[..]);
        let entry = archive.entries().unwrap().next().unwrap().unwrap();
        assert_eq!(tar_raw(&entry).mtime, Some(1_700_000_000));
    }
}
