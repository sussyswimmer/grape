//! Job planning: turn a selection and an operation into jobs with final
//! output paths. No execution and no disk access: everything the planner
//! knows about the disk is in [`Selection::dirs`].
//!
//! Sources of truth: `docs/FORMAT_MATRIX.md` → "Output naming" and
//! "Location", `docs/INTERACTION_SPEC.md` §7.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::naming::{
    fold, output_kind, output_name_folded, produces_pages, stem, unique_name, OutputKind,
};
use crate::operation::Operation;
use crate::selection::{wheel_content, Selection, SelectionItem, WheelEntry, WheelMode};
use crate::tier::Features;

/// Where outputs go (`output.location` in Settings).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OutputLocation {
    /// The same folder as each source (the default).
    #[default]
    NextToSource,
    /// A fixed folder chosen in Settings.
    Folder(PathBuf),
}

/// The settings planning depends on. The app maps its persisted settings
/// into this.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    /// `output.location`.
    pub output: OutputLocation,
    /// `output.fallback_dir`: used when the destination isn't writable.
    pub fallback_dir: PathBuf,
    /// `features.*`: which tiers are offered.
    pub features: Features,
}

impl Settings {
    /// Outputs next to the source, v1 only, with this fallback folder.
    pub fn new(fallback_dir: impl Into<PathBuf>) -> Settings {
        Settings {
            output: OutputLocation::NextToSource,
            fallback_dir: fallback_dir.into(),
            features: Features::V1_ONLY,
        }
    }
}

/// What one job writes. Every path here was unique in the plan's snapshot
/// and is reserved for this job.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum JobOutput {
    /// One file at this path.
    File(PathBuf),
    /// One new folder at this path (splitVideo, splitPDF, extractArchive).
    Folder(PathBuf),
    /// A document → image export: with one page, write `file`; with more,
    /// create `folder` and write `page_file_name(page_stem, n, count, ext)`
    /// inside it. Both paths are reserved.
    PagesFileOrFolder {
        /// `{stem}.{ext}`: used when there is one page.
        file: PathBuf,
        /// `{stem}` (collision-safe): used when there are several pages.
        folder: PathBuf,
        /// The source's stem, for the page file names.
        page_stem: String,
    },
}

impl JobOutput {
    /// The path to show the user ("Making {name}…"): the file, or the folder.
    pub fn display_path(&self) -> &Path {
        match self {
            JobOutput::File(path) | JobOutput::Folder(path) => path,
            JobOutput::PagesFileOrFolder { file, .. } => file,
        }
    }
}

/// One unit of work for `grape-engines`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Job {
    /// Source paths: one for per-file operations, all of them (in selection
    /// order) for combine tools.
    pub inputs: Vec<PathBuf>,
    /// Where the result goes.
    pub output: JobOutput,
    /// What to do.
    pub op: Operation,
    /// True when the output went to the fallback folder because the
    /// destination wasn't writable (the app shows a toast).
    pub used_fallback: bool,
}

/// The jobs for one drop, in selection order.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobPlan {
    /// The jobs.
    pub jobs: Vec<Job>,
}

/// Why a plan could not be made.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    /// Nothing is selected.
    #[error("nothing is selected")]
    EmptySelection,
    /// The operation is not on the wheel for this selection and features.
    #[error("this operation isn't offered for the selection")]
    NotOffered,
    /// A source path has no file name.
    #[error("source path has no file name: {}", .0.display())]
    NoFileName(PathBuf),
    /// A source path has no parent folder.
    #[error("source path has no parent folder: {}", .0.display())]
    NoParentDir(PathBuf),
    /// A directory the plan needs is missing from `Selection::dirs`.
    #[error("no snapshot of folder: {}", .0.display())]
    MissingDirSnapshot(PathBuf),
    /// Neither the destination nor the fallback folder is writable.
    #[error("neither the destination nor the fallback folder is writable")]
    NoWritableDestination,
}

/// Plan `op` for `sel` under `s`.
///
/// 1. `op` must be on the wheel for this selection ([`wheel_content`]),
///    otherwise [`PlanError::NotOffered`].
/// 2. Per-file operations give one job per item, in selection order.
///    Combine tools give one job with every input, named after the first
///    item and placed in its destination.
/// 3. The destination is the source's folder (or `OutputLocation::Folder`).
///    If that isn't writable, the fallback folder, with `used_fallback`. If
///    neither is, [`PlanError::NoWritableDestination`].
/// 4. Names never collide (case-insensitively) with the snapshot, with the
///    selected sources, or with names planned earlier in the batch.
pub fn plan(sel: &Selection, op: &Operation, s: &Settings) -> Result<JobPlan, PlanError> {
    let first = sel.items.first().ok_or(PlanError::EmptySelection)?;
    if !is_offered(&sel.items, op, s.features) {
        return Err(PlanError::NotOffered);
    }
    let mut planner = Planner {
        sel,
        settings: s,
        taken: BTreeMap::new(),
    };
    let jobs = match op {
        Operation::Tool { tool, .. } if tool.is_combine() => {
            let name = file_name(&first.path)?;
            let (dir, used_fallback) = planner.destination(&first.path)?;
            let out = planner.reserve(&dir, |taken| {
                output_name_folded(&name, first.format(), op, taken)
            });
            vec![Job {
                inputs: sel.items.iter().map(|item| item.path.clone()).collect(),
                output: JobOutput::File(dir.join(out)),
                op: *op,
                used_fallback,
            }]
        }
        _ => sel
            .items
            .iter()
            .map(|item| planner.per_file(item, op))
            .collect::<Result<Vec<_>, _>>()?,
    };
    Ok(JobPlan { jobs })
}

