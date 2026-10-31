//! Grape's page copier for Split PDF and Merge PDFs, on lopdf documents.
//!
//! lopdf's own helpers don't fit: `delete_pages` keeps the whole catalog
//! (forms, outlines, named destinations of the deleted pages) and is
//! quadratic, and its merge example merges `Pages` nodes with
//! `Dictionary::extend`, which turns clashing keys into arrays. So a page
//! is copied the way PDFium's page exporter does it:
//!
//! - [`Tree::walk`] walks the page tree once, iteratively, with a seen-set
//!   (a cycle or a page listed twice is visited once), passing `MediaBox`,
//!   `CropBox`, `Resources` and `Rotate` down from the `Pages` nodes.
//! - [`Copier`] writes each page with those keys flattened onto it, drops
//!   `/Parent`, `/B` and `/StructParents`, and copies everything the page
//!   references through an explicit worklist with one id map per source
//!   (never recursion), so a shared font is copied once per output.
//! - A reference to a page that isn't in the output, to a `Pages` node or
//!   to the catalog becomes `null`. Link annotations whose destination is
//!   such a page (or a named destination: the names tree isn't copied) are
//!   dropped. Widgets lose `/Parent`: the AcroForm, the outline and the
//!   structure tree are not copied (D29), so form fields keep their
//!   appearance but aren't fields any more.
//! - Outputs are PDF 1.5 or later (lopdf writes a cross-reference stream).

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use lopdf::{dictionary, Dictionary, Document, Object, ObjectId};

/// The page attributes a page inherits from its `Pages` ancestors.
const INHERITABLE: [&[u8]; 4] = [b"MediaBox", b"CropBox", b"Resources", b"Rotate"];

/// The page tree depth past which nodes are ignored (lopdf's bound).
const MAX_TREE_DEPTH: usize = 256;

/// US Letter, PDFium's fallback for a page with neither box.
const LETTER: [i64; 4] = [0, 0, 612, 792];

/// Why a document's page tree couldn't be walked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TreeError {
    /// No `/Root` catalog, or no `/Pages` in it.
    NoTree,
}

/// One page of a [`Tree`].
#[derive(Debug, Clone)]
pub(crate) struct TreePage {
    /// The page object.
    pub(crate) id: ObjectId,
    /// What it inherits, shared with its siblings.
    inherited: Rc<Inherited>,
}

/// Inherited values, in [`INHERITABLE`] order.
#[derive(Debug, Clone, Default)]
struct Inherited([Option<Object>; 4]);

impl Inherited {
    /// These values, overridden by the ones `node` sets itself.
    fn under(&self, node: &Dictionary) -> Inherited {
        let mut next = self.clone();
        for (slot, key) in next.0.iter_mut().zip(INHERITABLE) {
            if let Ok(value) = node.get(key) {
                *slot = Some(value.clone());
            }
        }
        next
    }
}

/// A document's page tree, walked once.
#[derive(Debug)]
pub(crate) struct Tree {
    /// The pages in reading order.
    pub(crate) pages: Vec<TreePage>,
    /// Every page's id.
    page_ids: HashSet<ObjectId>,
    /// Every `Pages` node and the catalog: never copied.
    skip: HashSet<ObjectId>,
    /// The root node's `/Count`, when it is a number.
    declared: Option<i64>,
}

