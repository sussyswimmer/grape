use std::io::Cursor;

use super::*;

const LOSSY: Options = Options {
    mode: Mode::Lossy {
        quality: 80.0,
        method: 4,
    },
    threads: false,
};

const LOSSLESS: Options = Options {
    mode: Mode::Lossless { level: 6 },
    threads: false,
};

/// Smooth gradients, `channels` bytes a pixel. With 4, alpha runs from
/// opaque down to fully transparent, with a hole.
fn picture(width: u32, height: u32, channels: usize) -> Vec<u8> {
    let mut px = Vec::with_capacity(width as usize * height as usize * channels);
    for y in 0..height {
        for x in 0..width {
            let r = (x * 255 / width.max(2)) as u8;
            let g = (y * 255 / height.max(2)) as u8;
            let b = ((x + 2 * y) * 255 / (width + 2 * height)) as u8;
            px.extend_from_slice(&[r, g, b]);
            if channels == 4 {
                let hole = x > width / 3 && x < width / 2 && y > height / 3 && y < height / 2;
                px.push(if hole {
                    0
                } else {
                    (255 - (y * 255 / height.max(2))) as u8
                });
            }
        }
    }
    px
}

fn go(_: u8) -> bool {
    true
}

/// image-webp's decode: the size, whether it's lossy, and the pixels (RGBA
/// when the file has alpha).
fn decode(webp: &[u8]) -> ((u32, u32), bool, Vec<u8>) {
    let mut dec = image_webp::WebPDecoder::new(Cursor::new(webp)).unwrap();
    let size = dec.dimensions();
    let lossy = dec.is_lossy();
    let mut px = vec![0; dec.output_buffer_size().unwrap()];
    dec.read_image(&mut px).unwrap();
    (size, lossy, px)
}

/// The fourcc of each top-level chunk after `RIFF size WEBP`, checking the
/// RIFF size and the padding as it goes.
fn chunks(webp: &[u8]) -> Vec<String> {
    assert_eq!(&webp[..4], b"RIFF");
    assert_eq!(&webp[8..12], b"WEBP");
    let riff = u32::from_le_bytes(webp[4..8].try_into().unwrap()) as usize;
    assert_eq!(riff + 8, webp.len());
    let mut at = 12;
    let mut out = Vec::new();
    while at < webp.len() {
        let size = u32::from_le_bytes(webp[at + 4..at + 8].try_into().unwrap()) as usize;
        out.push(String::from_utf8(webp[at..at + 4].to_vec()).unwrap());
        at += 8 + size + (size & 1);
    }
    assert_eq!(at, webp.len());
    out
}

fn psnr(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len());
    let se: f64 = a
        .iter()
        .zip(b)
        .map(|(&x, &y)| (f64::from(x) - f64::from(y)).powi(2))
        .sum();
    10.0 * (255.0_f64.powi(2) / (se / a.len() as f64)).log10()
}

#[test]
fn lossy_rgb_is_a_plain_vp8_file_close_to_the_source() {
    let src = picture(97, 61, 3);
    let webp = encode(
        Pixels::Rgb {
            data: &src,
            width: 97,
            height: 61,
        },
        &LOSSY,
        &mut go,
    )
    .unwrap();
    assert_eq!(chunks(&webp), ["VP8 "]);
    let (size, lossy, px) = decode(&webp);
    assert_eq!(size, (97, 61));
    assert!(lossy);
    assert_eq!(px.len(), src.len());
    let db = psnr(&src, &px);
    assert!(db > 32.0, "PSNR {db:.1} dB");
}

#[test]
fn lossy_rgba_keeps_alpha_exactly() {
    let src = picture(80, 48, 4);
    for threads in [false, true] {
        let webp = encode(
            Pixels::Rgba {
                data: &src,
                width: 80,
                height: 48,
            },
            &Options { threads, ..LOSSY },
            &mut go,
        )
        .unwrap();
        assert_eq!(chunks(&webp), ["VP8X", "ALPH", "VP8 "]);
        let (size, lossy, px) = decode(&webp);
        assert_eq!(size, (80, 48));
        assert!(lossy);
        let alpha: Vec<u8> = px.iter().skip(3).step_by(4).copied().collect();
        let want: Vec<u8> = src.iter().skip(3).step_by(4).copied().collect();
        assert_eq!(alpha, want, "threads {threads}");
    }
}