fn is_offered(items: &[SelectionItem], op: &Operation, features: Features) -> bool {
    let (mode, entry) = match *op {
        Operation::Convert { to } => (WheelMode::Conversions, WheelEntry::Convert(to)),
        Operation::Tool { tool, .. } => (WheelMode::Tools, WheelEntry::Tool(tool)),
    };
    wheel_content(items, mode, features).contains(entry)
}

/// The file name of `path` as a string (lossy for non-Unicode names).
fn file_name(path: &Path) -> Result<String, PlanError> {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .ok_or_else(|| PlanError::NoFileName(path.to_path_buf()))
}

fn parent_dir(path: &Path) -> Option<&Path> {
    path.parent().filter(|p| !p.as_os_str().is_empty())
}

struct Planner<'a> {
    sel: &'a Selection,
    settings: &'a Settings,
    /// Folded names per destination: the snapshot, the selected sources in
    /// that folder, and everything planned so far.
    taken: BTreeMap<PathBuf, HashSet<String>>,
}

impl Planner<'_> {
    /// The output folder for a source, and whether it is the fallback.
    fn destination(&self, source: &Path) -> Result<(PathBuf, bool), PlanError> {
        let parent =
            parent_dir(source).ok_or_else(|| PlanError::NoParentDir(source.to_path_buf()))?;
        let preferred = match &self.settings.output {
            OutputLocation::NextToSource => parent.to_path_buf(),
            OutputLocation::Folder(dir) => dir.clone(),
        };
        if self.snapshot_writable(&preferred)? {
            return Ok((preferred, false));
        }
        let fallback = &self.settings.fallback_dir;
        if self.snapshot_writable(fallback)? {
            Ok((fallback.clone(), true))
        } else {
            Err(PlanError::NoWritableDestination)
        }
    }

    fn snapshot_writable(&self, dir: &Path) -> Result<bool, PlanError> {
        self.sel
            .dirs
            .get(dir)
            .map(|snapshot| snapshot.writable)
            .ok_or_else(|| PlanError::MissingDirSnapshot(dir.to_path_buf()))
    }

    /// The working taken set for `dir`, created on first use.
    fn taken_mut(&mut self, dir: &Path) -> &mut HashSet<String> {
        let sel = self.sel;
        self.taken.entry(dir.to_path_buf()).or_insert_with(|| {
            let mut set: HashSet<String> = sel
                .dirs
                .get(dir)
                .map(|snapshot| snapshot.names.iter().map(|n| fold(n)).collect())
                .unwrap_or_default();
            // Sources are never overwritten, even if the snapshot missed them.
            for item in &sel.items {
                if parent_dir(&item.path) == Some(dir) {
                    if let Some(name) = item.path.file_name() {
                        set.insert(fold(&name.to_string_lossy()));
                    }
                }
            }
            set
        })
    }

    /// Picks a name with `name_for` against `dir`'s taken set and reserves it.
    fn reserve(&mut self, dir: &Path, name_for: impl FnOnce(&HashSet<String>) -> String) -> String {
        let taken = self.taken_mut(dir);
        let name = name_for(taken);
        taken.insert(fold(&name));
        name
    }

    fn per_file(&mut self, item: &SelectionItem, op: &Operation) -> Result<Job, PlanError> {
        let name = file_name(&item.path)?;
        let (dir, used_fallback) = self.destination(&item.path)?;
        let pages = match (*op, item.format()) {
            (Operation::Convert { to }, Some(from)) => produces_pages(from, to),
            _ => false,
        };
        let file = self.reserve(&dir, |taken| {
            output_name_folded(&name, item.format(), op, taken)
        });
        let output = if pages {
            let page_stem = stem(&name).to_string();
            let folder = self.reserve(&dir, |taken| unique_name(&page_stem, None, taken));
            JobOutput::PagesFileOrFolder {
                file: dir.join(file),
                folder: dir.join(folder),
                page_stem,
            }
        } else {
            match output_kind(op) {
                OutputKind::File => JobOutput::File(dir.join(file)),
                OutputKind::Folder => JobOutput::Folder(dir.join(file)),
            }
        };
        Ok(Job {
            inputs: vec![item.path.clone()],
            output,
            op: *op,
            used_fallback,
        })
    }
}
