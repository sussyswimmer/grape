//! Grape's archive engine (DECISIONS D27): ZIP, TAR and GZIP conversions
//! among each other, and Extract Archive, in pure Rust (`zip`, `tar`,
//! `flate2` on miniz_oxide). RAR is read through Grape's own bsdtar
//! sidecar ([`rar`]): a Rust preflight of the RAR's headers, then bsdtar's
//! tar stream through the same checks and writers as any tar.
//!
//! - **Container by content.** [`sniff`] reads the first bytes: a `.tar`
//!   that is really gzip is read through gzip, and so on. A gzip file is a
//!   tarball if its data starts with a tar header, else one plain file
//!   (named by the header's FNAME, or the source's stem).
//! - **Every entry is checked** by [`source::Walk`] with one validator
//!   ([`names`]) before anything is written: unsafe names and two items
//!   with one name refuse the archive. Links, devices and macOS metadata
//!   are left out and counted ([`LeftOut`]); a conversion carries
//!   symlinks whose target stays inside the archive.
//! - **Bounds** ([`Limits`]) hold the entries and the bytes written as
//!   they stream, so a decompression bomb stops early.
//! - **Conversions** write a temp file next to the planned name and
//!   publish it with [`crate::publish::finalize`]. tar → tar.gz and back
//!   keep the tar's bytes exactly; everything else re-packs the entries.
//! - **Extract** writes into a hidden temp folder and publishes it with
//!   [`folder::publish`], which never replaces an existing folder.
//! - The source is opened read-only and never changed. On an error or a
//!   cancel, everything the job created is removed.

pub(crate) mod folder;
pub(crate) mod gz;
pub(crate) mod motw;
pub(crate) mod names;
pub(crate) mod rar;
pub(crate) mod sink;
pub(crate) mod sniff;
pub(crate) mod source;
pub(crate) mod stream;
pub(crate) mod time;

#[cfg(test)]
mod tests;

use std::fs::{self, File};
use std::io::{Seek, SeekFrom};
use std::path::Path;

use flate2::write::GzEncoder;
use flate2::Compression;
use grape_core::{Format, Job};

use crate::output::TempFile;
use crate::publish::{self, Names};
use crate::{EngineCtx, ErrorKind, GrapeError, JobOutcome};

use folder::TempDir;
use gz::{GzInfo, GzMembers};
use motw::Mark;
use rar::{Launch, RarFailure, RarPaths};
use sink::{finish_out, FolderSink, Out, Sink, TarSink, ZipSink, LEVEL};
use sniff::{is_empty_tar, sniff, Container, EMPTY_TAR_MAX};
use source::{drain, walk_tar, walk_zip, Kind, Mode, Opened, Raw, RawKind, Walk};
use stream::{read_prefix, Exact, Metered, Pump, Run, Side, Tee, MAX_HEADER_BYTES};

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
const GIB: u64 = 1024 * MIB;

/// The bounds a job enforces on what an archive unpacks to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Limits {
    /// The most entries an archive may have, written or not.
    pub(crate) max_entries: u64,
    /// The most bytes written in all.
    pub(crate) max_total: u64,
    /// Past this many bytes written, [`Limits::max_ratio`] applies.
    pub(crate) ratio_floor: u64,
    /// The most bytes written per byte of archive, past the floor.
    pub(crate) max_ratio: u64,
}

impl Default for Limits {
    /// 100,000 entries; 64 GiB; past 1 GiB, at most 1000× the archive.
    fn default() -> Self {
        Limits {
            max_entries: 100_000,
            max_total: 64 * GIB,
            ratio_floor: GIB,
            max_ratio: 1000,
        }
    }
}

/// Which bound a job went over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Over {
    /// [`Limits::max_entries`].
    Entries,
    /// [`Limits::max_total`].
    Total,
    /// [`Limits::max_ratio`].
    Ratio,
    /// One tar entry's headers ran over `MAX_HEADER_BYTES`.
    Header,
}

impl Over {
    /// The bound in words, for the user message.
    fn why(self, limits: &Limits) -> String {
        match self {
            Over::Entries => format!("more than {} items", thousands(limits.max_entries)),
            Over::Total => format!("more than {}", bytes(limits.max_total)),
            Over::Ratio => format!("more than {} times its own size", limits.max_ratio),
            Over::Header => format!("an item header over {}", bytes(MAX_HEADER_BYTES)),
        }
    }
}

