//! The heap Remove Metadata on HEIC holds at its peak (D28): a `meta` just
//! under its 16 MiB cap, and one packed at every cap of the model, through
//! `run_job`, under an allocator that counts. The tests take turns, so
//! nothing else allocates meanwhile.
//!
//! The plan holds the source's `meta` and its model while it writes the
//! rebuilt one, and the check the output's and its model, once what was
//! written is gone.
#![allow(clippy::unwrap_used)]

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};

use common::{plan_one, TestDir};
use grape_core::{Job, Operation, ToolKey};
use grape_engines::{run_job, EngineCtx, GrapeError, JobOutcome};

/// The bytes held now, and the most held since the last reset.
static HELD: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grow(bytes: usize) {
    let now = HELD.fetch_add(bytes, Ordering::Relaxed) + bytes;
    PEAK.fetch_max(now, Ordering::Relaxed);
}

fn shrink(bytes: usize) {
    HELD.fetch_sub(bytes, Ordering::Relaxed);
}

/// The system allocator, counting what it holds.
struct Counting;

// SAFETY: every method hands its arguments to `System` unchanged and
// returns what `System` returns, so every block is `System`'s, under
// `System`'s guarantees. The counting around it doesn't allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller upholds `alloc`'s contract for `layout`.
        let block = unsafe { System.alloc(layout) };
        if !block.is_null() {
            grow(layout.size());
        }
        block
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller upholds `alloc_zeroed`'s contract for `layout`.
        let block = unsafe { System.alloc_zeroed(layout) };
        if !block.is_null() {
            grow(layout.size());
        }
        block
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller upholds `dealloc`'s contract: `ptr` came from
        // this allocator, so from `System`, with `layout`.
        unsafe { System.dealloc(ptr, layout) };
        shrink(layout.size());
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller upholds `realloc`'s contract: `ptr` came from
        // `System` with `layout`, and `new_size` is valid for it.
        let block = unsafe { System.realloc(ptr, layout, new_size) };
        if !block.is_null() {
            // Counted as both while it moves.
            grow(new_size);
            shrink(layout.size());
        }
        block
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Each test's turn, held for all of it: what it builds counts too.
fn turn() -> MutexGuard<'static, ()> {
    static TURN: Mutex<()> = Mutex::new(());
    TURN.lock().unwrap_or_else(|e| e.into_inner())
}

/// The most heap `job` held over what was held before it ran, and how it
/// ended.
fn peak_of(job: &Job) -> (usize, Result<JobOutcome, GrapeError>) {
    let before = HELD.load(Ordering::Relaxed);
    PEAK.store(before, Ordering::Relaxed);
    let result = run_job(job, &EngineCtx::default(), &mut |_| {});
    (PEAK.load(Ordering::Relaxed) - before, result)
}

fn bx(kind: &[u8; 4], content: &[u8]) -> Vec<u8> {
    let mut out = u32::try_from(8 + content.len())
        .unwrap()
        .to_be_bytes()
        .to_vec();
    out.extend_from_slice(kind);
    out.extend_from_slice(content);
    out
}

fn full(kind: &[u8; 4], version: u8, flags: u32, content: &[u8]) -> Vec<u8> {
    let mut c = (u32::from(version) << 24 | flags).to_be_bytes().to_vec();
    c.extend_from_slice(content);
    bx(kind, &c)
}

/// An Exif payload: its header offset, `Exif\0\0`, and a big-endian TIFF
/// with a camera make.
fn exif() -> Vec<u8> {
    let mut p = 6u32.to_be_bytes().to_vec();
    p.extend_from_slice(b"Exif\0\0MM\0\x2a\0\0\0\x08\0\x01");
    p.extend_from_slice(&[0x01, 0x0F, 0, 2, 0, 0, 0, 4]);
    p.extend_from_slice(b"Cam\0");
    p.extend_from_slice(&[0; 4]);
    p
}

/// What goes in a HEIF file's `meta` for [`heif`]: item 1 is the primary
/// (100 bytes in `mdat`), item 2 its Exif.
struct Meta {
    /// `infe` boxes after items 1 and 2.
    infe: Vec<u8>,
    count: usize,
    /// The Exif's data in `idat` instead of `mdat`, with `idat` this long.
    idat: Option<usize>,
    /// For each item after 2, its extents (each one byte of the primary's
    /// data).
    extents: Vec<u16>,
    /// `iref`'s boxes after the Exif's `cdsc`.
    iref: Vec<u8>,
    ipco: Vec<u8>,
    /// `iprp`'s boxes after `ipco`: the `ipma` boxes.
    ipma: Vec<u8>,
    grpl: Option<Vec<u8>>,
    /// More boxes at the end of `meta`.
    extra: Vec<u8>,
}

