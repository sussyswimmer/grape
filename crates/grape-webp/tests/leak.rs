//! 1,000 encodes, through every way out of `encode`, leave nothing behind.
//! A test binary of its own, so no other test's memory moves the numbers.
//! The memory is read on Linux only (`/proc/self/statm`); elsewhere the loop
//! still runs, for its drop guards.

use grape_webp::{encode, Error, Mode, Options, Pixels};

const W: u32 = 192;
const H: u32 = 128;

/// Noise, so that each file is large (libwebp's writer grows its buffer to
/// hold it) and a leaked one shows.
fn picture() -> Vec<u8> {
    let mut x = 0x2545_f491_u32;
    (0..W * H * 4)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}

/// One round of every path: lossy and lossless (each allocates libwebp's
/// picture buffers and its writer), cancelled halfway, and a hook that
/// panics.
fn round(src: &[u8]) {
    let px = Pixels::Rgba {
        data: src,
        width: W,
        height: H,
    };
    let lossy = Options {
        mode: Mode::Lossy {
            quality: 75.0,
            method: 0,
        },
        threads: true,
    };
    let lossless = Options {
        mode: Mode::Lossless { level: 0 },
        threads: false,
    };
    assert!(encode(px, &lossy, &mut |_| true).is_ok());
    assert!(encode(px, &lossless, &mut |_| true).is_ok());
    assert_eq!(encode(px, &lossy, &mut |p| p < 50), Err(Error::Cancelled));
    assert_eq!(
        encode(px, &lossless, &mut |p| p < 50),
        Err(Error::Cancelled)
    );
    assert_eq!(
        encode(px, &lossy, &mut |_| panic!("on purpose")),
        Err(Error::Cancelled)
    );
}

/// Resident memory in bytes, where the OS says.
fn resident() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages * 4096)
}

#[test]
fn a_thousand_encodes_leak_nothing() {
    // The panicking hook would print 200 backtraces.
    std::panic::set_hook(Box::new(|_| {}));
    let src = picture();
    for _ in 0..50 {
        round(&src);
    }
    let before = resident();
    for _ in 50..200 {
        round(&src);
    }
    let after = resident();
    let _ = std::panic::take_hook();
    // 200 rounds are 1,000 encodes. Over the 150 measured, it grew by
    // nothing; without the picture's drop guard by 56 MiB, without the
    // writer's by 28 MiB (Linux, 2026-10-30).
    if let (Some(before), Some(after)) = (before, after) {
        let grew = after.saturating_sub(before);
        println!("grew {grew} bytes");
        assert!(grew < 4 << 20, "grew {grew} bytes");
    }
}
