//! SVG input (DECISIONS D39): SVG → JPG, PNG, WebP, TIFF and BMP, HEIC and
//! AVIF with Grape's image helper, and (through `pdf::create`) PDF and
//! Create PDF pages, all drawn by resvg in Grape's SVG worker, a separate
//! process. Lane A's module, by agreement with lane B, who owns this crate.
//!
//! - **Every SVG is drawn in the worker,** `Grape --grape-svg-worker`
//!   ([`worker_main`]): resvg and usvg have no cancel, progress or budget
//!   hook, their time and memory aren't bounded by the file's size, and a
//!   stack overflow or a failed allocation ends the process it runs in.
//!   The worker holds itself to the request's limits and a heap bound
//!   ([`crate::pdf::heap`]); the parent (`run`) kills it on a cancel or a
//!   timeout, and the app's container caps it.
//! - **References are `data:` only** (`resolve`): no file is ever read for
//!   an `<image>` or `<feImage>` href, and embedded data is held to a
//!   job-wide budget. What was left out is counted, and the toast says so
//!   ([`crate::Note::ImagesLeftOut`]).
//! - **Text** is drawn with the system's fonts, loaded only when the
//!   document can draw text.
//! - **The size rule** (`size`): the SVG's own size, twice when it's under
//!   256 px, or 300 DPI for a PDF page, fitted into the target's caps
//!   ([`crate::Note::DrawnSmaller`] when it was made smaller).
//! - **Outputs:** the raster engine publishes the pixels
//!   ([`image::publish_pixels`]: JPEG on white, PNG and TIFF with alpha,
//!   lossless WebP), HEIC and AVIF go through the image helper's encode
//!   ([`heif::encode_pixels`]), and a PDF page is drawn at the SVG's own
//!   size by `pdf::create`. A `.svg` whose content is a raster image is the
//!   raster engine's (FORMAT_MATRIX rule 3).
//! - **The protocol** (`protocol`): one JSON request on stdin, JSON lines
//!   on stdout, the pixels in `pixels.rgba` in a private work folder.

#[doc(hidden)]
pub mod it;
mod protocol;
mod resolve;
mod run;
mod size;
mod worker;

#[cfg(test)]
mod tests;

use std::path::Path;

use grape_core::{Format, Job};
use grape_heif_proto::HeifFormat;

pub(crate) use protocol::Mode;
pub(crate) use run::{Drawn, Limits, Rendered, Renderer, WORK_PREFIX};
pub use worker::worker_main;

use crate::helper::Launch;
use crate::publish::{self, Names};
use crate::{heif, image, EngineCtx, ErrorKind, GrapeError, JobOutcome, Note, SidecarPaths};

/// The first argument that turns Grape's executable into the SVG worker:
/// `Grape --grape-svg-worker`. The app checks for it before anything else
/// starts and then calls [`worker_main`].
pub const WORKER_FLAG: &str = "--grape-svg-worker";

/// True for SVG → JPG, PNG, WebP, TIFF or BMP with the worker
/// (`svg_worker`), and SVG → HEIC or AVIF with the image helper (`heif`)
/// too. SVG → PDF is `pdf::create`'s.
pub(crate) fn supports(from: Format, to: Format, sidecars: &SidecarPaths) -> bool {
    from == Format::Svg
        && sidecars.svg_worker.is_some()
        && (image::RASTER.contains(&to) || (heif::HEIF.contains(&to) && sidecars.heif.is_some()))
}

/// Converts the SVG `src` to `to` at `dest`, the path the plan reserved,
/// with the worker in `ctx.sidecars.svg_worker` (and, for HEIC and AVIF,
/// the image helper in `ctx.sidecars.heif`).
pub(crate) fn convert(
    job: &Job,
    src: &Path,
    dest: &Path,
    to: Format,
    ctx: &EngineCtx,
    progress: &mut dyn FnMut(f32),
) -> Result<JobOutcome, GrapeError> {
    // `run_job` sends a job here only with a worker; this is for direct
    // calls.
    let Some(renderer) = Renderer::from_sidecars(&ctx.sidecars) else {
        return Err(run::missing());
    };
    let helper = ctx.sidecars.heif.as_deref().map(Launch::heif_helper);
    let helper_limits = heif::Limits::default();
    let heif = helper.as_ref().map(|launch| (launch, &helper_limits));
    convert_with(job, src, dest, to, ctx, progress, &renderer, heif)
}

/// What a raster or HEIC/AVIF output is made with.
enum Target {
    /// JPG, PNG, WebP, TIFF or BMP: the raster engine.
    Raster,
    /// HEIC or AVIF: the image helper's encode.
    Heif(HeifFormat),
}