/// A HEIF file: `ftyp`, a `meta` of `m`, then `mdat` (the primary's 100
/// bytes, then the Exif's, unless it's in `idat`).
fn heif(m: &Meta) -> Vec<u8> {
    let exif = exif();
    let build = |mdat_at: u32| -> Vec<u8> {
        let primary = mdat_at + 8;
        let mut iinf = u16::try_from(m.count).unwrap().to_be_bytes().to_vec();
        iinf.extend_from_slice(&full(b"infe", 2, 0, b"\0\x01\0\0hvc1\0"));
        iinf.extend_from_slice(&full(b"infe", 2, 1, b"\0\x02\0\0Exif\0"));
        iinf.extend_from_slice(&m.infe);
        let mut iloc = vec![0x44, 0x00];
        iloc.extend_from_slice(&u16::try_from(m.count).unwrap().to_be_bytes());
        let extent = |iloc: &mut Vec<u8>, at: u32, len: u32| {
            iloc.extend_from_slice(&at.to_be_bytes());
            iloc.extend_from_slice(&len.to_be_bytes());
        };
        iloc.extend_from_slice(&[0, 1, 0, 0, 0, 0, 0, 1]);
        extent(&mut iloc, primary, 100);
        match m.idat {
            Some(_) => {
                iloc.extend_from_slice(&[0, 2, 0, 1, 0, 0, 0, 1]);
                extent(&mut iloc, 0, exif.len() as u32);
            }
            None => {
                iloc.extend_from_slice(&[0, 2, 0, 0, 0, 0, 0, 1]);
                extent(&mut iloc, primary + 100, exif.len() as u32);
            }
        }
        for (id, &n) in (3u32..).zip(&m.extents) {
            iloc.extend_from_slice(&u16::try_from(id).unwrap().to_be_bytes());
            iloc.extend_from_slice(&[0, 0, 0, 0]);
            iloc.extend_from_slice(&n.to_be_bytes());
            for _ in 0..n {
                extent(&mut iloc, primary, 1);
            }
        }
        let mut iref = bx(b"cdsc", &[0, 2, 0, 1, 0, 1]);
        iref.extend_from_slice(&m.iref);
        let mut content = vec![0; 4];
        content.extend_from_slice(&full(
            b"hdlr",
            0,
            0,
            &[&[0; 4][..], b"pict", &[0; 13]].concat(),
        ));
        content.extend_from_slice(&full(b"pitm", 0, 0, &[0, 1]));
        content.extend_from_slice(&full(b"iinf", 0, 0, &iinf));
        content.extend_from_slice(&full(b"iloc", 1, 0, &iloc));
        content.extend_from_slice(&full(b"iref", 0, 0, &iref));
        content.extend_from_slice(&bx(
            b"iprp",
            &[bx(b"ipco", &m.ipco), m.ipma.clone()].concat(),
        ));
        if let Some(n) = m.idat {
            let mut idat = exif.clone();
            idat.resize(n, 0x5A);
            content.extend_from_slice(&bx(b"idat", &idat));
        }
        if let Some(grpl) = &m.grpl {
            content.extend_from_slice(&bx(b"grpl", grpl));
        }
        content.extend_from_slice(&m.extra);
        bx(b"meta", &content)
    };
    let ftyp = bx(b"ftyp", b"heic\0\0\0\0mif1heic");
    let mdat_at = (ftyp.len() + build(0).len()) as u32;
    let mut mdat = vec![0xA1; 100];
    if m.idat.is_none() {
        mdat.extend_from_slice(&exif);
    }
    [ftyp, build(mdat_at), bx(b"mdat", &mdat)].concat()
}

/// The size of `file`'s `meta`, after `ftyp`'s 24 bytes.
fn meta_len(file: &[u8]) -> usize {
    u32::from_be_bytes(file[24..28].try_into().unwrap()) as usize
}

