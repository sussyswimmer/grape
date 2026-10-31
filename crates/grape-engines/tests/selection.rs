//! `build_selection`: items, directory snapshots and writability probes.
#![allow(clippy::unwrap_used)]

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::{assert_no_leftovers, TestDir};
use grape_core::{
    plan, DirSnapshot, Format, ItemKind, JobOutput, Operation, OutputLocation, PlanError,
    SelectionItem, Settings, ToolKey,
};
use grape_engines::build_selection;

/// Settings whose output folder is missing, so every plan falls back and
/// `build_selection` has to read the fallback.
fn falling_back(dir: &TestDir, fallback: impl Into<PathBuf>) -> Settings {
    Settings {
        output: OutputLocation::Folder(dir.join("Missing output")),
        ..Settings::new(fallback)
    }
}

#[test]
fn files_and_folders_are_classified() {
    let dir = TestDir::new("items");
    let folder = dir.join("Holiday");
    fs::create_dir(&folder).unwrap();
    let png = dir.join("photo.PNG");
    fs::write(&png, b"not really a png").unwrap();
    let unknown = dir.join("notes.xyz");
    fs::write(&unknown, b"?").unwrap();
    let no_ext = dir.join("README");
    fs::write(&no_ext, b"?").unwrap();
    let missing = dir.join("gone.jpg");

    let paths = vec![
        folder.clone(),
        png.clone(),
        unknown.clone(),
        no_ext.clone(),
        missing.clone(),
    ];
    let sel = build_selection(&paths, &Settings::new(dir.join("fallback")));

    assert_eq!(
        sel.items,
        [
            SelectionItem::folder(&folder),
            SelectionItem::file_with_format(&png, Some(Format::Png)),
            SelectionItem::file_with_format(&unknown, None),
            SelectionItem::file_with_format(&no_ext, None),
            // Can't be read, so kept as a file: the job will say so.
            SelectionItem::file_with_format(&missing, Some(Format::Jpg)),
        ]
    );
    assert_eq!(sel.items[0].kind, ItemKind::Folder);
}

#[test]
fn the_source_folder_snapshot_lists_names_and_is_writable() {
    let dir = TestDir::new("parent");
    let photos = dir.join("photos");
    fs::create_dir(&photos).unwrap();
    for name in ["a.png", "a.jpg", "b.webp"] {
        fs::write(photos.join(name), b"x").unwrap();
    }
    fs::create_dir(photos.join("sub")).unwrap();
    let fallback = dir.join("fallback");

    let sel = build_selection(&[photos.join("a.png")], &Settings::new(&fallback));

    let keys: Vec<&PathBuf> = sel.dirs.keys().collect();
    assert_eq!(keys, [&fallback, &photos]);
    let snapshot = &sel.dirs[&photos];
    assert!(snapshot.writable);
    let mut names: Vec<&str> = snapshot.names.iter().map(String::as_str).collect();
    names.sort_unstable();
    assert_eq!(names, ["a.jpg", "a.png", "b.webp", "sub"]);
    // The probe cleaned up after itself, and the fallback wasn't created.
    assert_no_leftovers(&photos);
    assert!(!fallback.exists());

    // The snapshot keys are exactly what the planner looks up.
    let job = plan(
        &sel,
        &Operation::convert(Format::Jpg),
        &Settings::new(&fallback),
    )
    .unwrap()
    .jobs
    .remove(0);
    assert_eq!(job.output, JobOutput::File(photos.join("a 2.jpg")));
    assert!(!job.used_fallback);
}

