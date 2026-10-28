//! Flate streams for images → PDF (D37): rows filtered with PNG predictors
//! (`/Predictor 15`: each row starts with its filter type, chosen per row)
//! and deflated with `flate2` into a [`Sink`] reserved at the zlib bound,
//! and 1-bit packing for pure black-and-white gray.

use std::io::{self, Write};

use flate2::write::ZlibEncoder;
use flate2::Compression;

use super::writer::Sink;
use crate::CancelToken;

/// How many rows are filtered between cancel checks.
pub(crate) const CANCEL_ROWS: u32 = 64;

/// Why a stream couldn't be built.
#[derive(Debug)]
pub(crate) enum FlateError {
    /// Its buffer couldn't be reserved.
    Memory,
    /// The job was cancelled.
    Cancelled,
    /// Deflate went past the bound (it never should: stored blocks bound
    /// it). The detail.
    Over(String),
}

/// The most bytes zlib turns `raw` bytes into: stored blocks add 5 bytes
/// per 64 KiB, the header and checksum 6; this allows more than twice
/// that.
pub(crate) fn bound(raw: u64) -> u64 {
    raw.saturating_add(raw / 1024).saturating_add(256)
}

/// The length of one row of `width` pixels of `channels` samples at
/// `bits` per sample, padded to a byte.
pub(crate) fn row_bytes(width: u32, channels: u8, bits: u8) -> u64 {
    (u64::from(width) * u64::from(channels) * u64::from(bits)).div_ceil(8)
}

/// What [`deflate_rows`] filters and compresses: `height` rows of
/// `row_len` bytes, with `bpp` bytes per pixel for the predictors (1 for
/// fewer than 8 bits a pixel).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Rows {
    pub(crate) height: u32,
    pub(crate) row_len: usize,
    pub(crate) bpp: usize,
}

impl Rows {
    /// The filtered bytes: a filter-type byte per row, then the row.
    pub(crate) fn filtered_len(&self) -> u64 {
        u64::from(self.height).saturating_mul(self.row_len as u64 + 1)
    }
}

/// Filters each row `fill(y, row)` writes, picking per row the PNG filter
/// whose output has the smallest sum of absolute values (as signed bytes),
/// and deflates the result into a buffer reserved at [`bound`]. Checks
/// `cancel` every [`CANCEL_ROWS`] rows and at every write.
pub(crate) fn deflate_rows(
    rows: Rows,
    cancel: &CancelToken,
    mut fill: impl FnMut(u32, &mut [u8]),
) -> Result<Vec<u8>, FlateError> {
    let cap = usize::try_from(bound(rows.filtered_len())).map_err(|_| FlateError::Memory)?;
    let sink = Sink::with_cap(cap, cancel).map_err(|()| FlateError::Memory)?;
    let mut encoder = ZlibEncoder::new(sink, Compression::default());
    let mut prev = vec![0u8; rows.row_len];
    let mut cur = vec![0u8; rows.row_len];
    let mut filtered: [Vec<u8>; 5] = std::array::from_fn(|_| vec![0u8; rows.row_len + 1]);
    for y in 0..rows.height {
        if y % CANCEL_ROWS == 0 && cancel.is_cancelled() {
            return Err(FlateError::Cancelled);
        }
        fill(y, &mut cur);
        let best = choose(&cur, &prev, rows.bpp, &mut filtered);
        encoder
            .write_all(&filtered[best])
            .map_err(|e| failed(e, cancel))?;
        std::mem::swap(&mut prev, &mut cur);
    }
    let sink = encoder.finish().map_err(|e| failed(e, cancel))?;
    Ok(sink.into_inner())
}

/// `data` deflated, without predictors (an ICC profile).
pub(crate) fn deflate(data: &[u8], cancel: &CancelToken) -> Result<Vec<u8>, FlateError> {
    let cap = usize::try_from(bound(data.len() as u64)).map_err(|_| FlateError::Memory)?;
    let sink = Sink::with_cap(cap, cancel).map_err(|()| FlateError::Memory)?;
    let mut encoder = ZlibEncoder::new(sink, Compression::default());
    encoder.write_all(data).map_err(|e| failed(e, cancel))?;
    let sink = encoder.finish().map_err(|e| failed(e, cancel))?;
    Ok(sink.into_inner())
}

fn failed(e: io::Error, cancel: &CancelToken) -> FlateError {
    if cancel.is_cancelled() {
        FlateError::Cancelled
    } else {
        FlateError::Over(format!("{:?}", e.kind()))
    }
}