/// Runs Remove Metadata on `file` and gives its peak heap in metas.
fn metas(dir: &TestDir, name: &str, file: &[u8]) -> f64 {
    let (_, job) = plan_one(dir, name, file, Operation::tool(ToolKey::RemoveMetadata));
    let (peak, result) = peak_of(&job);
    result.unwrap();
    let metas = peak as f64 / meta_len(file) as f64;
    eprintln!(
        "{name}: meta {:.2} MiB, peak {:.2} MiB, {metas:.2} metas",
        meta_len(file) as f64 / f64::from(1 << 20),
        peak as f64 / f64::from(1 << 20)
    );
    metas
}

#[test]
fn a_meta_of_data_just_under_its_cap_is_held_twice() {
    // An idat of 16 MiB, the Exif at its start: the source's meta and the
    // rebuilt one, and a model of a few items.
    let _turn = turn();
    let dir = TestDir::new("heic-heap-meta");
    let file = heif(&Meta {
        infe: Vec::new(),
        count: 2,
        idat: Some((16 << 20) - 512),
        extents: Vec::new(),
        iref: Vec::new(),
        ipco: bx(b"hvcC", &[1; 23]),
        ipma: full(b"ipma", 0, 0, &[0, 0, 0, 1, 0, 1, 1, 0x81]),
        grpl: None,
        extra: Vec::new(),
    });
    assert!(meta_len(&file) > (16 << 20) - 256);
    let metas = metas(&dir, "big.heic", &file);
    assert!(metas < 2.25, "{metas:.2} metas");
}

#[test]
fn a_meta_packed_at_every_cap_is_held_twice_beside_its_model() {
    // 65,535 items, 262,134 extents, 65,534 iref boxes of 1,048,529 ids,
    // 65,536 properties, 65,535 ipma entries of 1,048,560 associations in
    // 65,535 ipma boxes (65,536 boxes in iprp), 1,048,576 group entities
    // and 65,536 boxes in meta: its model is about 2.4 times its 15 MiB.
    let _turn = turn();
    let dir = TestDir::new("heic-heap-caps");
    let items = 65_535u16;
    let mut infe = Vec::new();
    let mut iref = Vec::new();
    for id in 3..=items {
        let mut c = id.to_be_bytes().to_vec();
        c.extend_from_slice(b"\0\0hvt1\0");
        infe.extend_from_slice(&full(b"infe", 2, 1, &c));
        let mut r = id.to_be_bytes().to_vec();
        r.extend_from_slice(&16u16.to_be_bytes());
        r.extend_from_slice(&[0, 1].repeat(16));
        iref.extend_from_slice(&bx(b"thmb", &r));
    }
    // A udes first, so every later index is renumbered.
    let mut ipco = full(b"udes", 0, 0, b"en\0Title\0\0\0");
    ipco.extend_from_slice(&bx(b"hvcC", &[1; 23]));
    for _ in 2..65_536 {
        ipco.extend_from_slice(&bx(b"abcd", b""));
    }
    let mut entries = 65_535u32.to_be_bytes().to_vec();
    for id in 1..=items {
        entries.extend_from_slice(&id.to_be_bytes());
        entries.push(16);
        for k in 0..16 {
            let index = 1 + (u32::from(id) + k) % 1000;
            entries.extend_from_slice(&u16::try_from(index).unwrap().to_be_bytes());
        }
    }
    let mut ipma = full(b"ipma", 0, 1, &entries);
    for _ in 1..65_535 {
        ipma.extend_from_slice(&full(b"ipma", 0, 1, &[0; 4]));
    }
    let mut grpl = Vec::new();
    for g in 0..16_384u32 {
        let mut c = (100_000 + g).to_be_bytes().to_vec();
        c.extend_from_slice(&64u32.to_be_bytes());
        for e in 0..64u32 {
            c.extend_from_slice(&(3 + (g * 64 + e) % 65_000).to_be_bytes());
        }
        grpl.extend_from_slice(&full(b"altr", 0, 0, &c));
    }
    // hdlr, pitm, iinf, iloc, iref, iprp and grpl, then the rest.
    let extra = bx(b"abcd", b"").repeat(65_536 - 7);
    let file = heif(&Meta {
        infe,
        count: usize::from(items),
        idat: None,
        extents: vec![4; usize::from(items) - 2],
        iref,
        ipco,
        ipma,
        grpl: Some(grpl),
        extra,
    });
    let metas = metas(&dir, "caps.heic", &file);
    assert!(metas < 5.0, "{metas:.2} metas");
}