#[test]
fn a_missing_fallback_folder_is_probed_but_not_created() {
    let dir = TestDir::new("fallback");
    let src = dir.join("photo.png");
    fs::write(&src, b"x").unwrap();
    let downloads = dir.join("Downloads");
    fs::create_dir(&downloads).unwrap();
    let fallback = downloads.join("Grape");
    let settings = falling_back(&dir, &fallback);

    let sel = build_selection(&[src], &settings);

    // Only a job that really falls back creates it.
    assert!(!fallback.exists());
    let snapshot = &sel.dirs[&fallback];
    assert!(snapshot.writable);
    assert!(snapshot.names.is_empty());
    // The probe ran in the nearest existing ancestor and cleaned up.
    assert_no_leftovers(&downloads);
    let job = plan(&sel, &Operation::convert(Format::Jpg), &settings)
        .unwrap()
        .jobs
        .remove(0);
    assert!(job.used_fallback);
    assert_eq!(job.output, JobOutput::File(fallback.join("photo.jpg")));
}

#[test]
fn a_missing_fallback_under_a_missing_parent_probes_the_nearest_folder() {
    let dir = TestDir::new("fallback-deep");
    let src = dir.join("photo.png");
    fs::write(&src, b"x").unwrap();
    let fallback = dir.join("Downloads").join("Grape");

    let sel = build_selection(&[src], &falling_back(&dir, &fallback));

    assert!(!dir.join("Downloads").exists());
    assert!(sel.dirs[&fallback].writable);
}

/// Makes `link` a link to `target`, which is then gone: a symlink on Unix,
/// a junction on Windows (which needs no privilege).
fn dangling_link(link: &Path, target: &Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
    #[cfg(windows)]
    {
        fs::create_dir(target).unwrap();
        let made = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .unwrap();
        assert!(made.status.success(), "mklink /J failed: {made:?}");
        fs::remove_dir(target).unwrap();
    }
    assert!(fs::symlink_metadata(link).is_ok());
    assert!(!link.exists(), "the link should lead nowhere");
}

#[test]
fn a_link_to_nowhere_on_the_fallback_path_is_not_writable() {
    // `exists` calls a dangling link missing, but nothing can be created
    // in its place, so the planner must not count on the fallback.
    let dir = TestDir::new("fallback-link");
    let src = dir.join("photo.png");
    fs::write(&src, b"x").unwrap();
    let downloads = dir.join("Downloads");
    fs::create_dir(&downloads).unwrap();
    let fallback = downloads.join("Grape");
    dangling_link(&fallback, &dir.join("gone"));
    let settings = falling_back(&dir, &fallback);

    let sel = build_selection(std::slice::from_ref(&src), &settings);
    assert!(!sel.dirs[&fallback].writable);
    assert_eq!(
        plan(&sel, &Operation::convert(Format::Jpg), &settings),
        Err(PlanError::NoWritableDestination)
    );

    // The same for a link to nowhere in place of a parent folder.
    let deeper = falling_back(&dir, fallback.join("More"));
    let sel = build_selection(std::slice::from_ref(&src), &deeper);
    assert!(!sel.dirs[&deeper.fallback_dir].writable);
    assert_no_leftovers(&downloads);
}

#[test]
fn the_output_folder_setting_gets_a_snapshot_but_is_never_created() {
    let dir = TestDir::new("output-folder");
    let src = dir.join("photo.png");
    fs::write(&src, b"x").unwrap();
    let missing_out = dir.join("Converted");
    let settings = Settings {
        output: OutputLocation::Folder(missing_out.clone()),
        ..Settings::new(dir.join("fallback"))
    };

    let sel = build_selection(std::slice::from_ref(&src), &settings);

    assert!(!missing_out.exists());
    let snapshot = &sel.dirs[&missing_out];
    assert!(!snapshot.writable);
    assert!(snapshot.names.is_empty());

    // With the folder present, it is listed and writable.
    fs::create_dir(&missing_out).unwrap();
    fs::write(missing_out.join("photo.jpg"), b"x").unwrap();
    let sel = build_selection(&[src], &settings);
    let snapshot = &sel.dirs[&missing_out];
    assert!(snapshot.writable);
    assert!(snapshot.names.contains("photo.jpg"));
    assert_no_leftovers(&missing_out);
}

