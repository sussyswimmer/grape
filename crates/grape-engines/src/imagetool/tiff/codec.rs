//! The codecs Compress on TIFF reads and writes (D28): a strip or a tile
//! decoded, cut at the size its IFD gives it, and deflated again, never
//! held whole.
//!
//! - **Decoded** ([`Decoder`]): None (copied), LZW (weezl, with TIFF's
//!   early code-size switch, MSB first, stopping once the block is full as
//!   libtiff and the tiff crate do), Deflate (flate2's zlib inflater) and
//!   PackBits (Grape's own). Old-style LZW (LSB first, from before TIFF 5,
//!   which libtiff tells by its first two bytes) isn't decoded: read as
//!   MSB-first codes it could come out as other bytes without an error.
//! - **Cut** at the block's size ([`Geometry`]): its rows times a row's
//!   bytes (a plane's, when the planes are stored apart; a row of YCbCr
//!   sampling units when the chroma is subsampled, TIFF 6 §21), the last
//!   strip of each plane holding what's left of the picture, a tile always
//!   full size. Bytes after that are dropped, as readers drop them; data
//!   that ends before it can't be decoded.
//! - **Predicted** ([`Predictor`]): horizontal differencing (TIFF's
//!   predictor 2) over each row of 8 or 16-bit samples, in the file's byte
//!   order, and its inverse.
//! - **Deflated** ([`Encoder`]) at zlib's level 9 (flate2's Rust backend,
//!   miniz_oxide), the state reused from block to block.
//! - **Checked** ([`Codecs::check`]): a block Grape wrote, inflated again
//!   and its predictor undone, gives the CRC32 of the bytes it was made
//!   from ([`Codecs::recode`]).
//!
//! The decoded bytes go by in steps of [`STEP`] (whole rows when a
//! predictor runs over them), so what's held is a step, 64 KiB of the
//! block, and the codecs' state.

use std::io::Write;

use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress, Status};

use super::super::strip::{read_at, Error, In};
use super::read::{read_value, Block, Entry, Ifd, Order, Value};

/// The most decoded bytes handed on at once, unless a row is longer: what
/// the caller's cancel check is between.
pub(crate) const STEP: usize = 256 * 1024;

/// How much of a block is read at once.
const INPUT: usize = 64 * 1024;

/// The most samples a pixel has, for reading BitsPerSample and SampleFormat.
const MAX_SAMPLES: u64 = 1 << 16;

/// Why an IFD's blocks can't be re-encoded: the end of "copied as it is,
/// because …", for the log.
pub(crate) type Why = String;

/// A strip's or a tile's compression, among those Compress decodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Codec {
    None,
    Lzw,
    Deflate,
    PackBits,
}

impl Codec {
    /// The codec of Compression (259) `n`: 1, 5, 8 and Adobe's old 32946,
    /// or 32773; `None` for any other.
    pub(crate) fn of(n: u64) -> Option<Codec> {
        match n {
            1 => Some(Codec::None),
            5 => Some(Codec::Lzw),
            8 | 32946 => Some(Codec::Deflate),
            32773 => Some(Codec::PackBits),
            _ => None,
        }
    }
}

/// How an IFD's strips or tiles are laid out once decoded: how many there
/// are, and each one's size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Geometry {
    /// The bytes a row of a block holds: of pixels (of one plane's samples
    /// when the planes are apart), or of YCbCr sampling units.
    pub(crate) row: u64,
    /// The bits of each sample (the same for all).
    pub(crate) bits: u64,
    /// The samples a pixel has.
    pub(crate) samples: u64,
    /// Each sample in a plane of its own (PlanarConfiguration 2).
    pub(crate) planar: bool,
    /// Chroma subsampled YCbCr, whose rows are of sampling units.
    pub(crate) subsampled: bool,
    /// Tiles rather than strips.
    tiled: bool,
    /// The picture's rows.
    height: u64,
    /// The pixel rows of a full block: RowsPerStrip, or the tile's length.
    rows: u64,
    /// The pixel rows a row of the block covers: 1, or the vertical
    /// subsampling.
    per_row: u64,
    /// The blocks in each plane.
    per_plane: u64,
    /// The planes: one, or each sample's.
    planes: u64,
}

impl Geometry {
    /// The layout `ifd` says its blocks have once decoded, or why it can't
    /// be told; reads BitsPerSample and YCbCrSubsampling from `r` when they
    /// aren't inline, taking what's read from `left` (what's left of the
    /// bytes it may read for that: past it, the file is too large).
    pub(crate) fn of(
        r: &mut dyn In,
        ifd: &Ifd,
        order: Order,
        left: &mut u64,
    ) -> Result<Result<Geometry, Why>, Error> {
        let get = |tag| ifd.uint(tag, order);
        let not = |why: String| Ok(Err(why));
        let (Some(width), Some(height)) = (get(256), get(257)) else {
            return not("it has no size".into());
        };
        if width == 0 || height == 0 {
            return not(format!("it's {width} × {height}"));
        }
        let samples = get(277).unwrap_or(1);
        if samples == 0 || samples > MAX_SAMPLES {
            return not(format!("its SamplesPerPixel is {samples}"));
        }
        let bits = match ifd.get(258) {
            None => vec![1],
            Some(entry) => match numbers(r, entry, order, MAX_SAMPLES, left)? {
                Some(bits) => bits,
                None => return not("its BitsPerSample can't be read".into()),
            },
        };
        let bits = match bits.as_slice() {
            [one] => *one,
            [first, rest @ ..] if bits.len() as u64 == samples => {
                if rest.iter().any(|b| b != first) {
                    return not(format!("its samples have different sizes: {bits:?}"));
                }
                *first
            }
            _ => return not(format!("its BitsPerSample has {} values", bits.len())),
        };
        if bits == 0 || bits > 64 {
            return not(format!("its BitsPerSample is {bits}"));
        }
        let planar = match get(284).unwrap_or(1) {
            1 => false,
            2 => true,
            n => return not(format!("its PlanarConfiguration is {n}")),
        };
        // YCbCr is subsampled 2 × 2 unless it says otherwise (TIFF 6 §21).
        let (h, v) = if get(262) == Some(6) {
            let sub = match ifd.get(530) {
                None => vec![2, 2],
                Some(entry) => numbers(r, entry, order, 2, left)?.unwrap_or_default(),
            };
            match sub.as_slice() {
                &[h @ (1 | 2 | 4), v @ (1 | 2 | 4)] => (h, v),
                _ => return not(format!("its YCbCrSubsampling is {sub:?}")),
            }
        } else {
            (1, 1)
        };
        let subsampled = (h, v) != (1, 1);
        if subsampled && (planar || samples != 3) {
            return not("its subsampled YCbCr isn't three samples in one plane".into());
        }
        let past = || not("its sizes are past any number".into());
        let (tiled, across, rows, per_plane) = match (ifd.tiled, get(322), get(323)) {
            (true, Some(tw), Some(tl)) if tw > 0 && tl > 0 => {
                let Some(per_plane) = width.div_ceil(tw).checked_mul(height.div_ceil(tl)) else {
                    return past();
                };
                (true, tw, tl, per_plane)
            }
            (true, _, _) => return not("it has no tile size".into()),
            (false, _, _) => {
                let rows = get(278).unwrap_or(height).min(height);
                if rows == 0 {
                    return not("its RowsPerStrip is 0".into());
                }
                (false, width, rows, height.div_ceil(rows))
            }
        };
        let planes = if planar { samples } else { 1 };
        // A row's bits: of its pixels' samples, of one plane's, or of
        // sampling units, each h × v luma and two chroma samples.
        let row_bits = if subsampled {
            across
                .div_ceil(h)
                .checked_mul(h * v + 2)
                .and_then(|n| n.checked_mul(bits))
        } else {
            let per_pixel = if planar { 1 } else { samples };
            across
                .checked_mul(per_pixel)
                .and_then(|n| n.checked_mul(bits))
        };
        // A full block, and all of them, must have a size.
        let row = row_bits.map(|b| b.div_ceil(8)).filter(|row| {
            row.checked_mul(rows.div_ceil(v))
                .and_then(|block| block.checked_mul(per_plane)?.checked_mul(planes))
                .is_some()
        });
        let Some(row) = row else {
            return past();
        };
        Ok(Ok(Geometry {
            row,
            bits,
            samples,
            planar,
            subsampled,
            tiled,
            height,
            rows,
            per_row: v,
            per_plane,
            planes,
        }))
    }