/// Fills `out[k]` with `cur` under PNG filter `k` (its type byte first) for
/// every `k`, and returns the `k` whose filtered bytes have the smallest
/// sum of absolute values as signed bytes (the lowest `k` on a tie).
pub(crate) fn choose(cur: &[u8], prev: &[u8], bpp: usize, out: &mut [Vec<u8>; 5]) -> usize {
    let mut best = (0usize, u64::MAX);
    for (kind, buf) in out.iter_mut().enumerate() {
        let kind_byte = u8::try_from(kind).unwrap_or(0);
        filter(kind_byte, cur, prev, bpp, buf);
        let sum: u64 = buf[1..]
            .iter()
            .map(|&b| u64::from(if b < 128 { b } else { b.wrapping_neg() }))
            .sum();
        if sum < best.1 {
            best = (kind, sum);
        }
    }
    best.0
}

/// `cur` under PNG filter `kind` (0 None, 1 Sub, 2 Up, 3 Average, 4
/// Paeth), with `prev` the row above (zeros for the first), into `out`:
/// the type byte, then `cur.len()` filtered bytes.
pub(crate) fn filter(kind: u8, cur: &[u8], prev: &[u8], bpp: usize, out: &mut [u8]) {
    out[0] = kind;
    let out = &mut out[1..];
    let n = cur.len();
    let left = |row: &[u8], i: usize| if i >= bpp { row[i - bpp] } else { 0 };
    match kind {
        0 => out[..n].copy_from_slice(cur),
        1 => {
            for i in 0..n {
                out[i] = cur[i].wrapping_sub(left(cur, i));
            }
        }
        2 => {
            for i in 0..n {
                out[i] = cur[i].wrapping_sub(prev[i]);
            }
        }
        3 => {
            for i in 0..n {
                let mean = (u16::from(left(cur, i)) + u16::from(prev[i])) / 2;
                out[i] = cur[i].wrapping_sub(u8::try_from(mean).unwrap_or(u8::MAX));
            }
        }
        _ => {
            for i in 0..n {
                out[i] = cur[i].wrapping_sub(paeth(left(cur, i), prev[i], left(prev, i)));
            }
        }
    }
}