/// What an archive job left out, for the app to word ("2 links were left
/// out"). [`JobOutcome`] itself is unchanged.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct LeftOut {
    /// Symbolic links (all of them when extracting; escaping ones when
    /// converting) and tar hard links.
    pub(crate) links: u32,
    /// Devices, FIFOs, sockets and tar entry types Grape doesn't know.
    pub(crate) specials: u32,
    /// Files under `__MACOSX/` and AppleDouble `._` files.
    pub(crate) mac_metadata: u32,
    /// RAR5 file-copy references (WinRAR's "store identical files as
    /// references").
    pub(crate) copies: u32,
}

/// What a successful archive job wrote, and what it left out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ArchiveOutcome {
    /// The output, as every engine reports it.
    pub(crate) outcome: JobOutcome,
    /// What wasn't carried over.
    pub(crate) left_out: LeftOut,
}

/// Why an archive job failed, before it becomes a [`GrapeError`]. Neither
/// the details nor `Display` ever hold a path or an entry name: they go to
/// the log. The names a toast shows travel in the variants' fields.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Failure {
    /// The job was cancelled.
    #[error("cancelled")]
    Cancelled,
    /// The bytes match no archive Grape reads.
    #[error("no ZIP, gzip, tar or RAR signature at the start")]
    NotArchive,
    /// The archive is damaged (a bad checksum, truncated data, a broken
    /// header); the detail says where.
    #[error("damaged archive: {0}")]
    Corrupt(String),
    /// An entry (or a RAR's headers) is encrypted.
    #[error("an encrypted entry")]
    Encrypted,
    /// A ZIP entry uses a compression method this build can't read (its
    /// name, if Grape knows it).
    #[error("ZIP compression method {}", .0.unwrap_or("unknown"))]
    Unsupported(Option<&'static str>),
    /// An entry's name failed the name check (the name, as shown).
    #[error("an entry name failed the name check")]
    UnsafeName(String),
    /// Two entries share a name, ignoring case (the name, as shown).
    #[error("two entries share a name, ignoring case")]
    Duplicate(String),
    /// The archive unpacks to more than [`Limits`] allow.
    #[error("over the {0:?} bound")]
    TooBig(Over),
    /// The source couldn't be read.
    #[error("reading failed: {0}")]
    Read(String),
    /// The output couldn't be written.
    #[error("writing failed: {0}")]
    Write(String),
    /// A failure already in Grape's words (from publishing).
    #[error("publishing failed: {:?}", .0.kind)]
    Grape(GrapeError),
    /// A RAR this build won't read, or bsdtar failed.
    #[error("{0}")]
    Rar(RarFailure),
}

impl Failure {
    /// True for a refusal of Grape's own (a cancel, a bound, a name, the
    /// output): bsdtar is killed at once, and its own failure never
    /// replaces one of these. Anything else is a broken stream.
    pub(crate) fn is_policy(&self) -> bool {
        matches!(
            self,
            Failure::Cancelled
                | Failure::TooBig(_)
                | Failure::UnsafeName(_)
                | Failure::Duplicate(_)
                | Failure::Write(_)
                | Failure::Grape(_)
                | Failure::Rar(RarFailure::Names(_))
        )
    }

