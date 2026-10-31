//! Remove Metadata for PDF, with lopdf: the document-level metadata goes
//! (ENGINES: "clear the Info dictionary and XMP Metadata stream").
//!
//! [`strip`] removes the trailer's `/Info` and the catalog's `/Metadata`,
//! then keeps only the objects still reachable from the trailer, so the
//! old Info and XMP objects (and anything only an older revision used) are
//! not written again. The caller saves the result as a full rewrite, never
//! an incremental update, which would keep the old bytes in the file.
//! Page-level XMP, image metadata, annotation authors and form values stay
//! (they aren't document level).

use std::collections::HashSet;

use lopdf::xref::XrefEntry;
use lopdf::{Document, Object, ObjectId};

use super::protocol::MetaKey;

/// Removes the Info dictionary and the XMP stream from `doc` and drops
/// every object nothing reachable uses any more. Returns what was there
/// with a value, sorted.
pub(crate) fn strip(doc: &mut Document) -> Vec<MetaKey> {
    let mut removed = Vec::new();
    if let Ok(info) = doc.trailer.get(b"Info") {
        if let Ok((_, Object::Dictionary(info))) = doc.dereference(info) {
            for (key, value) in info.iter() {
                let value = doc.dereference(value).map(|(_, v)| v).unwrap_or(value);
                if has_value(value) {
                    removed.push(meta_key(key));
                }
            }
        }
    }
    doc.trailer.remove(b"Info");
    if let Ok(catalog) = doc.catalog_mut() {
        if let Some(xmp) = catalog.remove(b"Metadata") {
            if !matches!(xmp, Object::Null) {
                removed.push(MetaKey::Xmp);
            }
        }
    }
    let (keep, _) = reachable(doc);
    doc.objects.retain(|id, _| keep.contains(id));
    removed.sort();
    removed.dedup();
    removed
}

/// The key an Info entry is reported as.
fn meta_key(key: &[u8]) -> MetaKey {
    match key {
        b"Title" => MetaKey::Title,
        b"Author" => MetaKey::Author,
        b"Subject" => MetaKey::Subject,
        b"Keywords" => MetaKey::Keywords,
        b"Creator" => MetaKey::Creator,
        b"Producer" => MetaKey::Producer,
        b"CreationDate" => MetaKey::CreationDate,
        b"ModDate" => MetaKey::ModDate,
        _ => MetaKey::Other,
    }
}

/// False for null and for an empty string (a UTF-16 byte-order mark alone
/// counts as empty).
fn has_value(value: &Object) -> bool {
    match value {
        Object::Null => false,
        Object::String(bytes, _) => {
            let text = bytes.strip_prefix(&[0xFE, 0xFF]).unwrap_or(bytes);
            !text.is_empty()
        }
        _ => true,
    }
}

/// The objects a document uses (reachable from its trailer) that lopdf
/// didn't load.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Missing {
    /// Listed as in use by the cross-reference table lopdf read: lopdf
    /// couldn't read them, and a rewrite would lose them.
    pub(crate) listed: usize,
    /// Listed as free, or not at all. That is just null (PDF 32000-1
    /// §7.3.10), unless the file is a hybrid-reference one whose extra
    /// cross-reference stream lopdf skipped.
    pub(crate) unlisted: usize,
}

/// What [`Missing`] counts for `doc`. Any `listed` object means the file
/// is damaged for lopdf (left to PDFium, for split and merge).
pub(crate) fn missing_objects(doc: &Document) -> Missing {
    reachable(doc).1
}