#[test]
fn a_file_where_a_folder_should_be_is_not_writable() {
    let dir = TestDir::new("not-a-dir");
    let src = dir.join("photo.png");
    fs::write(&src, b"x").unwrap();
    // The fallback path is taken by a file, so it can't be created.
    let blocker = dir.join("Grape");
    fs::write(&blocker, b"x").unwrap();

    let sel = build_selection(&[src], &falling_back(&dir, &blocker));

    let snapshot = &sel.dirs[&blocker];
    assert!(!snapshot.writable);
    assert!(snapshot.names.is_empty());
    assert_eq!(fs::read(&blocker).unwrap(), b"x");
}

#[cfg(unix)]
#[test]
fn a_read_only_folder_is_not_writable() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TestDir::new("read-only");
    let locked = dir.join("locked");
    fs::create_dir(&locked).unwrap();
    let src = locked.join("photo.png");
    fs::write(&src, b"x").unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();
    // Restore write access before `TestDir` deletes the tree.
    struct Unlock(PathBuf);
    impl Drop for Unlock {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
        }
    }
    let _unlock = Unlock(locked.clone());

    // Root (and CAP_DAC_OVERRIDE) ignores permission bits: check first.
    let canary = locked.join("canary");
    if fs::write(&canary, b"x").is_ok() {
        let _ = fs::remove_file(&canary);
        println!("note: skipped, this user can write into a 0o555 folder (running as root?)");
        return;
    }

    let fallback = dir.join("fallback");
    let sel = build_selection(std::slice::from_ref(&src), &Settings::new(&fallback));

    let snapshot = &sel.dirs[&locked];
    assert!(!snapshot.writable);
    assert!(snapshot.names.contains("photo.png"));
    assert!(sel.dirs[&fallback].writable);
    // The plan falls back to Grape's folder.
    let job = plan(
        &sel,
        &Operation::convert(Format::Jpg),
        &Settings::new(&fallback),
    )
    .unwrap()
    .jobs
    .remove(0);
    assert!(job.used_fallback);
    assert_eq!(job.output, JobOutput::File(fallback.join("photo.jpg")));
}

#[test]
fn a_unicode_file_name_is_kept_exactly() {
    let dir = TestDir::new("unicode");
    let name = "ảnh chụp.png";
    let src = dir.join(name);
    fs::write(&src, b"x").unwrap();

    let sel = build_selection(
        std::slice::from_ref(&src),
        &Settings::new(dir.join("fallback")),
    );

    assert_eq!(
        sel.items,
        [SelectionItem::file_with_format(&src, Some(Format::Png))]
    );
    assert!(sel.dirs[&dir.path].names.contains(name));
}

#[test]
fn without_a_fallback_folder_the_plan_has_nowhere_to_write() {
    let dir = TestDir::new("no-fallback");
    // The source's folder is missing, so it isn't writable either.
    let src = dir.join("gone").join("photo.png");
    let settings = Settings::new("");

    let sel = build_selection(std::slice::from_ref(&src), &settings);

    assert!(!sel.dirs[&dir.join("gone")].writable);
    assert!(!sel.dirs[&PathBuf::new()].writable);
    assert_eq!(
        plan(&sel, &Operation::convert(Format::Jpg), &settings),
        Err(PlanError::NoWritableDestination)
    );
}

#[test]
fn an_empty_drop_still_snapshots_the_fallback() {
    let dir = TestDir::new("empty");
    let fallback = dir.join("fallback");
    // A missing output folder: plans would fall back, so it's read.
    let sel = build_selection(&[], &falling_back(&dir, &fallback));
    assert!(sel.items.is_empty());
    assert_eq!(sel.dirs.len(), 2);
    assert!(sel.dirs[&fallback].writable);

    // Next to the source, nothing can fall back: it's there, unread.
    let sel = build_selection(&[], &Settings::new(&fallback));
    assert_eq!(sel.dirs.len(), 1);
    assert_eq!(sel.dirs[&fallback], DirSnapshot::default());
}