    /// The error in Grape's words; the log detail is the failure's own
    /// `Display`, which never names a file.
    fn into_error(self, src: &Path, names: &Names, limits: &Limits) -> GrapeError {
        let source = &names.source;
        let detail = self.to_string();
        let bad_input =
            |message: String| GrapeError::new(ErrorKind::DecodeFailed, message, &detail);
        match self {
            Failure::Cancelled => crate::cancelled(src),
            Failure::NotArchive => {
                bad_input(format!("{source} doesn't look like a {} file.", names.from))
            }
            Failure::Corrupt(_) => names.decode_failed(detail),
            Failure::Encrypted => bad_input(format!("{source} is password-protected.")),
            Failure::Unsupported(method) => GrapeError::new(
                ErrorKind::NotYetSupported,
                match method {
                    Some(method) => {
                        format!("{source} uses {method} compression, which Grape can't read yet.")
                    }
                    None => format!("{source} uses a kind of compression Grape can't read yet."),
                },
                &detail,
            ),
            Failure::UnsafeName(name) => bad_input(format!(
                "{source} has an item with a name Grape won't write: {name}."
            )),
            Failure::Duplicate(name) => bad_input(format!("{source} has two items named {name}.")),
            Failure::TooBig(over) => bad_input(format!(
                "{source} is larger unpacked than Grape will write ({}).",
                over.why(limits)
            )),
            Failure::Read(_) => names.read_failed(detail),
            Failure::Write(_) => names.write_failed(detail),
            Failure::Grape(error) => error,
            Failure::Rar(rar) => rar.into_error(source, detail),
        }
    }
}

/// `n` with thousands separators: `100,000`.
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A byte count in the largest whole binary unit: `64 GiB`, `16 MiB`.
fn bytes(n: u64) -> String {
    for (unit, name) in [(GIB, "GiB"), (MIB, "MiB"), (KIB, "KiB")] {
        if n >= unit && n.is_multiple_of(unit) {
            return format!("{} {name}", thousands(n / unit));
        }
    }
    format!("{} bytes", thousands(n))
}

/// True for the six v1 pairs (ZIP → TAR or GZIP, TAR → ZIP or GZIP, GZIP →
/// ZIP or TAR), and RAR → ZIP, TAR or GZIP when this install has bsdtar
/// (`has_bsdtar`: the app found `SidecarPaths::bsdtar`).
pub(crate) fn supports(from: Format, to: Format, has_bsdtar: bool) -> bool {
    matches!(
        (from, to),
        (Format::Zip, Format::Tar | Format::Gz)
            | (Format::Tar, Format::Zip | Format::Gz)
            | (Format::Gz, Format::Zip | Format::Tar)
    ) || (has_bsdtar && from == Format::Rar && matches!(to, Format::Zip | Format::Tar | Format::Gz))
}

/// True when Extract Archive runs on `from` in this build: ZIP, TAR, GZIP,
/// and RAR when this install has bsdtar.
pub(crate) fn extracts(from: Format, has_bsdtar: bool) -> bool {
    matches!(from, Format::Zip | Format::Tar | Format::Gz) || (has_bsdtar && from == Format::Rar)
}

/// Converts the archive `src` (of format `from`, by its name) to `to` at
/// `dest`, the path the plan reserved. `rar` says where bsdtar is, if this
/// install has it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn convert(
    job: &Job,
    src: &Path,
    dest: &Path,
    from: Format,
    to: Format,
    ctx: &EngineCtx,
    progress: &mut dyn FnMut(f32),
    rar: Option<&RarPaths>,
) -> Result<ArchiveOutcome, GrapeError> {
    let launch = rar.map(Launch::bsdtar);
    convert_with(
        job,
        src,
        dest,
        from,
        to,
        ctx,
        progress,
        Limits::default(),
        launch.as_ref(),
    )
}

/// [`convert`] under `limits`, starting bsdtar as `rar` says.
#[allow(clippy::too_many_arguments)]
pub(crate) fn convert_with(
    job: &Job,
    src: &Path,
    dest: &Path,
    from: Format,
    to: Format,
    ctx: &EngineCtx,
    progress: &mut dyn FnMut(f32),
    limits: Limits,
    rar: Option<&Launch>,
) -> Result<ArchiveOutcome, GrapeError> {
    let names = Names::new(src, dest, from, to);
    let report = |failure: Failure| failure.into_error(src, &names, &limits);
    let dir = publish::dest_dir(dest, &names)?;
    if !supports(from, to, rar.is_some()) {
        return Err(crate::not_yet_supported(job, Some(from)));
    }
    if ctx.cancel.is_cancelled() {
        return Err(crate::cancelled(src));
    }
    let (file, len, container) = open_source(src, from).map_err(report)?;
    if container == Container::Rar && rar.is_none() {
        return Err(report(Failure::Rar(RarFailure::NoHelper)));
    }

    publish::ensure_dir(job, ctx, dir, &names)?;
    // From here on, `temp` removes the temp file on every return path.
    let (temp, out) = TempFile::create(dir, to.output_ext())
        .map_err(|e| names.write_failed(format!("creating the temp file: {:?}", e.kind())))?;
    let left_out = {
        let run = Run::new(&ctx.cancel, limits, len, &mut *progress);
        convert_into(
            &run,
            Input {
                file,
                len,
                container,
            },
            src,
            to,
            out,
            rar,
        )
    }
    .map_err(report)?;
    let mark = Mark::of(src);
    mark.apply(temp.path());

    if ctx.cancel.is_cancelled() {
        return Err(crate::cancelled(src));
    }
    let outcome = finalize_marked(&temp, job, src, dest, dir, &names, &mark)?;
    drop(temp);
    progress(1.0);
    Ok(ArchiveOutcome { outcome, left_out })
}