    /// How many blocks the layout has.
    pub(crate) fn blocks(&self) -> u64 {
        self.per_plane * self.planes
    }

    /// The bytes block `n` decodes to: a full block, or a plane's last
    /// strip, which holds the rows left.
    pub(crate) fn expected(&self, n: u64) -> u64 {
        let rows = if self.tiled {
            self.rows
        } else {
            let first = (n % self.per_plane.max(1)) * self.rows;
            self.rows.min(self.height.saturating_sub(first))
        };
        rows.div_ceil(self.per_row) * self.row
    }
}

/// The unsigned numbers `entry` holds (BYTE, SHORT, LONG or LONG8), at
/// most `cap` of them, read from `r` when they aren't inline and taken
/// from `left`; `None` when there are more, they aren't numbers, or they
/// can't be read.
pub(crate) fn numbers(
    r: &mut dyn In,
    entry: &Entry,
    order: Order,
    cap: u64,
    left: &mut u64,
) -> Result<Option<Vec<u64>>, Error> {
    let size = match entry.kind {
        1 => 1,
        3 => 2,
        4 => 4,
        16 => 8,
        _ => return Ok(None),
    };
    if entry.count > cap {
        return Ok(None);
    }
    if let Value::At { len, .. } = entry.value {
        *left = left
            .checked_sub(len)
            .ok_or_else(|| Error::TooLarge("sample sizes read past the file's size".into()))?;
    }
    let bytes = read_value(r, entry, cap * 8)?;
    Ok(bytes.map(|bytes| bytes.chunks_exact(size).map(|n| order.uint(n)).collect()))
}

/// TIFF's horizontal differencing (predictor 2): each sample less the one
/// `stride` samples before it in its row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Predictor {
    /// 16-bit samples rather than 8-bit ones.
    pub(crate) wide: bool,
    /// The samples from one to the one it's taken from: a pixel's
    /// samples, or 1 when each plane is apart.
    pub(crate) stride: usize,
    /// The file's byte order, which 16-bit samples are in.
    pub(crate) order: Order,
}

impl Predictor {
    /// Differences `row` (whole samples) in place.
    pub(crate) fn apply(&self, row: &mut [u8]) {
        let s = self.stride;
        if self.wide {
            let n = row.len() / 2;
            for i in (s..n).rev() {
                let v = self.get(row, i).wrapping_sub(self.get(row, i - s));
                self.set(row, i, v);
            }
        } else {
            for i in (s..row.len()).rev() {
                row[i] = row[i].wrapping_sub(row[i - s]);
            }
        }
    }

    /// Undoes [`Predictor::apply`] on `row` in place.
    pub(crate) fn undo(&self, row: &mut [u8]) {
        let s = self.stride;
        if self.wide {
            for i in s..row.len() / 2 {
                let v = self.get(row, i).wrapping_add(self.get(row, i - s));
                self.set(row, i, v);
            }
        } else {
            for i in s..row.len() {
                row[i] = row[i].wrapping_add(row[i - s]);
            }
        }
    }

    fn get(&self, row: &[u8], i: usize) -> u16 {
        let pair = [row[2 * i], row[2 * i + 1]];
        match self.order {
            Order::Little => u16::from_le_bytes(pair),
            Order::Big => u16::from_be_bytes(pair),
        }
    }

    fn set(&self, row: &mut [u8], i: usize, v: u16) {
        let pair = match self.order {
            Order::Little => v.to_le_bytes(),
            Order::Big => v.to_be_bytes(),
        };
        row[2 * i..2 * i + 2].copy_from_slice(&pair);
    }
}

/// How many decoded bytes go by at once for rows of `row` bytes: whole
/// rows, as many as [`STEP`] holds (one at least), when a predictor runs
/// over them; [`STEP`] otherwise.
pub(crate) fn step(row: u64, predicted: bool) -> usize {
    match usize::try_from(row) {
        Ok(row) if predicted && row > 0 => (STEP / row).max(1) * row,
        _ => STEP,
    }
}

/// Why a block wasn't decoded.
#[derive(Debug)]
pub(crate) enum Fail {
    /// Its data can't be decoded to its size: why, for the log.
    Undecodable(Why),
    /// Reading the source or writing the output failed, or the job stopped.
    Job(Error),
}

impl From<Error> for Fail {
    fn from(e: Error) -> Fail {
        Fail::Job(e)
    }
}

/// A block's bytes, read [`INPUT`] at a time.
struct Input<'b> {
    buf: &'b mut [u8],
    /// The read part of `buf`, and how much of it is used.
    len: usize,
    pos: usize,
    /// Where the next read starts, and where the block ends.
    at: u64,
    end: u64,
}

