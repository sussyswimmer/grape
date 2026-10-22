//! `build_selection`: items, directory snapshots and writability probes.
#![allow(clippy::unwrap_used)]

mod common;

use std::fs;
use std::path::PathBuf;

use common::{assert_no_leftovers, TestDir};
use grape_core::{
    plan, Format, ItemKind, JobOutput, Operation, OutputLocation, SelectionItem, Settings,
};
use grape_engines::build_selection;

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
    // The probe cleaned up after itself.
    assert_no_leftovers(&photos);
    assert_no_leftovers(&fallback);

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
fn a_missing_fallback_folder_is_created_and_probed() {
    let dir = TestDir::new("fallback");
    let src = dir.join("photo.png");
    fs::write(&src, b"x").unwrap();
    let fallback = dir.join("Downloads").join("Grape");
    assert!(!fallback.exists());

    let sel = build_selection(&[src], &Settings::new(&fallback));

    assert!(fallback.is_dir());
    let snapshot = &sel.dirs[&fallback];
    assert!(snapshot.writable);
    assert!(snapshot.names.is_empty());
    assert_no_leftovers(&fallback);
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

    let sel = build_selection(&[src], &Settings::new(&blocker));

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
fn an_empty_drop_still_snapshots_the_fallback() {
    let dir = TestDir::new("empty");
    let fallback = dir.join("fallback");
    let sel = build_selection(&[], &Settings::new(&fallback));
    assert!(sel.items.is_empty());
    assert_eq!(sel.dirs.len(), 1);
    assert!(sel.dirs[&fallback].writable);
}