/// [`convert`] with this worker and image helper (with its limits).
#[allow(clippy::too_many_arguments)] // `convert`'s, plus the helpers.
pub(crate) fn convert_with(
    job: &Job,
    src: &Path,
    dest: &Path,
    to: Format,
    ctx: &EngineCtx,
    progress: &mut dyn FnMut(f32),
    renderer: &Renderer,
    heif: Option<(&Launch, &heif::Limits)>,
) -> Result<JobOutcome, GrapeError> {
    let from = Format::Svg;
    let names = Names::new(src, dest, from, to);
    publish::dest_dir(dest, &names)?;
    let cancel = &ctx.cancel;
    if cancel.is_cancelled() {
        return Err(run::cancelled());
    }
    let target = match to {
        Format::Heic => Target::Heif(HeifFormat::Heic),
        Format::Avif => Target::Heif(HeifFormat::Avif),
        _ if image::RASTER.contains(&to) => Target::Raster,
        _ => return Err(crate::not_yet_supported(job, Some(from))),
    };
    let helper = match (&target, heif) {
        (Target::Raster, _) => None,
        (Target::Heif(_), Some((launch, limits))) => Some(heif::Run {
            launch,
            limits,
            cancel,
        }),
        (Target::Heif(_), None) => {
            return Err(GrapeError::new(
                ErrorKind::NotYetSupported,
                "Grape's image helper is missing.",
                "SidecarPaths::heif is None",
            ))
        }
    };
    // What the pixels must fit: the target itself, or `Format::Heic` for
    // both HEIC and AVIF (the RGBA8 the helper's input is made from).
    let (fit, span) = match target {
        Target::Raster => (to, (0.1, 0.5)),
        Target::Heif(_) => (Format::Heic, (0.05, 0.4)),
    };
    let mut last = 0.0f32;
    let drawn = {
        let mut forward = |fraction: f32| {
            if fraction > last {
                last = fraction;
                progress(fraction);
            }
        };
        renderer.render(src, &names, Mode::Raster, fit, cancel, span, &mut forward)?
    };
    let mut forward = |fraction: f32| {
        if fraction > last {
            last = fraction;
            progress(fraction);
        }
    };
    match (drawn, target, helper) {
        // FORMAT_MATRIX rule 3: the content decides. The raster engine
        // keeps its own limits, and its messages name the file by its name.
        (Drawn::Raster, Target::Raster, _) => {
            image::convert(job, src, dest, from, to, ctx, &mut forward)
        }
        (Drawn::Raster, Target::Heif(format), Some(helper)) => {
            let (img, icc) = image::decode(src, from, Format::Heic, &names, cancel)
                .map_err(|e| publish::or_cancelled(cancel, src, e))?;
            forward(0.4);
            heif::encode_pixels(
                job,
                src,
                dest,
                from,
                format,
                img,
                icc,
                ctx,
                &mut forward,
                (0.5, 0.9),
                &helper,
            )
        }
        (Drawn::Svg(rendered), Target::Raster, _) => {
            forward(0.5);
            let notes = notes(&rendered);
            let outcome = image::publish_pixels(
                job,
                src,
                dest,
                from,
                to,
                rendered.img,
                None,
                ctx,
                &mut forward,
            )?;
            Ok(with_notes(outcome, notes))
        }
        (Drawn::Svg(rendered), Target::Heif(format), Some(helper)) => {
            forward(0.4);
            let notes = notes(&rendered);
            let outcome = heif::encode_pixels(
                job,
                src,
                dest,
                from,
                format,
                rendered.img,
                None,
                ctx,
                &mut forward,
                (0.5, 0.9),
                &helper,
            )?;
            Ok(with_notes(outcome, notes))
        }
        // `helper` is `Some` for every HEIC or AVIF target (above).
        (_, Target::Heif(_), None) => Err(crate::not_yet_supported(job, Some(from))),
    }
}

/// What the toast says about a drawn SVG: the references left out, then
/// whether it was drawn smaller to fit.
fn notes(rendered: &Rendered) -> Vec<Note> {
    let mut notes = Vec::new();
    if rendered.left_out > 0 {
        notes.push(Note::ImagesLeftOut(rendered.left_out));
    }
    if rendered.scaled {
        notes.push(Note::DrawnSmaller);
    }
    notes
}

fn with_notes(mut outcome: JobOutcome, notes: Vec<Note>) -> JobOutcome {
    outcome.notes.extend(notes);
    outcome
}