impl Tree {
    /// Walks `doc`'s page tree. A node is a `/Type /Pages` dictionary, or
    /// one without a type that has `/Kids`; anything else in a `/Kids`
    /// array is a page (as PDFium reads it). Missing kids are skipped.
    pub(crate) fn walk(doc: &Document) -> Result<Tree, TreeError> {
        let catalog_id = doc
            .trailer
            .get(b"Root")
            .and_then(Object::as_reference)
            .map_err(|_| TreeError::NoTree)?;
        let catalog = doc
            .get_dictionary(catalog_id)
            .map_err(|_| TreeError::NoTree)?;
        let root = catalog
            .get(b"Pages")
            .and_then(Object::as_reference)
            .map_err(|_| TreeError::NoTree)?;
        let declared = doc
            .get_dictionary(root)
            .ok()
            .and_then(|node| node.get(b"Count").ok())
            .and_then(|count| doc.dereference(count).ok())
            .and_then(|(_, count)| count.as_i64().ok());

        let mut pages = Vec::new();
        let mut page_ids = HashSet::new();
        let mut skip = HashSet::from([catalog_id]);
        let mut seen = HashSet::new();
        let mut stack = vec![(root, Rc::new(Inherited::default()), 0usize)];
        while let Some((id, inherited, depth)) = stack.pop() {
            if !seen.insert(id) {
                continue;
            }
            let Ok(dict) = doc.get_dictionary(id) else {
                continue;
            };
            let is_node = match dict.get_type() {
                Ok(b"Pages") => true,
                Ok(b"Page") => false,
                _ => dict.has(b"Kids"),
            };
            if !is_node {
                page_ids.insert(id);
                pages.push(TreePage { id, inherited });
                continue;
            }
            skip.insert(id);
            if depth >= MAX_TREE_DEPTH {
                continue;
            }
            let below = Rc::new(inherited.under(dict));
            let kids = dict
                .get(b"Kids")
                .ok()
                .and_then(|kids| doc.dereference(kids).ok())
                .and_then(|(_, kids)| kids.as_array().ok());
            for kid in kids.into_iter().flatten().rev() {
                if let Ok(kid) = kid.as_reference() {
                    stack.push((kid, Rc::clone(&below), depth + 1));
                }
            }
        }
        // A page that is also a node (or the catalog) is only a page.
        skip.retain(|id| !page_ids.contains(id));
        Ok(Tree {
            pages,
            page_ids,
            skip,
            declared,
        })
    }

    /// True when the walk found pages and as many as the root's `/Count`
    /// says. Anything else means objects went missing while loading.
    pub(crate) fn is_whole(&self) -> bool {
        !self.pages.is_empty() && i64::try_from(self.pages.len()).ok() == self.declared
    }
}

/// An object still to copy: from `src` in the source to `dest` in the
/// output, prepared already (an annotation) or taken from the source.
struct Pending {
    src: ObjectId,
    dest: ObjectId,
    prepared: Option<Object>,
}

/// Copies pages of one source document into one output document.
pub(crate) struct Copier<'a> {
    src: &'a Document,
    tree: &'a Tree,
    /// The source pages this output gets, with their ids in it.
    pages: HashMap<ObjectId, ObjectId>,
    /// Every other source object copied so far, with its id in the output.
    copied: HashMap<ObjectId, ObjectId>,
}