/// The PNG Paeth predictor.
fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let (ia, ib, ic) = (i16::from(a), i16::from(b), i16::from(c));
    let p = ia + ib - ic;
    let (pa, pb, pc) = ((p - ia).abs(), (p - ib).abs(), (p - ic).abs());
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// Packs a row of gray samples that are each 0 or 255 into 1 bit a
/// sample, most significant bit first, 1 for white; the last byte is
/// padded with zeros.
pub(crate) fn pack_bits(gray: &[u8], out: &mut [u8]) {
    out.fill(0);
    for (i, &v) in gray.iter().enumerate() {
        if v != 0 {
            out[i / 8] |= 0x80 >> (i % 8);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::Read;

    use flate2::read::ZlibDecoder;

    use super::*;

    /// Reverses [`filter`]: the row from its filtered bytes and the row
    /// above.
    pub(crate) fn unfilter(filtered: &[u8], prev: &[u8], bpp: usize) -> Vec<u8> {
        let kind = filtered[0];
        let data = &filtered[1..];
        let mut row = vec![0u8; data.len()];
        for i in 0..data.len() {
            let a = if i >= bpp { row[i - bpp] } else { 0 };
            let b = prev[i];
            let c = if i >= bpp { prev[i - bpp] } else { 0 };
            let predicted = match kind {
                0 => 0,
                1 => a,
                2 => b,
                3 => u8::try_from((u16::from(a) + u16::from(b)) / 2).unwrap(),
                _ => paeth(a, b, c),
            };
            row[i] = data[i].wrapping_add(predicted);
        }
        row
    }

    /// A small deterministic generator, so the tests need no crate.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }
    }

    #[test]
    fn every_filter_round_trips_on_random_rows() {
        let mut rng = Lcg(7);
        for _ in 0..200 {
            let bpp = [1usize, 2, 3, 4][usize::try_from(rng.next() % 4).unwrap()];
            let len = usize::try_from(rng.next() % 50).unwrap() * bpp + bpp;
            let row = |rng: &mut Lcg| -> Vec<u8> {
                (0..len)
                    .map(|_| u8::try_from(rng.next() % 256).unwrap())
                    .collect()
            };
            let prev = row(&mut rng);
            let cur = row(&mut rng);
            for kind in 0..5u8 {
                let mut out = vec![0u8; len + 1];
                filter(kind, &cur, &prev, bpp, &mut out);
                assert_eq!(out[0], kind);
                assert_eq!(
                    unfilter(&out, &prev, bpp),
                    cur,
                    "kind {kind} bpp {bpp} len {len}"
                );
            }
        }
    }

    #[test]
    fn the_filter_with_the_smallest_signed_sum_wins() {
        let mut out: [Vec<u8>; 5] = std::array::from_fn(|_| vec![0u8; 5]);
        // A ramp: Sub turns it into ones.
        assert_eq!(choose(&[10, 20, 30, 40], &[0; 4], 1, &mut out), 1);
        // The same row again below: Up turns it into zeros.
        assert_eq!(choose(&[10, 20, 30, 40], &[10, 20, 30, 40], 1, &mut out), 2);
        // All zeros: None, the lowest on a tie.
        assert_eq!(choose(&[0; 4], &[0; 4], 1, &mut out), 0);
        // 255 is -1 as a signed byte, so it counts as 1, not 255: None's
        // 4 beats Sub's 7.
        assert_eq!(choose(&[255, 1, 255, 1], &[0; 4], 1, &mut out), 0);
    }

    #[test]
    fn deflated_rows_inflate_and_unfilter_to_the_rows() {
        let mut rng = Lcg(11);
        let (width, height, bpp) = (37usize, 70u32, 3usize);
        let rows: Vec<Vec<u8>> = (0..height)
            .map(|y| {
                (0..width * bpp)
                    .map(|x| {
                        if y % 3 == 0 {
                            u8::try_from(rng.next() % 256).unwrap()
                        } else {
                            u8::try_from((x + usize::try_from(y).unwrap()) % 256).unwrap()
                        }
                    })
                    .collect()
            })
            .collect();
        let shape = Rows {
            height,
            row_len: width * bpp,
            bpp,
        };
        let stream = deflate_rows(shape, &CancelToken::new(), |y, row| {
            row.copy_from_slice(&rows[usize::try_from(y).unwrap()]);
        })
        .unwrap();
        let mut filtered = Vec::new();
        ZlibDecoder::new(&stream[..])
            .read_to_end(&mut filtered)
            .unwrap();
        assert_eq!(filtered.len() as u64, shape.filtered_len());
        let mut prev = vec![0u8; width * bpp];
        for (y, chunk) in filtered.chunks(width * bpp + 1).enumerate() {
            let row = unfilter(chunk, &prev, bpp);
            assert_eq!(row, rows[y], "row {y}");
            prev = row;
        }
    }

    #[test]
    fn noise_stays_within_the_bound() {
        let mut rng = Lcg(3);
        let noise: Vec<u8> = (0..300_000)
            .map(|_| u8::try_from(rng.next() % 256).unwrap())
            .collect();
        let shape = Rows {
            height: 100,
            row_len: 3000,
            bpp: 3,
        };
        let stream = deflate_rows(shape, &CancelToken::new(), |y, row| {
            let at = usize::try_from(y).unwrap() * 3000;
            row.copy_from_slice(&noise[at..at + 3000]);
        })
        .unwrap();
        assert!(stream.len() as u64 <= bound(shape.filtered_len()));
        let profile = deflate(&noise, &CancelToken::new()).unwrap();
        assert!(profile.len() as u64 <= bound(noise.len() as u64));
    }

    #[test]
    fn a_cancel_stops_the_rows() {
        let cancel = CancelToken::new();
        cancel.cancel();
        let shape = Rows {
            height: 10,
            row_len: 4,
            bpp: 1,
        };
        assert!(matches!(
            deflate_rows(shape, &cancel, |_, _| {}),
            Err(FlateError::Cancelled)
        ));
    }

    #[test]
    fn bits_pack_most_significant_first_with_white_as_one() {
        for width in [1usize, 7, 8, 9, 15, 16, 17, 33] {
            let gray: Vec<u8> = (0..width)
                .map(|i| if i % 3 == 0 { 255 } else { 0 })
                .collect();
            let mut packed = vec![0xAAu8; width.div_ceil(8)];
            pack_bits(&gray, &mut packed);
            assert_eq!(
                packed.len() as u64,
                row_bytes(u32::try_from(width).unwrap(), 1, 1)
            );
            for (i, &v) in gray.iter().enumerate() {
                let bit = packed[i / 8] >> (7 - i % 8) & 1;
                assert_eq!(bit == 1, v == 255, "width {width} sample {i}");
            }
            // The padding is zero.
            for i in width..packed.len() * 8 {
                assert_eq!(packed[i / 8] >> (7 - i % 8) & 1, 0, "width {width} pad {i}");
            }
        }
    }
}