impl Input<'_> {
    /// What's read and not used yet, reading on when that's nothing; empty
    /// at the block's end.
    fn fill(&mut self, r: &mut dyn In) -> Result<&[u8], Error> {
        if self.pos == self.len && self.at < self.end {
            let n = usize::try_from(self.end - self.at)
                .map_or(self.buf.len(), |n| n.min(self.buf.len()));
            read_at(r, self.at, &mut self.buf[..n])?;
            self.at += n as u64;
            self.len = n;
            self.pos = 0;
        }
        Ok(&self.buf[self.pos..self.len])
    }

    fn consume(&mut self, n: usize) {
        self.pos += n;
    }

    /// Whether every byte of the block is used.
    fn done(&self) -> bool {
        self.pos == self.len && self.at == self.end
    }
}

/// The decoded bytes, gathered into steps and handed on.
struct Fill<'c> {
    step: &'c mut [u8],
    filled: usize,
    /// The bytes still to come before the block is full.
    left: u64,
    each: &'c mut dyn FnMut(&mut [u8]) -> Result<(), Error>,
}

impl Fill<'_> {
    /// Where the next decoded bytes go: never empty while the block isn't
    /// full.
    fn room(&mut self) -> &mut [u8] {
        let free = self.step.len() - self.filled;
        let free = usize::try_from(self.left).map_or(free, |left| free.min(left));
        &mut self.step[self.filled..self.filled + free]
    }

    /// `n` bytes went into the room; a full step, or the block's last
    /// bytes, are handed on.
    fn took(&mut self, n: usize) -> Result<(), Error> {
        self.filled += n;
        self.left -= n as u64;
        if self.filled == self.step.len() || (self.left == 0 && self.filled > 0) {
            (self.each)(&mut self.step[..self.filled])?;
            self.filled = 0;
        }
        Ok(())
    }

    fn full(&self) -> bool {
        self.left == 0
    }
}

/// What a PackBits decoder is in the middle of.
enum Pack {
    /// Before a header byte.
    Header,
    /// Copying this many bytes more.
    Literal(usize),
    /// Before the byte a run of this many repeats.
    RunOf(usize),
    /// Writing this byte this many times more.
    Run(u8, usize),
}

/// The decoders, kept from block to block.
pub(crate) struct Decoder {
    lzw: weezl::decode::Decoder,
    inflate: Decompress,
    input: Vec<u8>,
    step: Vec<u8>,
}

impl Default for Decoder {
    fn default() -> Self {
        Decoder::new()
    }
}

impl Decoder {
    pub(crate) fn new() -> Decoder {
        let lzw = weezl::decode::Configuration::with_tiff_size_switch(weezl::BitOrder::Msb, 8)
            .with_yield_on_full_buffer(true)
            .build();
        Decoder {
            lzw,
            inflate: Decompress::new(true),
            input: vec![0; INPUT],
            step: Vec::new(),
        }
    }

    /// Decodes `block` of `r` as `codec` and hands `each` its first
    /// `expected` bytes, `step` at a time (fewer only at the end); the
    /// bytes after them are never read. Data that ends before that, or
    /// that `codec` can't read, is [`Fail::Undecodable`]. With `exact`,
    /// Deflate data must end there too: a block Grape wrote.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn decode(
        &mut self,
        r: &mut dyn In,
        block: Block,
        codec: Codec,
        expected: u64,
        step: usize,
        exact: bool,
        each: &mut dyn FnMut(&mut [u8]) -> Result<(), Error>,
    ) -> Result<(), Fail> {
        if expected == 0 {
            return Ok(());
        }
        self.step.resize(step.max(1), 0);
        let mut input = Input {
            buf: &mut self.input,
            len: 0,
            pos: 0,
            at: block.offset,
            end: block.offset + block.len,
        };
        let mut fill = Fill {
            step: &mut self.step,
            filled: 0,
            left: expected,
            each,
        };
        let short = || Fail::Undecodable("its data ends before its picture does".into());
        let stuck = || Fail::Undecodable("its decoder stopped short".into());
        match codec {
            Codec::None => {
                while !fill.full() {
                    let inp = input.fill(r)?;
                    if inp.is_empty() {
                        return Err(short());
                    }
                    let room = fill.room();
                    let n = room.len().min(inp.len());
                    room[..n].copy_from_slice(&inp[..n]);
                    input.consume(n);
                    fill.took(n)?;
                }
            }
            Codec::PackBits => {
                let mut state = Pack::Header;
                while !fill.full() {
                    state = match state {
                        Pack::Run(byte, n) => {
                            let room = fill.room();
                            let k = room.len().min(n);
                            room[..k].fill(byte);
                            fill.took(k)?;
                            if k == n {
                                Pack::Header
                            } else {
                                Pack::Run(byte, n - k)
                            }
                        }
                        Pack::Literal(n) => {
                            let inp = input.fill(r)?;
                            if inp.is_empty() {
                                return Err(short());
                            }
                            let room = fill.room();
                            let k = room.len().min(n).min(inp.len());
                            room[..k].copy_from_slice(&inp[..k]);
                            input.consume(k);
                            fill.took(k)?;
                            if k == n {
                                Pack::Header
                            } else {
                                Pack::Literal(n - k)
                            }
                        }
                        Pack::Header | Pack::RunOf(_) => {
                            let inp = input.fill(r)?;
                            let Some(&b) = inp.first() else {
                                return Err(short());
                            };
                            input.consume(1);
                            match state {
                                Pack::RunOf(n) => Pack::Run(b, n),
                                // 0 to 127: that many and one copied; -1 to
                                // -127: the next byte that many and one
                                // times; -128 nothing, as libtiff reads it.
                                _ => match b as i8 {
                                    -128 => Pack::Header,
                                    n @ 0.. => Pack::Literal(n as usize + 1),
                                    n => Pack::RunOf(1 + n.unsigned_abs() as usize),
                                },
                            }
                        }
                    };
                }
            }
            Codec::Lzw => {
                // Old-style LZW starts with a clear code written LSB first:
                // libtiff's test.
                let mut head = [0; 2];
                if block.len >= 2 {
                    read_at(r, block.offset, &mut head)?;
                    if head[0] == 0 && head[1] & 1 == 1 {
                        return Err(Fail::Undecodable("its LZW is the old-style kind".into()));
                    }
                }
                self.lzw.reset();
                while !fill.full() {
                    let inp = input.fill(r)?;
                    let empty = inp.is_empty();
                    let res = self.lzw.decode_bytes(inp, fill.room());
                    input.consume(res.consumed_in);
                    fill.took(res.consumed_out)?;
                    match res.status {
                        // Nothing more comes of it: the data has ended.
                        Ok(weezl::LzwStatus::Ok | weezl::LzwStatus::NoProgress)
                            if res.consumed_in == 0 && res.consumed_out == 0 =>
                        {
                            return Err(if empty { short() } else { stuck() });
                        }
                        Ok(weezl::LzwStatus::Ok | weezl::LzwStatus::NoProgress) => {}
                        Ok(weezl::LzwStatus::Done) => {
                            if !fill.full() {
                                return Err(short());
                            }
                        }
                        Err(e) => return Err(Fail::Undecodable(format!("its LZW is broken: {e}"))),
                    }
                }
            }
            Codec::Deflate => {
                self.inflate.reset(true);
                while !fill.full() {
                    let inp = input.fill(r)?;
                    let (was_in, was_out) = (self.inflate.total_in(), self.inflate.total_out());
                    let status = self
                        .inflate
                        .decompress(inp, fill.room(), FlushDecompress::None)
                        .map_err(|e| Fail::Undecodable(format!("its Deflate is broken: {e}")))?;
                    let used = (self.inflate.total_in() - was_in) as usize;
                    let made = (self.inflate.total_out() - was_out) as usize;
                    input.consume(used);
                    fill.took(made)?;
                    match status {
                        Status::StreamEnd if !fill.full() => return Err(short()),
                        Status::StreamEnd => {}
                        Status::Ok | Status::BufError if used == 0 && made == 0 => {
                            return Err(if input.done() { short() } else { stuck() });
                        }
                        Status::Ok | Status::BufError => {}
                    }
                }
                if exact {
                    ends(&mut self.inflate, r, &mut input)?;
                }
            }
        }
        Ok(())
    }
}