/// Every object reachable from the trailer (without `/Info`), walked with a
/// worklist and a seen-set, and the references among them to objects that
/// aren't loaded.
fn reachable(doc: &Document) -> (HashSet<ObjectId>, Missing) {
    let mut missing = Missing::default();
    let mut seen = HashSet::new();
    let mut stack: Vec<&Object> = doc
        .trailer
        .iter()
        .filter(|(key, _)| !matches!(key.as_slice(), b"Info" | b"Prev" | b"XRefStm"))
        .map(|(_, value)| value)
        .collect();
    while let Some(object) = stack.pop() {
        match object {
            Object::Reference(id) => {
                if seen.insert(*id) {
                    match doc.objects.get(id) {
                        Some(target) => stack.push(target),
                        None => {
                            let listed = match doc.reference_table.get(id.0) {
                                Some(XrefEntry::Normal { generation, .. }) => *generation == id.1,
                                Some(XrefEntry::Compressed { .. }) => id.1 == 0,
                                _ => false,
                            };
                            if listed {
                                missing.listed += 1;
                            } else {
                                missing.unlisted += 1;
                            }
                        }
                    }
                }
            }
            Object::Array(items) => stack.extend(items.iter()),
            Object::Dictionary(dict) => stack.extend(dict.iter().map(|(_, value)| value)),
            Object::Stream(stream) => stack.extend(stream.dict.iter().map(|(_, value)| value)),
            _ => {}
        }
    }
    (seen, missing)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use lopdf::{dictionary, Stream};

    use super::super::copy::tests::{contains, fixture, save_classic, save_modern};
    use super::*;

    fn stripped(bytes: &[u8]) -> (Vec<MetaKey>, Vec<u8>) {
        let mut doc = Document::load_mem(bytes).unwrap();
        let removed = strip(&mut doc);
        let mut out = Vec::new();
        doc.save_to(&mut out).unwrap();
        (removed, out)
    }

    #[test]
    fn info_and_xmp_go_and_the_pages_stay() {
        let source = fixture("A");
        for saved in [save_classic(&source), save_modern(&source)] {
            let (removed, out) = stripped(&saved);
            assert_eq!(
                removed,
                [
                    MetaKey::Title,
                    MetaKey::Author,
                    MetaKey::Producer,
                    MetaKey::Other,
                    MetaKey::Xmp
                ]
            );
            for marker in [
                "MARK-TITLE",
                "MARK-AUTHOR",
                "MARK-PRODUCER",
                "MARK-COMPANY",
                "MARK-XMP",
            ] {
                assert!(!contains(&out, marker), "{marker}");
            }
            let doc = Document::load_mem(&out).unwrap();
            assert!(!doc.trailer.has(b"Info"));
            assert!(!doc.catalog().unwrap().has(b"Metadata"));
            assert_eq!(doc.get_pages().len(), 3);
            assert!(doc.extract_text(&[2]).unwrap().contains("A page 2"));
            // Everything else in the catalog is still there.
            assert!(doc.catalog().unwrap().has(b"Outlines"));
            assert!(doc.catalog().unwrap().has(b"AcroForm"));
        }
    }

    #[test]
    fn an_older_revision_is_not_kept() {
        // An incremental update that replaces the Info dictionary: the old
        // revision's Author is still in the bytes until a full rewrite.
        let source = fixture("A");
        let mut bytes = save_classic(&source);
        let doc = Document::load_mem(&bytes).unwrap();
        let info = doc.trailer.get(b"Info").unwrap().as_reference().unwrap();
        let root = doc.trailer.get(b"Root").unwrap().as_reference().unwrap();
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let at = text.rfind("startxref").unwrap();
        let prev: usize = text[at + 9..]
            .trim()
            .trim_end_matches("%%EOF")
            .trim()
            .parse()
            .unwrap();
        bytes.extend_from_slice(b"\n");
        let offset = bytes.len();
        bytes.extend_from_slice(
            format!("{} 0 obj\n<< /Author (MARK-NEWER) >>\nendobj\n", info.0).as_bytes(),
        );
        let xref = bytes.len();
        bytes.extend_from_slice(
            format!(
                "xref\n0 1\n0000000000 65535 f \n{} 1\n{offset:010} 00000 n \ntrailer\n<< /Size {} /Root {} 0 R /Info {} 0 R /Prev {prev} >>\nstartxref\n{xref}\n%%EOF\n",
                info.0,
                doc.max_id + 1,
                root.0,
                info.0
            )
            .as_bytes(),
        );
        assert!(contains(&bytes, "MARK-AUTHOR") && contains(&bytes, "MARK-NEWER"));
        let (removed, out) = stripped(&bytes);
        assert_eq!(removed, [MetaKey::Author, MetaKey::Xmp]);
        assert!(!contains(&out, "MARK-AUTHOR") && !contains(&out, "MARK-NEWER"));
        assert_eq!(Document::load_mem(&out).unwrap().get_pages().len(), 3);
    }

    #[test]
    fn nothing_to_remove_reports_nothing() {
        let mut doc = fixture("A");
        doc.trailer.remove(b"Info");
        doc.catalog_mut().unwrap().remove(b"Metadata");
        let (removed, _) = stripped(&save_classic(&doc));
        assert!(removed.is_empty());

        // Empty values don't count; an indirect value does.
        let mut doc = fixture("A");
        let title = doc.add_object(Object::string_literal("MARK-INDIRECT"));
        let info = doc.add_object(dictionary! {
            "Title" => title,
            "Author" => Object::string_literal(""),
            "Keywords" => Object::String(vec![0xFE, 0xFF], lopdf::StringFormat::Hexadecimal),
            "Trapped" => Object::Null,
        });
        doc.trailer.set("Info", info);
        doc.catalog_mut().unwrap().set("Metadata", Object::Null);
        let (removed, out) = stripped(&save_classic(&doc));
        assert_eq!(removed, [MetaKey::Title]);
        assert!(!contains(&out, "MARK-INDIRECT"));
    }

    #[test]
    fn objects_lopdf_could_not_read_are_counted() {
        let bytes = save_classic(&fixture("A"));
        let mut doc = Document::load_mem(&bytes).unwrap();
        assert_eq!(missing_objects(&doc), Missing::default());
        // A page's content stream is listed but didn't load.
        let page = doc.get_pages()[&1];
        let content = doc
            .get_dictionary(page)
            .unwrap()
            .get(b"Contents")
            .unwrap()
            .as_reference()
            .unwrap();
        doc.objects.remove(&content);
        assert_eq!(
            missing_objects(&doc),
            Missing {
                listed: 1,
                unlisted: 0
            }
        );
        // A reference to an object nobody lists is counted apart: it is
        // only null, unless the file is a hybrid one.
        let mut doc = Document::load_mem(&bytes).unwrap();
        doc.get_dictionary_mut(page)
            .unwrap()
            .set("Thumb", Object::Reference((9999, 0)));
        assert_eq!(
            missing_objects(&doc),
            Missing {
                listed: 0,
                unlisted: 1
            }
        );
    }

    #[test]
    fn unreachable_objects_are_dropped() {
        let mut doc = fixture("A");
        doc.add_object(Stream::new(dictionary! {}, b"MARK-ORPHAN".to_vec()));
        let (_, out) = stripped(&save_classic(&doc));
        assert!(!contains(&out, "MARK-ORPHAN"));
    }
}