#[test]
fn a_drop_that_cant_fall_back_leaves_the_fallback_unread() {
    // On macOS, reading `~/Downloads` asks the user for the Downloads
    // folder, so a plan that can't reach the fallback mustn't read it.
    let dir = TestDir::new("fallback-unread");
    let photos = dir.join("photos");
    fs::create_dir(&photos).unwrap();
    let src = photos.join("photo.png");
    fs::write(&src, b"x").unwrap();
    let downloads = dir.join("Downloads");
    fs::create_dir(&downloads).unwrap();
    let fallback = downloads.join("Grape");
    fs::create_dir(&fallback).unwrap();
    fs::write(fallback.join("photo.jpg"), b"x").unwrap();
    let settings = Settings::new(&fallback);
    let jpg = Operation::convert(Format::Jpg);

    let sel = build_selection(std::slice::from_ref(&src), &settings);

    // A read would have listed `photo.jpg` and found the folder writable.
    assert_eq!(sel.dirs[&fallback], DirSnapshot::default());
    let job = plan(&sel, &jpg, &settings).unwrap().jobs.remove(0);
    assert!(!job.used_fallback);
    assert_eq!(job.output, JobOutput::File(photos.join("photo.jpg")));

    // A missing fallback's parent isn't probed either: it is writable.
    let missing = Settings::new(downloads.join("Missing"));
    let sel = build_selection(std::slice::from_ref(&src), &missing);
    assert_eq!(sel.dirs[&missing.fallback_dir], DirSnapshot::default());

    // A writable output folder is tried first, so a source folder that
    // isn't writable doesn't matter: it isn't even read.
    let out = dir.join("Converted");
    fs::create_dir(&out).unwrap();
    let to_out = Settings {
        output: OutputLocation::Folder(out.clone()),
        ..settings.clone()
    };
    let gone = dir.join("gone").join("photo.png");
    let sel = build_selection(std::slice::from_ref(&gone), &to_out);
    assert!(!sel.dirs.contains_key(&dir.join("gone")));
    assert_eq!(sel.dirs[&fallback], DirSnapshot::default());
    let job = plan(&sel, &jpg, &to_out).unwrap().jobs.remove(0);
    assert!(!job.used_fallback);
    assert_eq!(job.output, JobOutput::File(out.join("photo.jpg")));
}

/// Sets `dirs`' mtimes back to a fixed time, so creating or deleting
/// anything in them (as a probe does) shows. Unix only: there a folder
/// opened read-only takes a new mtime.
#[cfg(unix)]
fn set_back(dirs: &[&Path]) -> std::time::SystemTime {
    let then = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
    for dir in dirs {
        fs::File::open(dir).unwrap().set_modified(then).unwrap();
    }
    then
}

#[cfg(unix)]
fn modified(dir: &Path) -> std::time::SystemTime {
    fs::metadata(dir).unwrap().modified().unwrap()
}

#[cfg(unix)]
#[test]
fn a_drop_that_cant_fall_back_probes_nothing_near_the_fallback() {
    let dir = TestDir::new("fallback-unprobed");
    let src = dir.join("photo.png");
    fs::write(&src, b"x").unwrap();
    let downloads = dir.join("Downloads");
    fs::create_dir(&downloads).unwrap();
    let fallback = downloads.join("Grape");
    fs::create_dir(&fallback).unwrap();
    let missing = downloads.join("Missing");

    // Present: the fallback itself would be probed; missing: its parent.
    let then = set_back(&[&fallback, &downloads]);
    build_selection(std::slice::from_ref(&src), &Settings::new(&fallback));
    build_selection(std::slice::from_ref(&src), &Settings::new(&missing));
    assert_eq!(modified(&fallback), then);
    assert_eq!(modified(&downloads), then);

    // The watch works: a plan that may fall back probes both.
    build_selection(std::slice::from_ref(&src), &falling_back(&dir, &fallback));
    build_selection(std::slice::from_ref(&src), &falling_back(&dir, &missing));
    assert_ne!(modified(&fallback), then);
    assert_ne!(modified(&downloads), then);
    assert_no_leftovers(&fallback);
    assert_no_leftovers(&downloads);
}