/// Publishes the temp file as [`publish::finalize`] does, then writes the
/// source's mark onto the published file as well. The mark is already on
/// the temp file, which a hard link carries; the copy fallback (a volume
/// without hard links) copies only the file's data.
fn finalize_marked(
    temp: &TempFile,
    job: &Job,
    src: &Path,
    dest: &Path,
    dir: &Path,
    names: &Names,
    mark: &Mark,
) -> Result<JobOutcome, GrapeError> {
    let outcome = publish::finalize(temp, job, src, dest, dir, names)?;
    if let Some(output) = outcome.outputs.first() {
        mark.apply(output);
    }
    Ok(outcome)
}

/// Extracts the archive `src` (of format `from`, by its name) into a new
/// folder at `dest_folder`, the path the plan reserved (`{stem} Extracted`).
/// `rar` says where bsdtar is, if this install has it.
pub(crate) fn extract(
    job: &Job,
    src: &Path,
    dest_folder: &Path,
    from: Format,
    ctx: &EngineCtx,
    progress: &mut dyn FnMut(f32),
    rar: Option<&RarPaths>,
) -> Result<ArchiveOutcome, GrapeError> {
    let launch = rar.map(Launch::bsdtar);
    extract_with(
        job,
        src,
        dest_folder,
        from,
        ctx,
        progress,
        Limits::default(),
        launch.as_ref(),
    )
}

/// [`extract`] under `limits`, starting bsdtar as `rar` says.
#[allow(clippy::too_many_arguments)]
pub(crate) fn extract_with(
    job: &Job,
    src: &Path,
    dest_folder: &Path,
    from: Format,
    ctx: &EngineCtx,
    progress: &mut dyn FnMut(f32),
    limits: Limits,
    rar: Option<&Launch>,
) -> Result<ArchiveOutcome, GrapeError> {
    let names = Names::new(src, dest_folder, from, from);
    let report = |failure: Failure| failure.into_error(src, &names, &limits);
    let dir = publish::dest_dir(dest_folder, &names)?;
    if !extracts(from, rar.is_some()) {
        return Err(crate::not_yet_supported(job, Some(from)));
    }
    if ctx.cancel.is_cancelled() {
        return Err(crate::cancelled(src));
    }
    let (file, len, container) = open_source(src, from).map_err(report)?;
    if container == Container::Rar && rar.is_none() {
        return Err(report(Failure::Rar(RarFailure::NoHelper)));
    }

    publish::ensure_dir(job, ctx, dir, &names)?;
    // From here on, `temp` removes the temp folder on every return path.
    let temp = TempDir::create(dir)
        .map_err(|e| names.write_failed(format!("creating the temp folder: {:?}", e.kind())))?;
    let left_out = {
        let run = Run::new(&ctx.cancel, limits, len, &mut *progress);
        extract_into(
            &run,
            Input {
                file,
                len,
                container,
            },
            src,
            temp.path(),
            rar,
        )
    }
    .map_err(report)?;

    if ctx.cancel.is_cancelled() {
        return Err(crate::cancelled(src));
    }
    let outcome = folder::publish(&temp, job, src, dest_folder, dir, &names).map_err(report)?;
    drop(temp);
    progress(1.0);
    Ok(ArchiveOutcome { outcome, left_out })
}