#[test]
fn lossy_rgba_that_is_all_opaque_has_no_alpha() {
    let mut src = picture(32, 32, 4);
    src.iter_mut().skip(3).step_by(4).for_each(|a| *a = 255);
    let webp = encode(
        Pixels::Rgba {
            data: &src,
            width: 32,
            height: 32,
        },
        &LOSSY,
        &mut go,
    )
    .unwrap();
    assert_eq!(chunks(&webp), ["VP8 "]);
}

#[test]
fn lossless_rgb_and_rgba_come_back_exactly() {
    let rgb = picture(75, 40, 3);
    let rgba = picture(75, 40, 4);
    for level in [0, 9] {
        for threads in [false, true] {
            let opts = Options {
                mode: Mode::Lossless { level },
                threads,
            };
            let webp = encode(
                Pixels::Rgb {
                    data: &rgb,
                    width: 75,
                    height: 40,
                },
                &opts,
                &mut go,
            )
            .unwrap();
            assert_eq!(chunks(&webp), ["VP8L"]);
            let (size, lossy, px) = decode(&webp);
            assert_eq!(size, (75, 40));
            assert!(!lossy);
            assert_eq!(px, rgb, "level {level}, threads {threads}");

            let webp = encode(
                Pixels::Rgba {
                    data: &rgba,
                    width: 75,
                    height: 40,
                },
                &opts,
                &mut go,
            )
            .unwrap();
            assert_eq!(chunks(&webp), ["VP8L"]);
            // Every byte, the color under the transparent hole too (`exact`).
            let (_, _, px) = decode(&webp);
            assert_eq!(px, rgba, "level {level}, threads {threads}");
        }
    }
}

#[test]
fn lossless_keeps_the_color_under_transparent_pixels() {
    // Fully transparent, each pixel a different color.
    let src: Vec<u8> = (0..16 * 16)
        .flat_map(|i: u32| [i as u8, (i * 3) as u8, (i * 7) as u8, 0])
        .collect();
    let webp = encode(
        Pixels::Rgba {
            data: &src,
            width: 16,
            height: 16,
        },
        &LOSSLESS,
        &mut go,
    )
    .unwrap();
    assert_eq!(decode(&webp).2, src);
}

#[test]
fn one_pixel_and_the_longest_side_encode() {
    for (w, h) in [(1, 1), (MAX_SIDE, 1), (1, MAX_SIDE)] {
        let src = vec![200; w as usize * h as usize * 3];
        let px = Pixels::Rgb {
            data: &src,
            width: w,
            height: h,
        };
        for opts in [LOSSY, LOSSLESS] {
            let webp = encode(px, &opts, &mut go).unwrap();
            let (size, _, out) = decode(&webp);
            assert_eq!(size, (w, h));
            if opts == LOSSLESS {
                assert_eq!(out, src);
            }
        }
    }
}

#[test]
fn a_size_that_doesnt_match_the_data_is_bad_input() {
    let src = picture(10, 10, 4);
    let cases = [
        Pixels::Rgba {
            data: &src[..src.len() - 1],
            width: 10,
            height: 10,
        },
        Pixels::Rgba {
            data: &src,
            width: 10,
            height: 9,
        },
        Pixels::Rgb {
            data: &src,
            width: 10,
            height: 10,
        },
        Pixels::Rgba {
            data: &src,
            width: 0,
            height: 10,
        },
        Pixels::Rgb {
            data: &[],
            width: 10,
            height: 0,
        },
        Pixels::Rgb {
            data: &[],
            width: 0,
            height: 0,
        },
    ];
    for px in cases {
        for opts in [LOSSY, LOSSLESS] {
            assert_eq!(encode(px, &opts, &mut go), Err(Error::BadInput), "{px:?}");
        }
    }
    let mut extra = src.clone();
    extra.push(0);
    let px = Pixels::Rgba {
        data: &extra,
        width: 10,
        height: 10,
    };
    assert_eq!(encode(px, &LOSSY, &mut go), Err(Error::BadInput));
}