impl<'a> Copier<'a> {
    /// A copier from `src`, whose tree is `tree`.
    pub(crate) fn new(src: &'a Document, tree: &'a Tree) -> Copier<'a> {
        Copier {
            src,
            tree,
            pages: HashMap::new(),
            copied: HashMap::new(),
        }
    }

    /// Gives the pages `which` (indexes into the tree) their ids in `out`,
    /// before any is copied, so links between them survive. Returns the
    /// ids in the same order.
    pub(crate) fn reserve(&mut self, out: &mut Document, which: &[usize]) -> Vec<ObjectId> {
        let mut ids = Vec::with_capacity(which.len());
        for &index in which {
            if let Some(page) = self.tree.pages.get(index) {
                let id = *self
                    .pages
                    .entry(page.id)
                    .or_insert_with(|| out.new_object_id());
                ids.push(id);
            }
        }
        ids
    }

    /// Copies page `index` (reserved before) into `out`, under the `Pages`
    /// node `parent`, with everything it references. Does nothing for a
    /// page that wasn't reserved.
    pub(crate) fn copy_page(&mut self, out: &mut Document, index: usize, parent: ObjectId) {
        let (src, tree) = (self.src, self.tree);
        let Some(page) = tree.pages.get(index) else {
            return;
        };
        let Some(&dest) = self.pages.get(&page.id) else {
            return;
        };
        let mut dict = src.get_dictionary(page.id).cloned().unwrap_or_default();
        for (key, value) in INHERITABLE.iter().zip(page.inherited.0.iter()) {
            if !dict.has(key) {
                if let Some(value) = value {
                    dict.set(*key, value.clone());
                }
            }
        }
        if !dict.has(b"MediaBox") {
            let media = dict
                .get(b"CropBox")
                .cloned()
                .unwrap_or_else(|_| Object::Array(LETTER.iter().map(|&v| v.into()).collect()));
            dict.set("MediaBox", media);
        }
        if !dict.has(b"Resources") {
            dict.set("Resources", Dictionary::new());
        }
        for key in [b"Parent".as_slice(), b"B", b"StructParents"] {
            dict.remove(key);
        }
        dict.set("Type", "Page");

        // The annotations' references are made output ids here, so they go
        // back in after the page's own references are remapped.
        let mut work = Vec::new();
        let kept = dict
            .remove(b"Annots")
            .and_then(|annots| self.annotations(&annots, out, &mut work));
        let mut object = Object::Dictionary(dict);
        self.remap(&mut object, out, &mut work);
        if let Object::Dictionary(dict) = &mut object {
            dict.set("Parent", parent);
            if let Some(kept) = kept {
                dict.set("Annots", kept);
            }
        }
        out.objects.insert(dest, object);

        while let Some(pending) = work.pop() {
            let mut object = match pending.prepared {
                Some(object) => object,
                None => src.get_object(pending.src).cloned().unwrap_or(Object::Null),
            };
            self.remap(&mut object, out, &mut work);
            out.objects.insert(pending.dest, object);
        }
    }

    /// The page's `/Annots` without the annotations that can't come along,
    /// each kept one prepared for copying, as output references (or
    /// remapped direct dictionaries). `None` drops the key (not an array).
    fn annotations(
        &mut self,
        annots: &Object,
        out: &mut Document,
        work: &mut Vec<Pending>,
    ) -> Option<Object> {
        let src = self.src;
        let (_, list) = src.dereference(annots).ok()?;
        let list = list.as_array().ok()?;
        let mut kept = Vec::with_capacity(list.len());
        for item in list {
            let (id, annot) = match item {
                Object::Reference(id) => match src.get_dictionary(*id) {
                    Ok(annot) => (Some(*id), annot),
                    Err(_) => continue,
                },
                Object::Dictionary(annot) => (None, annot),
                _ => continue,
            };
            if !self.keeps(annot) {
                continue;
            }
            let mut prepared = annot.clone();
            prepared.remove(b"StructParent");
            if matches!(prepared.get(b"Subtype"), Ok(Object::Name(name)) if name == b"Widget") {
                prepared.remove(b"Parent");
            }
            match id {
                Some(id) => {
                    if let Some(&copied) = self.copied.get(&id) {
                        kept.push(Object::Reference(copied));
                        continue;
                    }
                    let dest = out.new_object_id();
                    self.copied.insert(id, dest);
                    work.push(Pending {
                        src: id,
                        dest,
                        prepared: Some(Object::Dictionary(prepared)),
                    });
                    kept.push(Object::Reference(dest));
                }
                None => {
                    let mut direct = Object::Dictionary(prepared);
                    self.remap(&mut direct, out, work);
                    kept.push(direct);
                }
            }
        }
        Some(Object::Array(kept))
    }

    /// False for a link whose destination doesn't come along.
    fn keeps(&self, annot: &Dictionary) -> bool {
        if !matches!(annot.get(b"Subtype"), Ok(Object::Name(name)) if name == b"Link") {
            return true;
        }
        if let Ok(dest) = annot.get(b"Dest") {
            return self.dest_kept(dest);
        }
        let action = annot
            .get(b"A")
            .ok()
            .and_then(|action| self.src.dereference(action).ok())
            .and_then(|(_, action)| action.as_dict().ok());
        match action {
            Some(action) if matches!(action.get(b"S"), Ok(Object::Name(s)) if s == b"GoTo") => {
                action.get(b"D").map(|d| self.dest_kept(d)).unwrap_or(false)
            }
            _ => true,
        }
    }

    /// True for an explicit destination on a page this output gets.
    fn dest_kept(&self, dest: &Object) -> bool {
        let Ok((_, mut dest)) = self.src.dereference(dest) else {
            return false;
        };
        // A destination dictionary (`<< /D [...] >>`) holds the array.
        if let Ok(dict) = dest.as_dict() {
            match dict
                .get(b"D")
                .ok()
                .and_then(|d| self.src.dereference(d).ok())
            {
                Some((_, d)) => dest = d,
                None => return false,
            }
        }
        match dest.as_array().ok().and_then(|array| array.first()) {
            Some(Object::Reference(page)) => self.pages.contains_key(page),
            _ => false,
        }
    }

    /// Rewrites every reference inside `object` to its id in `out`,
    /// queueing objects not copied yet. Walks the object's own nesting with
    /// a stack, not recursion.
    fn remap(&mut self, object: &mut Object, out: &mut Document, work: &mut Vec<Pending>) {
        let mut stack: Vec<&mut Object> = vec![object];
        while let Some(item) = stack.pop() {
            match item {
                Object::Reference(id) => {
                    let id = *id;
                    *item = self.target(id, out, work);
                }
                Object::Array(items) => stack.extend(items.iter_mut()),
                Object::Dictionary(dict) => stack.extend(dict.iter_mut().map(|(_, value)| value)),
                Object::Stream(stream) => {
                    stack.extend(stream.dict.iter_mut().map(|(_, value)| value))
                }
                _ => {}
            }
        }
    }

    /// What a reference to source object `id` becomes in the output.
    fn target(&mut self, id: ObjectId, out: &mut Document, work: &mut Vec<Pending>) -> Object {
        if let Some(&page) = self.pages.get(&id) {
            return Object::Reference(page);
        }
        if self.tree.page_ids.contains(&id) || self.tree.skip.contains(&id) {
            return Object::Null;
        }
        if let Some(&copied) = self.copied.get(&id) {
            return Object::Reference(copied);
        }
        let dest = out.new_object_id();
        self.copied.insert(id, dest);
        work.push(Pending {
            src: id,
            dest,
            prepared: None,
        });
        Object::Reference(dest)
    }
}