/// Opens `src` read-only and sniffs it: the file (back at its start), its
/// size and its real container. A file of only zero blocks is an empty tar.
/// For anything else, `from` (the format by the name) decides between a
/// self-extracting RAR and no archive at all ([`rar::header::foreign`]).
fn open_source(src: &Path, from: Format) -> Result<(File, u64, Container), Failure> {
    let read = |what: &str, e: std::io::Error| Failure::Read(format!("{what}: {:?}", e.kind()));
    let mut file = File::open(src).map_err(|e| read("opening the source", e))?;
    let len = file
        .metadata()
        .map_err(|e| read("reading the source's size", e))?
        .len();
    let head =
        read_prefix(&mut file, EMPTY_TAR_MAX + 1).map_err(|e| read("reading the source", e))?;
    file.seek(SeekFrom::Start(0))
        .map_err(|e| read("reading the source", e))?;
    match sniff(&head).or_else(|| is_empty_tar(&head).then_some(Container::Tar)) {
        Some(container) => Ok((file, len, container)),
        None => Err(rar::header::foreign(&mut file, &head, from)),
    }
}

/// The opened source: the file (at its start), its size and its container.
struct Input {
    file: File,
    len: u64,
    container: Container,
}

/// bsdtar's launch, for a RAR (the callers checked it is there).
fn helper(rar: Option<&Launch>) -> Result<&Launch, Failure> {
    rar.ok_or(Failure::Rar(RarFailure::NoHelper))
}

/// Writes `to` into `out` from the source.
fn convert_into(
    run: &Run<'_>,
    input: Input,
    src: &Path,
    to: Format,
    out: File,
    rar: Option<&Launch>,
) -> Result<LeftOut, Failure> {
    let Input {
        file,
        len,
        container,
    } = input;
    let out: Out = std::io::BufWriter::with_capacity(
        256 * KIB as usize,
        crate::cancel::Cancellable::new(out, run.token()),
    );
    let mut walk = Walk::new(Mode::Repack, &run.limits);
    if container == Container::Rar {
        // Always re-packed, never kept: bsdtar's PAX records don't reach
        // the user.
        let launch = helper(rar)?;
        let out = to_target(run, to, out, |sink| {
            rar::walk(run, file, len, launch, &mut walk, sink)
        })?;
        drop(finish_out(run, out)?);
        return Ok(walk.left_out);
    }
    let out = match source::open(run, file, container)? {
        Opened::Zip(file) => to_target(run, to, out, |sink| {
            walk_zip(run, file, src, &mut walk, sink)
        })?,
        Opened::Tar(stream) => match to {
            Format::Tar => pass_through(run, stream, out)?,
            Format::Gz => {
                let gz = pass_through(run, stream, GzEncoder::new(out, Compression::new(LEVEL)))?;
                gz.finish()
                    .map_err(|e| run.failure(&e, Side::Write, "finishing the gzip stream"))?
            }
            _ => to_target(run, to, out, |sink| {
                let mut rest = walk_tar(run, stream, &mut walk, Some(sink))?;
                drain(run, &mut rest, "after the tar").map(drop)
            })?,
        },
        Opened::Plain(stream, info) => {
            // Measured once, then written: a tar entry needs its size first.
            run.set_passes(2);
            to_target(run, to, out, |sink| {
                plain_into(run, stream, info.as_ref(), src, &mut walk, sink)
            })?
        }
    };
    drop(finish_out(run, out)?);
    Ok(walk.left_out)
}

/// Runs `fill` against a new archive of format `to` written to `out`, then
/// finishes the archive and returns `out`.
fn to_target(
    run: &Run<'_>,
    to: Format,
    out: Out,
    fill: impl FnOnce(&mut dyn Sink) -> Result<(), Failure>,
) -> Result<Out, Failure> {
    match to {
        Format::Zip => {
            let mut sink = ZipSink::new(out);
            fill(&mut sink)?;
            sink.finish(run)
        }
        Format::Tar => {
            let mut sink = TarSink::new(out);
            fill(&mut sink)?;
            sink.finish(run)
        }
        Format::Gz => {
            let mut sink = TarSink::new(GzEncoder::new(out, Compression::new(LEVEL)));
            fill(&mut sink)?;
            sink.finish(run)?
                .finish()
                .map_err(|e| run.failure(&e, Side::Write, "finishing the gzip stream"))
        }
        other => Err(Failure::Write(format!(
            "no archive writer for {}",
            other.key()
        ))),
    }
}