#[test]
fn numbers_out_of_range_are_bad_input() {
    let src = picture(8, 8, 3);
    let px = Pixels::Rgb {
        data: &src,
        width: 8,
        height: 8,
    };
    for mode in [
        Mode::Lossy {
            quality: 100.5,
            method: 4,
        },
        Mode::Lossy {
            quality: -0.5,
            method: 4,
        },
        Mode::Lossy {
            quality: f32::NAN,
            method: 4,
        },
        Mode::Lossy {
            quality: 80.0,
            method: 7,
        },
        Mode::Lossless { level: 10 },
    ] {
        let opts = Options {
            mode,
            threads: false,
        };
        assert_eq!(encode(px, &opts, &mut go), Err(Error::BadInput), "{mode:?}");
    }
    // The ends of each range are fine.
    for mode in [
        Mode::Lossy {
            quality: 0.0,
            method: 0,
        },
        Mode::Lossy {
            quality: 100.0,
            method: 6,
        },
    ] {
        let opts = Options {
            mode,
            threads: true,
        };
        assert!(encode(px, &opts, &mut go).is_ok(), "{mode:?}");
    }
}

#[test]
fn a_side_over_16383_is_too_large() {
    let src = vec![0; (MAX_SIDE as usize + 1) * 3];
    for (w, h) in [(MAX_SIDE + 1, 1), (1, MAX_SIDE + 1)] {
        let px = Pixels::Rgb {
            data: &src,
            width: w,
            height: h,
        };
        assert_eq!(encode(px, &LOSSY, &mut go), Err(Error::TooLarge));
        assert_eq!(encode(px, &LOSSLESS, &mut go), Err(Error::TooLarge));
    }
    // Before the data is looked at: no buffer this size is needed.
    let px = Pixels::Rgba {
        data: &[],
        width: u32::MAX,
        height: u32::MAX,
    };
    assert_eq!(encode(px, &LOSSY, &mut go), Err(Error::TooLarge));
}

#[test]
fn progress_rises_to_the_end() {
    let src = picture(128, 96, 4);
    for opts in [LOSSY, LOSSLESS] {
        let mut seen = Vec::new();
        encode(
            Pixels::Rgba {
                data: &src,
                width: 128,
                height: 96,
            },
            &opts,
            &mut |p| {
                seen.push(p);
                true
            },
        )
        .unwrap();
        assert!(seen.len() >= 3, "{seen:?}");
        assert!(seen.windows(2).all(|w| w[0] < w[1]), "{seen:?}");
        // libwebp reports a lossy encode's 100 only when it's built without
        // its statistics.
        let last = seen.last().copied().unwrap_or(0);
        assert!(last >= 90, "{seen:?}");
        if opts == LOSSLESS {
            assert_eq!(last, 100, "{seen:?}");
        }
    }
}

#[test]
fn a_hook_that_says_stop_cancels_and_isnt_called_again() {
    let src = picture(128, 96, 4);
    let px = Pixels::Rgba {
        data: &src,
        width: 128,
        height: 96,
    };
    for opts in [LOSSY, LOSSLESS] {
        // At once, and part of the way through.
        for stop_at in [0, 30] {
            let mut calls = Vec::new();
            let got = encode(px, &opts, &mut |p| {
                calls.push(p);
                p < stop_at
            });
            assert_eq!(got, Err(Error::Cancelled), "{opts:?} at {stop_at}");
            let stopped = calls.iter().filter(|&&p| p >= stop_at).count();
            assert_eq!(stopped, 1, "{opts:?}: {calls:?}");
            assert!(calls.last().is_some_and(|&p| p >= stop_at), "{calls:?}");
        }
    }
}

#[test]
fn a_panicking_hook_cancels_without_unwinding_through_libwebp() {
    let src = picture(64, 64, 3);
    let px = Pixels::Rgb {
        data: &src,
        width: 64,
        height: 64,
    };
    for opts in [LOSSY, LOSSLESS] {
        let mut calls = 0;
        let got = encode(px, &opts, &mut |_| {
            calls += 1;
            panic!("the hook's own bug");
        });
        assert_eq!(got, Err(Error::Cancelled));
        assert_eq!(calls, 1);
        // And the next encode is fine.
        assert!(encode(px, &opts, &mut go).is_ok());
    }
}

#[test]
fn the_version_is_1_6_0() {
    assert_eq!(version(), (1, 6, 0));
}

#[test]
fn errors_read_as_grape_writes_them() {
    assert_eq!(Error::TooLarge.to_string(), "over 16,383 px a side");
    assert_eq!(Error::Encoder(6).to_string(), "libwebp error 6");
}