/// The version for an output made from sources of `versions`: the highest,
/// and at least 1.5 (lopdf writes a cross-reference stream). Unreadable
/// versions are ignored.
pub(crate) fn output_version<'v>(versions: impl IntoIterator<Item = &'v str>) -> String {
    let parse = |version: &str| -> Option<(u32, u32)> {
        let (major, minor) = version.trim().split_once('.')?;
        Some((major.parse().ok()?, minor.parse().ok()?))
    };
    let (major, minor) = versions
        .into_iter()
        .filter_map(parse)
        .fold((1, 5), |best, version| best.max(version));
    format!("{major}.{minor}")
}

/// One output document being built from whole source pages: a flat
/// `Pages` node and a fresh catalog, nothing else of the sources.
pub(crate) struct Output {
    doc: Document,
    pages_id: ObjectId,
    kids: Vec<Object>,
    versions: Vec<String>,
}

impl Output {
    /// An empty output.
    pub(crate) fn new() -> Output {
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        Output {
            doc,
            pages_id,
            kids: Vec::new(),
            versions: Vec::new(),
        }
    }

    /// Adds the pages `which` (indexes into `tree`) of `src`, in that
    /// order. Links among them survive; links to other pages don't.
    pub(crate) fn add(&mut self, src: &Document, tree: &Tree, which: &[usize]) {
        let mut copier = Copier::new(src, tree);
        let ids = copier.reserve(&mut self.doc, which);
        for &index in which {
            copier.copy_page(&mut self.doc, index, self.pages_id);
        }
        self.kids.extend(ids.into_iter().map(Object::Reference));
        self.versions.push(src.version.clone());
    }

    /// How many pages were added.
    pub(crate) fn page_count(&self) -> usize {
        self.kids.len()
    }

    /// The finished document.
    pub(crate) fn finish(mut self) -> Document {
        let count = i64::try_from(self.kids.len()).unwrap_or(i64::MAX);
        self.doc.objects.insert(
            self.pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => Object::Array(self.kids),
                "Count" => count,
            }),
        );
        let catalog = self.doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => self.pages_id,
        });
        self.doc.trailer.set("Root", catalog);
        self.doc.version = output_version(self.versions.iter().map(String::as_str));
        self.doc
    }
}

#[cfg(test)]
pub(crate) mod tests {
    #![allow(clippy::unwrap_used)]

    use lopdf::content::{Content, Operation};
    use lopdf::xref::XrefType;
    use lopdf::Stream;

    use super::*;