/// Fails unless the Deflate data `inflate` is reading in `input` ends
/// right after the bytes it gave: its stream ends, with nothing after it.
fn ends(inflate: &mut Decompress, r: &mut dyn In, input: &mut Input) -> Result<(), Fail> {
    let longer = || Fail::Undecodable("its Deflate data holds more than its picture".into());
    let mut spare = [0; 1];
    loop {
        let inp = input.fill(r)?;
        let (was_in, was_out) = (inflate.total_in(), inflate.total_out());
        let status = inflate
            .decompress(inp, &mut spare, FlushDecompress::None)
            .map_err(|e| Fail::Undecodable(format!("its Deflate is broken: {e}")))?;
        let used = (inflate.total_in() - was_in) as usize;
        input.consume(used);
        if inflate.total_out() > was_out {
            return Err(longer());
        }
        match status {
            Status::StreamEnd if input.done() => return Ok(()),
            Status::StreamEnd => return Err(longer()),
            _ if used == 0 => return Err(Fail::Undecodable("its Deflate data doesn't end".into())),
            _ => {}
        }
    }
}

/// The zlib deflater, kept from block to block.
pub(crate) struct Encoder {
    deflate: Compress,
    out: Vec<u8>,
}

impl Default for Encoder {
    fn default() -> Self {
        Encoder::new()
    }
}

impl Encoder {
    pub(crate) fn new() -> Encoder {
        Encoder {
            deflate: Compress::new(Compression::best(), true),
            out: vec![0; INPUT],
        }
    }

    /// Starts a new zlib stream.
    fn begin(&mut self) {
        self.deflate.reset();
    }

    /// Deflates `data` into `w`, holding back what the next bytes may
    /// still change.
    fn feed(&mut self, mut data: &[u8], w: &mut dyn Write) -> Result<(), Error> {
        while !data.is_empty() {
            let used = self.run(data, w, FlushCompress::None)?.0;
            data = &data[used..];
        }
        Ok(())
    }

    /// Ends the stream, writing the rest of it into `w`.
    fn finish(&mut self, w: &mut dyn Write) -> Result<(), Error> {
        while self.run(&[], w, FlushCompress::Finish)?.1 != Status::StreamEnd {}
        Ok(())
    }

    /// One step of the deflater over `data`, its output written into `w`;
    /// gives the bytes it used and how it ended.
    fn run(
        &mut self,
        data: &[u8],
        w: &mut dyn Write,
        flush: FlushCompress,
    ) -> Result<(usize, Status), Error> {
        let (was_in, was_out) = (self.deflate.total_in(), self.deflate.total_out());
        let status = self
            .deflate
            .compress(data, &mut self.out, flush)
            .map_err(|e| Error::Write(std::io::Error::other(e)))?;
        let used = (self.deflate.total_in() - was_in) as usize;
        let made = (self.deflate.total_out() - was_out) as usize;
        w.write_all(&self.out[..made]).map_err(Error::Write)?;
        if used == 0 && made == 0 && status != Status::StreamEnd {
            return Err(Error::Write(std::io::Error::other("the deflater is stuck")));
        }
        Ok((used, status))
    }
}

/// A block's codec and layout, as [`Codecs::recode`] and
/// [`Codecs::check`] read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Recipe {
    pub(crate) codec: Codec,
    /// The bytes it decodes to.
    pub(crate) expected: u64,
    /// The bytes of each of its rows.
    pub(crate) row: u64,
    /// The predictor Grape adds, if any.
    pub(crate) predictor: Option<Predictor>,
}

/// The decoders and the deflater, kept from block to block.
#[derive(Default)]
pub(crate) struct Codecs {
    decoder: Decoder,
    encoder: Encoder,
    /// A test's broken encoder: the first byte of each step changes after
    /// it's counted.
    #[cfg(test)]
    pub(crate) broken: bool,
}

impl Codecs {
    /// Decodes `block` of `r` as `recipe` says, cut at its size, adds its
    /// predictor, and deflates it into `w`; gives the CRC32 of the bytes it
    /// decoded to, before the predictor. `tick` is told of each step's
    /// decoded bytes, and stops the job by failing.
    pub(crate) fn recode(
        &mut self,
        r: &mut dyn In,
        block: Block,
        recipe: Recipe,
        w: &mut dyn Write,
        tick: &mut dyn FnMut(u64) -> Result<(), Error>,
    ) -> Result<u32, Fail> {
        let Codecs {
            decoder, encoder, ..
        } = self;
        #[cfg(test)]
        let broken = self.broken;
        let mut crc = crc32fast::Hasher::new();
        let step = step(recipe.row, recipe.predictor.is_some());
        let row = usize::try_from(recipe.row).unwrap_or(usize::MAX).max(1);
        encoder.begin();
        decoder.decode(
            r,
            block,
            recipe.codec,
            recipe.expected,
            step,
            false,
            &mut |bytes| {
                crc.update(bytes);
                #[cfg(test)]
                if broken {
                    bytes[0] ^= 1;
                }
                if let Some(predictor) = recipe.predictor {
                    bytes
                        .chunks_exact_mut(row)
                        .for_each(|row| predictor.apply(row));
                }
                encoder.feed(bytes, w)?;
                tick(bytes.len() as u64)
            },
        )?;
        encoder.finish(w)?;
        Ok(crc.finalize())
    }