#[test]
fn a_drop_from_or_into_the_fallback_reads_it() {
    let dir = TestDir::new("fallback-used");
    let fallback = dir.join("Grape");
    fs::create_dir(&fallback).unwrap();
    let inside = fallback.join("photo.png");
    fs::write(&inside, b"x").unwrap();
    fs::write(fallback.join("photo.jpg"), b"x").unwrap();
    let jpg = Operation::convert(Format::Jpg);

    // A source in the fallback: the plan writes next to it.
    let settings = Settings::new(&fallback);
    let sel = build_selection(std::slice::from_ref(&inside), &settings);
    let snapshot = &sel.dirs[&fallback];
    assert!(snapshot.writable);
    assert!(snapshot.names.contains("photo.jpg"));
    let job = plan(&sel, &jpg, &settings).unwrap().jobs.remove(0);
    assert!(!job.used_fallback);
    assert_eq!(job.output, JobOutput::File(fallback.join("photo 2.jpg")));

    // The output folder setting names the fallback.
    let photos = dir.join("photos");
    fs::create_dir(&photos).unwrap();
    let src = photos.join("photo.png");
    fs::write(&src, b"x").unwrap();
    let into = Settings {
        output: OutputLocation::Folder(fallback.clone()),
        ..settings
    };
    let sel = build_selection(std::slice::from_ref(&src), &into);
    assert!(sel.dirs[&fallback].names.contains("photo.jpg"));
    let job = plan(&sel, &jpg, &into).unwrap().jobs.remove(0);
    assert!(!job.used_fallback);
    assert_eq!(job.output, JobOutput::File(fallback.join("photo 2.jpg")));
    assert_no_leftovers(&fallback);
}

/// The fallback's snapshot as `build_selection` took it on every drop
/// before it became lazy, written out here so the old behaviour stays
/// reachable: none for an empty path; a present path listed and probed;
/// a missing one writable when its nearest existing ancestor is a folder
/// that passes the probe.
fn eager_fallback(fallback: &Path) -> DirSnapshot {
    if fallback.as_os_str().is_empty() {
        return DirSnapshot::default();
    }
    let present = |path: &Path| fs::symlink_metadata(path).is_ok();
    if present(fallback) {
        let Ok(entries) = fs::read_dir(fallback) else {
            return DirSnapshot::default();
        };
        let names: Vec<String> = entries
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        return DirSnapshot::new(names, probe(fallback));
    }
    let ancestor = fallback.ancestors().skip(1).find(|dir| present(dir));
    let writable = ancestor.is_some_and(|dir| dir.is_dir() && probe(dir));
    DirSnapshot::new(Vec::<String>::new(), writable)
}

/// A source folder's snapshot as `build_selection` reads one: its names
/// and the probe, or nothing when it can't be listed.
fn eager_dir(dir: &Path) -> DirSnapshot {
    let Ok(entries) = fs::read_dir(dir) else {
        return DirSnapshot::default();
    };
    let names: Vec<String> = entries
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    DirSnapshot::new(names, probe(dir))
}

/// Creates a file in `dir` and deletes it again, as the real probe does.
fn probe(dir: &Path) -> bool {
    let path = dir.join(".grape-probe-by-hand");
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(file) => {
            drop(file);
            fs::remove_file(&path).is_ok()
        }
        Err(_) => false,
    }
}