    /// A content stream that writes `text` in Helvetica.
    pub(crate) fn text_stream(text: &str) -> Stream {
        let content = Content {
            operations: vec![
                Operation::new("BT", vec![]),
                Operation::new("Tf", vec!["F1".into(), 24.into()]),
                Operation::new("Td", vec![50.into(), 100.into()]),
                Operation::new("Tj", vec![Object::string_literal(text)]),
                Operation::new("ET", vec![]),
            ],
        };
        Stream::new(dictionary! {}, content.encode().unwrap())
    }

    fn rect(v: [i64; 4]) -> Object {
        Object::Array(v.iter().map(|&x| x.into()).collect())
    }

    /// Three pages. The root node carries MediaBox 612×792, the Resources
    /// and Rotate 90; a nested node overrides Rotate to 0 and adds a
    /// CropBox; page 2 has its own MediaBox. Page 2 has a form widget whose
    /// field holds a marker; page 3 links to page 1, to a named destination
    /// and to a URL. The catalog has an outline, a form, page labels, named
    /// destinations and an open action, each with a marker; the Info
    /// dictionary and an XMP stream hold more.
    pub(crate) fn fixture(tag: &str) -> Document {
        let mut doc = Document::with_version("1.7");
        let root = doc.new_object_id();
        let mid = doc.new_object_id();
        let font = doc.add_object(
            dictionary! {"Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica"},
        );
        let res = doc.add_object(dictionary! {"Font" => dictionary! {"F1" => font}});
        let contents: Vec<ObjectId> = (1..=3)
            .map(|i| doc.add_object(text_stream(&format!("{tag} page {i}"))))
            .collect();
        let (p1, p2, p3) = (
            doc.new_object_id(),
            doc.new_object_id(),
            doc.new_object_id(),
        );
        let field = doc.new_object_id();
        let widget = doc.add_object(dictionary! {
            "Type" => "Annot", "Subtype" => "Widget", "Rect" => rect([50, 50, 200, 80]),
            "P" => p2, "Parent" => field,
        });
        doc.objects.insert(
            field,
            Object::Dictionary(dictionary! {
                "FT" => "Tx", "T" => Object::string_literal("name"),
                "V" => Object::string_literal("MARK-FORM-VALUE"), "Kids" => vec![widget.into()],
            }),
        );
        let link_page = doc.add_object(dictionary! {
            "Type" => "Annot", "Subtype" => "Link", "Rect" => rect([0, 0, 100, 20]),
            "Dest" => vec![p1.into(), "Fit".into()],
        });
        let link_named = doc.add_object(dictionary! {
            "Type" => "Annot", "Subtype" => "Link", "Rect" => rect([0, 20, 100, 30]),
            "A" => dictionary! {"S" => "GoTo", "D" => Object::string_literal("chap3")},
        });
        let link_uri = doc.add_object(dictionary! {
            "Type" => "Annot", "Subtype" => "Link", "Rect" => rect([0, 30, 100, 50]),
            "A" => dictionary! {"S" => "URI", "URI" => Object::string_literal("https://example.org")},
        });
        doc.objects.insert(
            p1,
            Object::Dictionary(
                dictionary! {"Type" => "Page", "Parent" => root, "Contents" => contents[0]},
            ),
        );
        doc.objects.insert(
            p2,
            Object::Dictionary(dictionary! {
                "Type" => "Page", "Parent" => mid, "Contents" => contents[1],
                "MediaBox" => rect([0, 0, 300, 300]), "Annots" => vec![widget.into()],
                "StructParents" => 0,
            }),
        );
        doc.objects.insert(
            p3,
            Object::Dictionary(dictionary! {
                "Type" => "Page", "Parent" => mid, "Contents" => contents[2],
                "Annots" => vec![link_page.into(), link_named.into(), link_uri.into()],
            }),
        );
        doc.objects.insert(
            mid,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Parent" => root, "Kids" => vec![p2.into(), p3.into()],
                "Count" => 2, "Rotate" => 0, "CropBox" => rect([0, 0, 500, 500]),
            }),
        );
        doc.objects.insert(
            root,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Kids" => vec![p1.into(), mid.into()], "Count" => 3,
                "MediaBox" => rect([0, 0, 612, 792]), "Resources" => res, "Rotate" => 90,
            }),
        );
        let outlines = doc.new_object_id();
        let item = doc.add_object(dictionary! {
            "Title" => Object::string_literal("MARK-OUTLINE"), "Parent" => outlines,
            "Dest" => vec![p3.into(), "Fit".into()],
        });
        doc.objects.insert(
            outlines,
            Object::Dictionary(
                dictionary! {"Type" => "Outlines", "First" => item, "Last" => item, "Count" => 1},
            ),
        );
        let xmp = doc.add_object(Stream::new(
            dictionary! {"Type" => "Metadata", "Subtype" => "XML"},
            b"<x:xmpmeta>MARK-XMP</x:xmpmeta>".to_vec(),
        ));
        let catalog = doc.add_object(dictionary! {
            "Type" => "Catalog", "Pages" => root, "Outlines" => outlines, "Metadata" => xmp,
            "AcroForm" => dictionary! {"Fields" => vec![field.into()]},
            "PageLabels" => dictionary! {"Nums" => vec![0.into(), dictionary! {"S" => "r", "P" => Object::string_literal("MARK-LABEL")}.into()]},
            "Names" => dictionary! {"Dests" => dictionary! {"Names" => vec![Object::string_literal("chap3"), vec![p3.into(), "Fit".into()].into()]}},
            "OpenAction" => vec![p3.into(), "Fit".into()],
        });
        let info = doc.add_object(dictionary! {
            "Title" => Object::string_literal("MARK-TITLE"),
            "Author" => Object::string_literal("MARK-AUTHOR"),
            "Producer" => Object::string_literal("MARK-PRODUCER"),
            "Company" => Object::string_literal("MARK-COMPANY"),
            "Subject" => Object::string_literal(""),
        });
        doc.trailer.set("Root", catalog);
        doc.trailer.set("Info", info);
        doc
    }

    /// `doc` saved with a classic cross-reference table and no object
    /// streams (so marker strings are visible in the bytes).
    pub(crate) fn save_classic(doc: &Document) -> Vec<u8> {
        let mut doc = doc.clone();
        doc.reference_table.cross_reference_type = XrefType::CrossReferenceTable;
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        bytes
    }

    /// `doc` saved with object streams and a cross-reference stream.
    pub(crate) fn save_modern(doc: &Document) -> Vec<u8> {
        let mut doc = doc.clone();
        let mut bytes = Vec::new();
        doc.save_modern(&mut bytes).unwrap();
        bytes
    }

    pub(crate) fn contains(bytes: &[u8], needle: &str) -> bool {
        bytes
            .windows(needle.len())
            .any(|window| window == needle.as_bytes())
    }

    /// The effective value of an inheritable `key` on `page`.
    pub(crate) fn effective(doc: &Document, page: ObjectId, key: &[u8]) -> Option<Object> {
        let mut id = page;
        for _ in 0..64 {
            let dict = doc.get_dictionary(id).ok()?;
            if let Ok(value) = dict.get(key) {
                return Some(doc.dereference(value).ok()?.1.clone());
            }
            id = dict.get(b"Parent").and_then(Object::as_reference).ok()?;
        }
        None
    }

    pub(crate) fn ints(object: &Object) -> Vec<i64> {
        object
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_i64().unwrap())
            .collect()
    }

    fn reload(doc: Document) -> (Document, Vec<u8>) {
        let bytes = save_classic(&doc);
        (Document::load_mem(&bytes).unwrap(), bytes)
    }

    fn split(doc: &Document) -> Vec<Document> {
        let tree = Tree::walk(doc).unwrap();
        (0..tree.pages.len())
            .map(|i| {
                let mut out = Output::new();
                out.add(doc, &tree, &[i]);
                out.finish()
            })
            .collect()
    }

    #[test]
    fn the_walk_inherits_down_the_tree() {
        let doc = fixture("A");
        let tree = Tree::walk(&doc).unwrap();
        assert_eq!(tree.pages.len(), 3);
        assert!(tree.is_whole());
        let rotate = |i: usize| tree.pages[i].inherited.0[3].clone().unwrap();
        assert_eq!(rotate(0), Object::Integer(90));
        assert_eq!(rotate(1), Object::Integer(0));
        // Two nodes and the catalog are never copied.
        assert_eq!(tree.skip.len(), 3);
    }

    #[test]
    fn the_walk_survives_cycles_and_typeless_nodes() {
        let mut doc = fixture("A");
        let tree = Tree::walk(&doc).unwrap();
        let (p1, p2) = (tree.pages[0].id, tree.pages[1].id);
        let root = doc
            .catalog()
            .unwrap()
            .get(b"Pages")
            .unwrap()
            .as_reference()
            .unwrap();
        // The root lists itself, page 1 twice, and a missing object.
        let root_dict = doc.get_dictionary_mut(root).unwrap();
        let mut kids = root_dict.get(b"Kids").unwrap().as_array().unwrap().clone();
        kids.extend([root.into(), p1.into(), Object::Reference((999, 0))]);
        root_dict.set("Kids", kids);
        // Page 2 loses its /Type: still a page (no /Kids).
        doc.get_dictionary_mut(p2).unwrap().remove(b"Type");
        let tree = Tree::walk(&doc).unwrap();
        assert_eq!(tree.pages.len(), 3);
        assert!(tree.is_whole());

        // A /Count that disagrees means something went missing.
        doc.get_dictionary_mut(root).unwrap().set("Count", 4);
        assert!(!Tree::walk(&doc).unwrap().is_whole());

        doc.trailer.remove(b"Root");
        assert_eq!(Tree::walk(&doc).unwrap_err(), TreeError::NoTree);
    }

    #[test]
    fn split_pages_carry_what_they_inherit_and_nothing_else() {
        let source = fixture("A");
        for saved in [save_classic(&source), save_modern(&source)] {
            let doc = Document::load_mem(&saved).unwrap();
            let outputs = split(&doc);
            assert_eq!(outputs.len(), 3);
            let expect = [
                ([0, 0, 612, 792], 90, None),
                ([0, 0, 300, 300], 0, Some([0, 0, 500, 500])),
                ([0, 0, 612, 792], 0, Some([0, 0, 500, 500])),
            ];
            for (n, (out, (media, rotate, crop))) in outputs.into_iter().zip(expect).enumerate() {
                assert_eq!(out.version, "1.7");
                let (out, bytes) = reload(out);
                let pages = out.get_pages();
                assert_eq!(pages.len(), 1);
                let page = pages[&1];
                assert_eq!(ints(&effective(&out, page, b"MediaBox").unwrap()), media);
                assert_eq!(
                    effective(&out, page, b"Rotate").unwrap().as_i64().unwrap(),
                    rotate
                );
                assert_eq!(
                    effective(&out, page, b"CropBox").map(|c| ints(&c)),
                    crop.map(Vec::from)
                );
                let text = out.extract_text(&[1]).unwrap();
                assert!(text.contains(&format!("A page {}", n + 1)), "{text:?}");
                let page_dict = out.get_dictionary(page).unwrap();
                assert!(!page_dict.has(b"StructParents"));
                for marker in [
                    "MARK-FORM-VALUE",
                    "MARK-OUTLINE",
                    "MARK-XMP",
                    "MARK-TITLE",
                    "MARK-LABEL",
                    "chap3",
                    "AcroForm",
                    "OpenAction",
                ] {
                    assert!(!contains(&bytes, marker), "page {} has {marker}", n + 1);
                }
            }
        }
    }

    #[test]
    fn links_to_pages_left_behind_are_dropped() {
        let doc = fixture("A");
        let outputs = split(&doc);
        // Page 3 links to page 1 (gone), to a named destination (gone) and
        // to a URL (kept).
        let (page3, _) = reload(outputs.into_iter().nth(2).unwrap());
        let annots = page3.get_page_annotations(page3.get_pages()[&1]).unwrap();
        assert_eq!(annots.len(), 1);
        let action = annots[0].get(b"A").unwrap().as_dict().unwrap();
        assert_eq!(action.get(b"S").unwrap().as_name().unwrap(), b"URI");

        // The widget on page 2 keeps its appearance but not its field.
        let (page2, _) = reload(split(&doc).into_iter().nth(1).unwrap());
        let annots = page2.get_page_annotations(page2.get_pages()[&1]).unwrap();
        assert_eq!(annots.len(), 1);
        assert!(!annots[0].has(b"Parent"));
        let p = annots[0].get(b"P").unwrap().as_reference().unwrap();
        assert_eq!(p, page2.get_pages()[&1]);
    }

    #[test]
    fn merging_keeps_order_inherited_values_and_links_within_a_source() {
        let a = Document::load_mem(&save_classic(&fixture("A"))).unwrap();
        let mut b = fixture("B");
        b.version = "1.4".to_string();
        let b = Document::load_mem(&save_modern(&b)).unwrap();
        let mut out = Output::new();
        for doc in [&b, &a, &b] {
            let tree = Tree::walk(doc).unwrap();
            let all: Vec<usize> = (0..tree.pages.len()).collect();
            out.add(doc, &tree, &all);
        }
        assert_eq!(out.page_count(), 9);
        let merged = out.finish();
        assert_eq!(merged.version, "1.7");
        let (merged, bytes) = reload(merged);
        let pages = merged.get_pages();
        assert_eq!(pages.len(), 9);
        let texts: Vec<String> = (1..=9)
            .map(|n| merged.extract_text(&[n]).unwrap().trim().to_string())
            .collect();
        let tags = ["B", "B", "B", "A", "A", "A", "B", "B", "B"];
        for (n, (text, tag)) in texts.iter().zip(tags).enumerate() {
            assert_eq!(text, &format!("{tag} page {}", n % 3 + 1));
        }
        assert_eq!(
            effective(&merged, pages[&4], b"Rotate")
                .unwrap()
                .as_i64()
                .unwrap(),
            90
        );
        // Page 3 of each source still links to page 1 of the same source.
        let annots = merged.get_page_annotations(pages[&6]).unwrap();
        assert_eq!(annots.len(), 2);
        let dest = annots[0].get(b"Dest").unwrap().as_array().unwrap();
        assert_eq!(dest[0].as_reference().unwrap(), pages[&4]);
        for marker in ["MARK-FORM-VALUE", "MARK-OUTLINE", "MARK-XMP", "MARK-TITLE"] {
            assert!(!contains(&bytes, marker), "{marker}");
        }
        // One flat Pages node.
        let root = merged
            .catalog()
            .unwrap()
            .get(b"Pages")
            .unwrap()
            .as_reference()
            .unwrap();
        let root = merged.get_dictionary(root).unwrap();
        assert_eq!(root.get(b"Count").unwrap().as_i64().unwrap(), 9);
        assert!(!root.has(b"MediaBox") && !root.has(b"Rotate"));
    }

    #[test]
    fn a_page_without_boxes_or_resources_gets_defaults() {
        let mut doc = Document::with_version("1.3");
        let root = doc.new_object_id();
        let content = doc.add_object(text_stream("bare"));
        let page =
            doc.add_object(dictionary! {"Type" => "Page", "Parent" => root, "Contents" => content});
        doc.objects.insert(
            root,
            Object::Dictionary(
                dictionary! {"Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1},
            ),
        );
        let catalog = doc.add_object(dictionary! {"Type" => "Catalog", "Pages" => root});
        doc.trailer.set("Root", catalog);
        let out = split(&doc).remove(0);
        assert_eq!(out.version, "1.5");
        let (out, _) = reload(out);
        let page = out.get_pages()[&1];
        assert_eq!(ints(&effective(&out, page, b"MediaBox").unwrap()), LETTER);
        assert!(effective(&out, page, b"Resources")
            .unwrap()
            .as_dict()
            .is_ok());
    }

    #[test]
    fn a_shared_resource_is_copied_once_per_output() {
        let doc = fixture("A");
        let tree = Tree::walk(&doc).unwrap();
        let mut out = Output::new();
        out.add(&doc, &tree, &[0, 1, 2]);
        let merged = out.finish();
        let fonts = merged
            .objects
            .values()
            .filter(|o| matches!(o.as_dict().map(|d| d.get_type()), Ok(Ok(b"Font"))))
            .count();
        assert_eq!(fonts, 1);
    }

    #[test]
    fn output_versions_are_at_least_one_point_five() {
        assert_eq!(output_version(["1.3"]), "1.5");
        assert_eq!(output_version(["1.4", "1.7", "1.6"]), "1.7");
        assert_eq!(output_version(["2.0", "1.7"]), "2.0");
        assert_eq!(output_version(["junk", ""]), "1.5");
        assert_eq!(output_version(std::iter::empty::<&str>()), "1.5");
    }
}