    /// Reads back `block`, which [`Codecs::recode`] wrote as `recipe` says:
    /// inflates it (it must end with its picture), undoes its predictor,
    /// and gives the CRC32 of what that comes to.
    pub(crate) fn check(
        &mut self,
        r: &mut dyn In,
        block: Block,
        recipe: Recipe,
    ) -> Result<u32, Fail> {
        let mut crc = crc32fast::Hasher::new();
        let step = step(recipe.row, recipe.predictor.is_some());
        let row = usize::try_from(recipe.row).unwrap_or(usize::MAX).max(1);
        self.decoder.decode(
            r,
            block,
            Codec::Deflate,
            recipe.expected,
            step,
            true,
            &mut |bytes| {
                if let Some(predictor) = recipe.predictor {
                    bytes
                        .chunks_exact_mut(row)
                        .for_each(|row| predictor.undo(row));
                }
                crc.update(bytes);
                Ok(())
            },
        )?;
        Ok(crc.finalize())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::io::{Cursor, Read};

    use super::super::read::Tiff;
    use super::super::tests::{gray, Builder, Page, B, V};
    use super::*;

    fn read(file: &[u8]) -> Tiff {
        Tiff::read(&mut Cursor::new(file), file.len() as u64).unwrap()
    }

    fn geometry(file: &[u8], ifd: &Ifd) -> Result<Geometry, Why> {
        let order = read(file).layout.order;
        let mut left = u64::MAX;
        Geometry::of(&mut Cursor::new(file), ifd, order, &mut left).unwrap()
    }

    /// Block `n` of `ifd` decoded as its compression says, cut at its size.
    fn decoded(file: &[u8], ifd: &Ifd, n: usize) -> Result<Vec<u8>, Fail> {
        let order = read(file).layout.order;
        let codec = Codec::of(ifd.uint(259, order).unwrap_or(1)).unwrap();
        let expected = geometry(file, ifd).unwrap().expected(n as u64);
        let mut out = Vec::new();
        Decoder::new().decode(
            &mut Cursor::new(file),
            ifd.blocks[n],
            codec,
            expected,
            STEP,
            false,
            &mut |bytes| {
                out.extend_from_slice(bytes);
                Ok(())
            },
        )?;
        Ok(out)
    }

    /// Samples the tiff crate decoded, as little-endian bytes.
    fn le(result: tiff::decoder::DecodingResult) -> Vec<u8> {
        use tiff::decoder::DecodingResult::*;
        match result {
            U8(v) => v,
            U16(v) => v.iter().flat_map(|x| x.to_le_bytes()).collect(),
            F32(v) => v.iter().flat_map(|x| x.to_le_bytes()).collect(),
            _ => panic!("another sample type"),
        }
    }

    fn undecodable<T: std::fmt::Debug>(result: Result<T, Fail>) -> String {
        match result {
            Err(Fail::Undecodable(why)) => why,
            other => panic!("{other:?}"),
        }
    }

    /// One page from the tiff crate: `compression`, `predictor`, strips of
    /// 7 rows of an RGB 8-bit or a gray 16-bit picture, 37 × 30.
    fn tiff_crate(
        compression: tiff::encoder::Compression,
        predictor: tiff::tags::Predictor,
        wide: bool,
    ) -> (Vec<u8>, Vec<u8>) {
        use tiff::encoder::{colortype, TiffEncoder};
        let mut out = Cursor::new(Vec::new());
        let e = TiffEncoder::new(&mut out)
            .unwrap()
            .with_compression(compression)
            .with_predictor(predictor);
        let mut e = e;
        let samples: Vec<u8> = if wide {
            let gray: Vec<u16> = (0..37 * 30).map(|i| (i * 977 % 65_521) as u16).collect();
            let mut image = e.new_image::<colortype::Gray16>(37, 30).unwrap();
            image.rows_per_strip(7).unwrap();
            image.write_data(&gray).unwrap();
            gray.iter().flat_map(|v| v.to_le_bytes()).collect()
        } else {
            let rgb: Vec<u8> = (0..37 * 30 * 3).map(|i| (i * 7 % 251) as u8).collect();
            let mut image = e.new_image::<colortype::RGB8>(37, 30).unwrap();
            image.rows_per_strip(7).unwrap();
            image.write_data(&rgb).unwrap();
            rgb
        };
        (out.into_inner(), samples)
    }

    #[test]
    fn every_codec_decodes_each_strip_as_the_tiff_crate_does() {
        use tiff::encoder::{Compression, DeflateLevel};
        use tiff::tags::Predictor as P;
        for (name, compression) in [
            ("none", Compression::Uncompressed),
            ("LZW", Compression::Lzw),
            ("Deflate", Compression::Deflate(DeflateLevel::Best)),
            ("PackBits", Compression::Packbits),
        ] {
            for wide in [false, true] {
                let (file, _) = tiff_crate(compression, P::None, wide);
                let tiff = read(&file);
                let page = &tiff.pages[0];
                assert_eq!(page.blocks.len(), 5);
                let mut decoder = tiff::decoder::Decoder::new(Cursor::new(&file)).unwrap();
                for n in 0..5 {
                    let ours = decoded(&file, page, n).unwrap();
                    let theirs = le(decoder.read_chunk(n as u32).unwrap());
                    assert_eq!(ours, theirs, "{name} strip {n}");
                    // The last strip holds the 2 rows left.
                    let rows = if n == 4 { 2 } else { 7 };
                    assert_eq!(ours.len(), rows * 37 * if wide { 2 } else { 3 });
                }
            }
        }
    }

    #[test]
    fn the_predictor_differences_as_the_tiff_crate_and_undoes_it() {
        use tiff::encoder::{Compression, DeflateLevel};
        use tiff::tags::Predictor as P;
        for (name, compression) in [
            ("LZW", Compression::Lzw),
            ("Deflate", Compression::Deflate(DeflateLevel::Fast)),
        ] {
            for wide in [false, true] {
                let (file, samples) = tiff_crate(compression, P::Horizontal, wide);
                let tiff = read(&file);
                let page = &tiff.pages[0];
                let g = geometry(&file, page).unwrap();
                let predictor = Predictor {
                    wide,
                    stride: if wide { 1 } else { 3 },
                    order: Order::Little,
                };
                let mut all = Vec::new();
                for n in 0..5 {
                    // The tiff crate's differenced bytes are ours.
                    let mut strip = decoded(&file, page, n).unwrap();
                    let start = all.len();
                    let mut plain = samples[start..start + strip.len()].to_vec();
                    for row in plain.chunks_exact_mut(g.row as usize) {
                        predictor.apply(row);
                    }
                    assert_eq!(strip, plain, "{name} strip {n}");
                    for row in strip.chunks_exact_mut(g.row as usize) {
                        predictor.undo(row);
                    }
                    all.extend(strip);
                }
                assert_eq!(all, samples);
            }
        }
    }

    #[test]
    fn the_predictor_works_in_the_files_byte_order() {
        // Differenced by hand, in both orders: 0x0100, 0x0105, 0x0003.
        let samples = [0x0100u16, 0x0105, 0x0003];
        for (order, bytes) in [
            (Order::Little, [0x00, 0x01, 0x05, 0x01, 0x03, 0x00]),
            (Order::Big, [0x01, 0x00, 0x01, 0x05, 0x00, 0x03]),
        ] {
            let p = Predictor {
                wide: true,
                stride: 1,
                order,
            };
            let mut row = bytes;
            p.apply(&mut row);
            let diffs: Vec<u16> = row
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| order.uint(b) as u16)
                .collect();
            assert_eq!(diffs, [0x0100, 0x0005, 0x0003u16.wrapping_sub(0x0105)]);
            p.undo(&mut row);
            assert_eq!(row, bytes);
            let back: Vec<u16> = row
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| order.uint(b) as u16)
                .collect();
            assert_eq!(back, samples);
        }
        // 8 bits, a pixel of 3 samples: each from the same sample before.
        let p = Predictor {
            wide: false,
            stride: 3,
            order: Order::Big,
        };
        let mut row = [10, 20, 30, 15, 18, 40, 0, 0, 0];
        p.apply(&mut row);
        assert_eq!(row, [10, 20, 30, 5, 254, 10, 241, 238, 216]);
        p.undo(&mut row);
        assert_eq!(row, [10, 20, 30, 15, 18, 40, 0, 0, 0]);
    }

    /// `samples` (a 16-bit gray picture, `w` × `h`) differenced in
    /// `order`, then compressed as `compression` says (5 or 8), one strip.
    fn mm_page(samples: &[u16], w: u32, h: u32, compression: u16) -> Vec<u8> {
        let p = Predictor {
            wide: true,
            stride: 1,
            order: Order::Big,
        };
        let mut bytes: Vec<u8> = samples.iter().flat_map(|v| v.to_be_bytes()).collect();
        for row in bytes.chunks_exact_mut(w as usize * 2) {
            p.apply(row);
        }
        let data = if compression == 5 {
            weezl::encode::Encoder::with_tiff_size_switch(weezl::BitOrder::Msb, 8)
                .encode(&bytes)
                .unwrap()
        } else {
            let mut z = flate2::write::ZlibEncoder::new(Vec::new(), Compression::fast());
            z.write_all(&bytes).unwrap();
            z.finish().unwrap()
        };
        let mut page = gray(w, h, h, vec![B::Data(data)]);
        page.tags[2] = (258, V::Short(vec![16]));
        page.tags.push((259, V::Short(vec![compression])));
        page.tags.push((317, V::Short(vec![2])));
        Builder::new(b'M', false, vec![page]).build()
    }

    #[test]
    fn a_big_endian_page_differenced_by_grape_reads_back_in_the_tiff_crate() {
        let samples: Vec<u16> = (0..23 * 9).map(|i| (i * 2_999 % 65_521) as u16).collect();
        for compression in [5, 8] {
            let file = mm_page(&samples, 23, 9, compression);
            let mut decoder = tiff::decoder::Decoder::new(Cursor::new(&file)).unwrap();
            let tiff::decoder::DecodingResult::U16(back) = decoder.read_image().unwrap() else {
                panic!("not 16-bit")
            };
            assert_eq!(back, samples, "compression {compression}");
        }
    }

    /// A gray page of one strip of `data`, `w` × `h`, `compression`.
    fn one_strip(w: u32, h: u32, compression: u16, data: Vec<u8>) -> Vec<u8> {
        let mut page = gray(w, h, h, vec![B::Data(data)]);
        page.tags.push((259, V::Short(vec![compression])));
        Builder::new(b'I', false, vec![page]).build()
    }

    #[test]
    fn packbits_reads_literals_runs_and_no_ops_and_stops_at_the_size() {
        // "ab", 4 × 'c', a no-op, 3 × 'd' then a literal past the picture.
        let data = vec![
            1, b'a', b'b', 0xFD, b'c', 0x80, 0xFE, b'd', 2, b'x', b'y', b'z',
        ];
        let file = one_strip(10, 1, 32773, data.clone());
        let page = &read(&file).pages[0];
        assert_eq!(decoded(&file, page, 0).unwrap(), b"abccccdddx");
        // The tiff crate reads it the same.
        let mut decoder = tiff::decoder::Decoder::new(Cursor::new(&file)).unwrap();
        assert_eq!(le(decoder.read_chunk(0).unwrap()), b"abccccdddx");
        // A run past the picture is cut too.
        let file = one_strip(5, 1, 32773, vec![1, b'a', b'b', 0x81, b'r']);
        assert_eq!(decoded(&file, &read(&file).pages[0], 0).unwrap(), b"abrrr");
        // Data that ends before the picture, mid-literal or before a run's
        // byte, can't be decoded.
        for short in [&[3, b'a', b'b'][..], &[1, b'a', b'b', 0xFE], &[0x80]] {
            let file = one_strip(10, 1, 32773, short.to_vec());
            assert!(undecodable(decoded(&file, &read(&file).pages[0], 0)).contains("ends"));
        }
    }

    #[test]
    fn data_short_of_the_picture_cant_be_decoded_and_data_past_it_is_cut() {
        let picture: Vec<u8> = (0..64).map(|i| (i * 13 % 256) as u8).collect();
        let lzw = |bytes: &[u8]| {
            weezl::encode::Encoder::with_tiff_size_switch(weezl::BitOrder::Msb, 8)
                .encode(bytes)
                .unwrap()
        };
        let zlib = |bytes: &[u8]| {
            let mut z = flate2::write::ZlibEncoder::new(Vec::new(), Compression::best());
            z.write_all(bytes).unwrap();
            z.finish().unwrap()
        };
        for compression in [1, 5, 8, 32946] {
            let code = |bytes: &[u8]| match compression {
                1 => bytes.to_vec(),
                5 => lzw(bytes),
                _ => zlib(bytes),
            };
            // More data than the picture: cut.
            let longer = [&picture[..], b"more bytes than the picture"].concat();
            let file = one_strip(8, 8, compression, code(&longer));
            assert_eq!(decoded(&file, &read(&file).pages[0], 0).unwrap(), picture);
            // Less: the stream ends early.
            let file = one_strip(8, 8, compression, code(&picture[..60]));
            let why = undecodable(decoded(&file, &read(&file).pages[0], 0));
            assert!(why.contains("ends before"), "{compression}: {why}");
            // Cut short: the data itself ends early.
            let whole = code(&picture);
            let file = one_strip(8, 8, compression, whole[..whole.len() - 8].to_vec());
            assert!(matches!(
                decoded(&file, &read(&file).pages[0], 0),
                Err(Fail::Undecodable(_))
            ));
        }
        // Junk isn't LZW or Deflate.
        for compression in [5, 8] {
            let file = one_strip(8, 8, compression, vec![0xFF; 40]);
            assert!(matches!(
                decoded(&file, &read(&file).pages[0], 0),
                Err(Fail::Undecodable(_))
            ));
        }
    }

    #[test]
    fn old_style_lzw_isnt_decoded() {
        // Its first code, a clear code (256), written LSB first.
        let file = one_strip(8, 8, 5, vec![0x00, 0x01, 0x02, 0x03, 0x04]);
        let why = undecodable(decoded(&file, &read(&file).pages[0], 0));
        assert!(why.contains("old-style"), "{why}");
    }

    #[test]
    fn the_decoded_bytes_go_by_a_step_at_a_time() {
        // 3 MiB and a bit, uncompressed and deflated: steps of 256 KiB, or
        // of whole 3,000-byte rows.
        let (w, h) = (3_000u32, 1_100u32);
        let picture: Vec<u8> = (0..w * h).map(|i| (i % 253) as u8).collect();
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), Compression::fast());
        z.write_all(&picture).unwrap();
        for (compression, data) in [(1, picture.clone()), (8, z.finish().unwrap())] {
            let file = one_strip(w, h, compression, data);
            let page = &read(&file).pages[0];
            let codec = Codec::of(compression.into()).unwrap();
            for (step, predicted) in [(STEP, false), (step(3_000, true), true)] {
                let mut sizes = Vec::new();
                let mut all = Vec::new();
                Decoder::new()
                    .decode(
                        &mut Cursor::new(&file),
                        page.blocks[0],
                        codec,
                        u64::from(w * h),
                        step,
                        false,
                        &mut |bytes| {
                            sizes.push(bytes.len());
                            all.extend_from_slice(bytes);
                            Ok(())
                        },
                    )
                    .unwrap();
                assert_eq!(all, picture);
                let (last, full) = sizes.split_last().unwrap();
                assert!(full.iter().all(|&n| n == step), "{sizes:?}");
                assert!(*last <= step && *last > 0);
                if predicted {
                    assert_eq!(step, 87 * 3_000);
                    assert!(sizes.iter().all(|n| n % 3_000 == 0));
                }
            }
        }
    }

    #[test]
    fn strips_tiles_and_planes_are_sized_as_libtiff_sizes_them() {
        let page = |tags: Vec<(u16, V)>, blocks: usize, tiled: bool| Page {
            tags,
            blocks: vec![B::data(b"x"); blocks],
            tiled,
            ..Page::default()
        };
        let sized = |p: Page| {
            let file = Builder::new(b'I', false, vec![p]).build();
            let tiff = read(&file);
            geometry(&file, &tiff.pages[0])
        };
        // RGB, 10 × 7, strips of 3 rows: 3 strips, the last of 1 row.
        let rgb = vec![
            (256, V::Long(vec![10])),
            (257, V::Long(vec![7])),
            (258, V::Short(vec![8, 8, 8])),
            (262, V::Short(vec![2])),
            (277, V::Short(vec![3])),
            (278, V::Long(vec![3])),
        ];
        let g = sized(page(rgb.clone(), 3, false)).unwrap();
        assert_eq!((g.blocks(), g.row), (3, 30));
        assert_eq!([0, 1, 2].map(|n| g.expected(n)), [90, 90, 30]);
        // Planar: each plane's strips, a row of one sample.
        let mut planar = rgb.clone();
        planar.push((284, V::Short(vec![2])));
        let g = sized(page(planar, 9, false)).unwrap();
        assert_eq!((g.blocks(), g.row, g.planar), (9, 10, true));
        assert_eq!(
            (0..9).map(|n| g.expected(n)).collect::<Vec<_>>(),
            [30, 30, 10, 30, 30, 10, 30, 30, 10]
        );
        // 1-bit, 10 wide: 2 bytes a row. 12-bit RGB: 45 bytes a row.
        let mut bilevel = rgb[..2].to_vec();
        bilevel.push((258, V::Short(vec![1])));
        let g = sized(page(bilevel, 1, false)).unwrap();
        assert_eq!((g.row, g.expected(0)), (2, 14));
        let mut twelve = rgb.clone();
        twelve[2] = (258, V::Short(vec![12, 12, 12]));
        assert_eq!(sized(page(twelve, 3, false)).unwrap().row, 45);
        // Tiles of 16 × 16 over 40 × 20: 3 × 2 of them, all full size.
        let tiles = vec![
            (256, V::Long(vec![40])),
            (257, V::Long(vec![20])),
            (258, V::Short(vec![16])),
            (262, V::Short(vec![1])),
            (322, V::Short(vec![16])),
            (323, V::Short(vec![16])),
        ];
        let g = sized(page(tiles, 6, true)).unwrap();
        assert_eq!((g.blocks(), g.row), (6, 32));
        assert!((0..6).all(|n| g.expected(n) == 512));
        // YCbCr subsampled 2 × 2 by default: 5 × 3 is 3 sampling units of
        // 6 samples a row, in 2 rows of them.
        let mut ycbcr = rgb.clone();
        ycbcr[0] = (256, V::Long(vec![5]));
        ycbcr[1] = (257, V::Long(vec![3]));
        ycbcr[3] = (262, V::Short(vec![6]));
        ycbcr[5] = (278, V::Long(vec![3]));
        let g = sized(page(ycbcr.clone(), 1, false)).unwrap();
        assert!(g.subsampled);
        assert_eq!((g.row, g.expected(0)), (18, 36));
        // 4 × 2: 2 units of 10 samples a row.
        let mut four = ycbcr.clone();
        four.push((530, V::Short(vec![4, 2])));
        let g = sized(page(four, 1, false)).unwrap();
        assert_eq!((g.row, g.expected(0)), (20, 40));
        // 1 × 1 is plain chunky YCbCr.
        let mut one = ycbcr.clone();
        one.push((530, V::Short(vec![1, 1])));
        let g = sized(page(one, 1, false)).unwrap();
        assert!(!g.subsampled);
        assert_eq!((g.row, g.expected(0)), (15, 45));
        // What can't be sized.
        let mut planes_apart = ycbcr.clone();
        planes_apart.push((284, V::Short(vec![2])));
        let mut odd = ycbcr.clone();
        odd.push((530, V::Short(vec![3, 1])));
        let mut mixed = rgb.clone();
        mixed[2] = (258, V::Short(vec![5, 6, 5]));
        let mut no_rows = rgb.clone();
        no_rows[5] = (278, V::Long(vec![0]));
        // 2^32 pixels of 65,535 samples of 64 bits, in a strip of 2^32
        // rows: 2^86 bytes.
        let mut huge = rgb.clone();
        huge[0] = (256, V::Long(vec![u32::MAX]));
        huge[1] = (257, V::Long(vec![u32::MAX]));
        huge[2] = (258, V::Short(vec![64]));
        huge[4] = (277, V::Short(vec![u16::MAX]));
        huge[5] = (278, V::Long(vec![u32::MAX]));
        for (tags, blocks, why) in [
            (planes_apart, 3, "subsampled"),
            (odd, 1, "YCbCrSubsampling"),
            (mixed, 3, "different sizes"),
            (no_rows, 3, "RowsPerStrip"),
            (huge, 1, "past any number"),
            (rgb[1..].to_vec(), 3, "no size"),
        ] {
            let got = sized(page(tags, blocks, false)).unwrap_err();
            assert!(got.contains(why), "{why}: {got}");
        }
    }

    #[test]
    fn sample_sizes_are_read_at_most_the_budget() {
        let mut page = gray(1, 1, 1, vec![B::data(b"x")]);
        page.tags[2] = (258, V::Short(vec![8; 4]));
        page.tags.push((277, V::Short(vec![4])));
        let file = Builder::new(b'I', false, vec![page]).build();
        let tiff = read(&file);
        let order = tiff.layout.order;
        let mut left = 8;
        let g = Geometry::of(&mut Cursor::new(&file), &tiff.pages[0], order, &mut left);
        assert_eq!(g.unwrap().unwrap().row, 4);
        assert_eq!(left, 0);
        let g = Geometry::of(&mut Cursor::new(&file), &tiff.pages[0], order, &mut left);
        assert!(matches!(g, Err(Error::TooLarge(_))), "{g:?}");
    }

    /// Recodes block 0 of `file`'s first page as `recipe` into a buffer at
    /// 1,000, past bytes the check must skip; gives its CRC, what the check
    /// gives, and the block written.
    fn round_trip(file: &[u8], recipe: Recipe, codecs: &mut Codecs) -> (u32, u32, Vec<u8>) {
        let page = &read(file).pages[0];
        let mut out = vec![0xAA; 1_000];
        let mut ticks = 0;
        let crc = codecs
            .recode(
                &mut Cursor::new(file),
                page.blocks[0],
                recipe,
                &mut out,
                &mut |n| {
                    ticks += n;
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(ticks, recipe.expected);
        let len = out.len() as u64 - 1_000;
        let block = Block { offset: 1_000, len };
        let back = codecs.check(&mut Cursor::new(&out), block, recipe).unwrap();
        (crc, back, out[1_000..].to_vec())
    }

    #[test]
    fn a_block_recoded_and_read_back_gives_the_same_crc() {
        let picture: Vec<u8> = (0..30 * 20 * 2).map(|i| (i * 7 % 256) as u8).collect();
        let lzw = weezl::encode::Encoder::with_tiff_size_switch(weezl::BitOrder::Msb, 8)
            .encode(&picture)
            .unwrap();
        let mut codecs = Codecs::default();
        for (compression, data) in [(1, picture.clone()), (5, lzw)] {
            let file = one_strip(60, 20, compression, data);
            for predictor in [
                None,
                Some(Predictor {
                    wide: true,
                    stride: 1,
                    order: Order::Little,
                }),
            ] {
                let recipe = Recipe {
                    codec: Codec::of(compression.into()).unwrap(),
                    expected: picture.len() as u64,
                    row: 60,
                    predictor,
                };
                let (crc, back, block) = round_trip(&file, recipe, &mut codecs);
                assert_eq!(crc, crc32fast::hash(&picture));
                assert_eq!(back, crc);
                // The block is zlib: the picture, differenced or not.
                let mut inflated = Vec::new();
                flate2::read::ZlibDecoder::new(&block[..])
                    .read_to_end(&mut inflated)
                    .unwrap();
                let mut want = picture.clone();
                if let Some(p) = predictor {
                    want.as_chunks_mut::<60>()
                        .0
                        .iter_mut()
                        .for_each(|row| p.apply(row));
                }
                assert_eq!(inflated, want);
                assert!(block.len() < picture.len());
            }
        }
        // A broken encoder is caught by the check.
        codecs.broken = true;
        let file = one_strip(60, 20, 1, picture.clone());
        let recipe = Recipe {
            codec: Codec::None,
            expected: picture.len() as u64,
            row: 60,
            predictor: None,
        };
        let (crc, back, _) = round_trip(&file, recipe, &mut codecs);
        assert_eq!(crc, crc32fast::hash(&picture));
        assert_ne!(back, crc);
    }

    #[test]
    fn the_check_wants_the_stream_to_end_with_the_picture() {
        let picture = vec![7u8; 100];
        let recipe = Recipe {
            codec: Codec::Deflate,
            expected: 100,
            row: 10,
            predictor: None,
        };
        let zlib = |bytes: &[u8]| {
            let mut z = flate2::write::ZlibEncoder::new(Vec::new(), Compression::best());
            z.write_all(bytes).unwrap();
            z.finish().unwrap()
        };
        let mut codecs = Codecs::default();
        let check = |codecs: &mut Codecs, file: &[u8]| {
            let block = Block {
                offset: 0,
                len: file.len() as u64,
            };
            codecs.check(&mut Cursor::new(file), block, recipe)
        };
        let exact = zlib(&picture);
        assert_eq!(
            check(&mut codecs, &exact).unwrap(),
            crc32fast::hash(&picture)
        );
        // A stream of more, or bytes after the stream, or one that doesn't
        // end.
        let longer = zlib(&[&picture[..], b"x"].concat());
        let trailing = [&exact[..], b"junk"].concat();
        let open = {
            let mut z = flate2::write::ZlibEncoder::new(Vec::new(), Compression::best());
            z.write_all(&picture).unwrap();
            z.flush().unwrap();
            z.get_ref().clone()
        };
        for (file, why) in [
            (longer, "holds more"),
            (trailing, "holds more"),
            (open, "doesn't end"),
        ] {
            let got = undecodable(check(&mut codecs, &file));
            assert!(got.contains(why), "{why}: {got}");
        }
    }

    #[test]
    fn a_tick_that_fails_stops_the_recode() {
        let picture = vec![1u8; 600_000];
        let file = one_strip(1_000, 600, 1, picture);
        let recipe = Recipe {
            codec: Codec::None,
            expected: 600_000,
            row: 1_000,
            predictor: None,
        };
        let page = &read(&file).pages[0];
        let mut ticks = 0;
        let result = Codecs::default().recode(
            &mut Cursor::new(&file),
            page.blocks[0],
            recipe,
            &mut Vec::new(),
            &mut |_| {
                ticks += 1;
                Err(Error::Write(std::io::Error::other("cancelled")))
            },
        );
        assert!(matches!(result, Err(Fail::Job(Error::Write(_)))));
        assert_eq!(ticks, 1);
    }
}