#[test]
fn the_lazy_fallback_plans_exactly_like_the_eager_one() {
    let dir = TestDir::new("fallback-same-plans");
    let file = |path: PathBuf| {
        fs::write(&path, b"x").unwrap();
        path
    };
    let folder = |path: PathBuf| {
        fs::create_dir_all(&path).unwrap();
        path
    };
    let photos = folder(dir.join("photos"));
    let a = file(photos.join("a.png"));
    let b = file(photos.join("b.png"));
    file(photos.join("a.jpg"));
    let more = folder(dir.join("more"));
    let c = file(more.join("c.png"));
    let pdf = file(folder(dir.join("docs")).join("doc.pdf"));
    // Source folders that aren't writable: a missing one, and a file.
    let gone = dir.join("gone").join("d.png");
    let blocker = file(dir.join("blocked"));
    let blocked = blocker.join("e.png");
    let downloads = folder(dir.join("Downloads"));
    let grape = folder(downloads.join("Grape"));
    // Names a plan into the fallback must step around.
    for name in ["a.jpg", "a 2.jpg", "d.jpg", "e.jpg", "inside.png", "doc"] {
        file(grape.join(name));
    }
    let converted = folder(dir.join("Converted"));
    file(converted.join("a.jpg"));

    let fallbacks = [
        grape.clone(),
        downloads.join("Missing"),
        dir.join("Nowhere").join("Grape"),
        blocker.clone(),
        // A source folder as the fallback.
        photos.clone(),
        PathBuf::new(),
    ];
    let ops = [
        Operation::convert(Format::Jpg),
        Operation::tool(ToolKey::CreatePdf),
        Operation::convert(Format::Png),
    ];
    let (mut plans, mut read, mut unread, mut fell_back, mut sources_unread) = (0, 0, 0, 0, 0);
    for fallback in &fallbacks {
        let inside = fallback.join("inside.png");
        let drops = [
            vec![],
            vec![a.clone()],
            vec![a.clone(), b.clone(), c.clone()],
            vec![c.clone(), gone.clone(), a.clone()],
            vec![blocked.clone()],
            // No parent folder.
            vec![PathBuf::from("f.png")],
            vec![inside.clone()],
            vec![a.clone(), inside.clone()],
            vec![pdf.clone()],
        ];
        let outputs = [
            OutputLocation::NextToSource,
            OutputLocation::Folder(converted.clone()),
            OutputLocation::Folder(dir.join("Missing output")),
            OutputLocation::Folder(fallback.clone()),
        ];
        for output in outputs {
            let settings = Settings {
                output,
                ..Settings::new(fallback)
            };
            for paths in &drops {
                let lazy = build_selection(paths, &settings);
                let eager_snapshot = eager_fallback(fallback);
                // Where the lazy one read it, it read the same.
                let lazy_snapshot = &lazy.dirs[fallback];
                if *lazy_snapshot != DirSnapshot::default() {
                    assert_eq!(*lazy_snapshot, eager_snapshot, "{paths:?}, {settings:?}");
                    read += 1;
                } else if eager_snapshot != DirSnapshot::default() {
                    unread += 1;
                }
                let mut eager = lazy.clone().with_dir(fallback.clone(), eager_snapshot);
                // The eager code also read every source's folder, whatever
                // the output.
                let parents = paths
                    .iter()
                    .filter_map(|path| path.parent())
                    .filter(|parent| !parent.as_os_str().is_empty() && *parent != fallback);
                for parent in parents {
                    if !eager.dirs.contains_key(parent) {
                        eager = eager.with_dir(parent, eager_dir(parent));
                        sources_unread += 1;
                    }
                }
                for op in &ops {
                    let planned = plan(&lazy, op, &settings);
                    assert_eq!(
                        planned,
                        plan(&eager, op, &settings),
                        "{op:?} for {paths:?}, {settings:?}"
                    );
                    plans += 1;
                    if let Ok(planned) = planned {
                        fell_back += planned.jobs.iter().filter(|j| j.used_fallback).count();
                    }
                }
            }
        }
    }
    // Both sides were reached: read and left unread, plans that fell back,
    // and sources' folders a chosen output folder left unread.
    assert_eq!(plans, 6 * 4 * 9 * 3);
    assert!(
        read > 0 && unread > 0 && fell_back > 0 && sources_unread > 0,
        "{read} {unread} {fell_back} {sources_unread}"
    );
    for folder in [&dir.path, &photos, &more, &downloads, &grape, &converted] {
        assert_no_leftovers(folder);
    }
}