/// Copies a tar stream's bytes to `out` unchanged while checking every
/// entry's name, then the bytes after the archive's end too.
fn pass_through<W: std::io::Write>(
    run: &Run<'_>,
    stream: source::Stream<'_>,
    out: W,
) -> Result<W, Failure> {
    let mut walk = Walk::new(Mode::Check, &run.limits);
    let mut tee = walk_tar(run, Tee::new(run, stream, out), &mut walk, None)?;
    drain(run, &mut tee, "after the tar")?;
    Ok(tee.into_out())
}

/// A plain `.gz`'s one entry: named by FNAME if that is one safe name,
/// else the source's stem (`notes.txt.gz` → `notes.txt`); dated by MTIME,
/// else the source's own mtime.
fn plain_raw(info: Option<&GzInfo>, src: &Path) -> Raw {
    let name = info
        .and_then(|info| info.name.as_deref())
        .map(names::decode)
        .filter(|name| !name.contains(['/', '\\']) && names::component_ok(name))
        .unwrap_or_else(|| grape_core::naming::stem(&publish::file_name(src)).to_string());
    let mtime = info
        .map(|info| info.mtime)
        .filter(|&mtime| mtime != 0)
        .map(i64::from)
        .or_else(|| {
            fs::metadata(src)
                .and_then(|meta| meta.modified())
                .ok()
                .map(time::from_system)
        });
    Raw {
        name,
        kind: RawKind::File { size: None },
        mtime,
        mode: Some(0o644),
    }
}

/// A plain `.gz` into an archive: measures the data (checking every
/// member), then reads the file again and writes exactly that many bytes.
fn plain_into(
    run: &Run<'_>,
    mut first: source::Stream<'_>,
    info: Option<&GzInfo>,
    src: &Path,
    walk: &mut Walk,
    sink: &mut dyn Sink,
) -> Result<(), Failure> {
    let Some(mut entry) = walk.admit_single(run, plain_raw(info, src))? else {
        // Nothing to write; the data is still checked to its end.
        drain(run, &mut first, "gzip data")?;
        return Ok(());
    };
    let size = drain(run, &mut first, "gzip data")?;
    drop(first);
    run.reset_budget();
    let again = File::open(src)
        .map_err(|e| Failure::Read(format!("opening the source again: {:?}", e.kind())))?;
    let (members, _) = GzMembers::new(source::source(run, again));
    let mut stream = Metered::new(run, members);
    entry.kind = Kind::File { size: Some(size) };
    let mut data = Pump::new(run, Exact::new(&mut stream, size));
    sink.file(run, &entry, Some(size), &mut data)
}

/// Extracts the source into the folder `root`.
fn extract_into(
    run: &Run<'_>,
    input: Input,
    src: &Path,
    root: &Path,
    rar: Option<&Launch>,
) -> Result<LeftOut, Failure> {
    let Input {
        file,
        len,
        container,
    } = input;
    let mut sink = FolderSink::new(root, Mark::of(src));
    let mut walk = Walk::new(Mode::Extract, &run.limits);
    if container == Container::Rar {
        rar::walk(run, file, len, helper(rar)?, &mut walk, &mut sink)?;
        sink.finish(run)?;
        return Ok(walk.left_out);
    }
    match source::open(run, file, container)? {
        Opened::Zip(file) => walk_zip(run, file, src, &mut walk, &mut sink)?,
        Opened::Tar(stream) => {
            let mut rest = walk_tar(run, stream, &mut walk, Some(&mut sink))?;
            drain(run, &mut rest, "after the tar")?;
        }
        Opened::Plain(mut stream, info) => {
            if let Some(entry) = walk.admit_single(run, plain_raw(info.as_ref(), src))? {
                sink.file(run, &entry, None, &mut Pump::new(run, &mut stream))?;
            }
            drain(run, &mut stream, "gzip data")?;
        }
    }
    sink.finish(run)?;
    Ok(walk.left_out)
}
