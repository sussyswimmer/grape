//! A door for the SVG integration tests (`tests/svg.rs`) only: runs an SVG
//! job as `run_job` does, with the worker they name and bounds they may
//! lower (a short deadline, a small heap bound, their own work folder).
//! Hidden from the docs, and not part of the crate's interface.

use std::path::{Path, PathBuf};
use std::time::Duration;

use grape_core::{Format, Job, JobOutput, Operation, ToolKey};

use super::{convert_with, Limits, Renderer};
use crate::helper::Launch;
use crate::pdf::create::{self, Helpers};
use crate::{heif, EngineCtx, GrapeError, JobOutcome};

/// The bounds a test may change; the defaults are Grape's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// The time every job gets before its canvas is known.
    pub timeout_base: Duration,
    /// The worker's heap bound.
    pub max_heap_bytes: u64,
    /// Where the work folder is made (`None`: the OS temp folder).
    pub work_parent: Option<PathBuf>,
}

impl Default for Options {
    fn default() -> Options {
        let limits = Limits::default();
        Options {
            timeout_base: limits.timeout_base,
            max_heap_bytes: limits.worker.max_heap_bytes,
            work_parent: None,
        }
    }
}

/// Runs `job` (an SVG to a raster image, HEIC, AVIF or PDF, or Create PDF
/// with SVGs among its inputs) as `run_job` will, with `worker` started as
/// `<worker> --grape-svg-worker` under `options`, and the image helper from
/// `ctx.sidecars.heif`.
pub fn run(
    job: &Job,
    ctx: &EngineCtx,
    progress: &mut dyn FnMut(f32),
    worker: &Path,
    options: &Options,
) -> Result<JobOutcome, GrapeError> {
    let mut limits = Limits {
        timeout_base: options.timeout_base,
        ..Limits::default()
    };
    limits.worker.max_heap_bytes = options.max_heap_bytes;
    if let Some(parent) = &options.work_parent {
        limits.work_parent = parent.clone();
    }
    let renderer = Renderer::new(Launch::svg_worker(worker), limits);
    let from = job.inputs.first().and_then(|path| crate::format_of(path));
    match (job.inputs.as_slice(), &job.output, job.op) {
        ([src], JobOutput::File(dest), Operation::Convert { to: Format::Pdf }) => {
            let helpers = Helpers {
                heif: heif::Decoder::from_sidecars(&ctx.sidecars),
                svg: Some(renderer),
            };
            create::convert_with(job, src, dest, Format::Svg, ctx, progress, &helpers)
        }
        ([src], JobOutput::File(dest), Operation::Convert { to }) => {
            let helper = ctx.sidecars.heif.as_deref().map(Launch::heif_helper);
            let helper_limits = heif::Limits::default();
            let heif = helper.as_ref().map(|launch| (launch, &helper_limits));
            convert_with(job, src, dest, to, ctx, progress, &renderer, heif)
        }
        (
            _,
            JobOutput::File(dest),
            Operation::Tool {
                tool: ToolKey::CreatePdf,
                ..
            },
        ) => {
            let helpers = Helpers {
                heif: heif::Decoder::from_sidecars(&ctx.sidecars),
                svg: Some(renderer),
            };
            create::combine_with(job, dest, ctx, progress, &helpers)
        }
        _ => Err(crate::not_yet_supported(job, from)),
    }
}
