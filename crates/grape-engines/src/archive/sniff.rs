//! Which container a file really is, by its first bytes
//! (`FORMAT_MATRIX.md` rule 3 [Grape]: the extension can lie).

/// How many leading bytes [`sniff`] looks at: one tar header block.
pub(crate) const HEAD: usize = 512;

/// The longest empty tar [`is_empty_tar`] knows: GNU tar's default record
/// (20 blocks). The tar crate writes 1024 bytes.
pub(crate) const EMPTY_TAR_MAX: usize = 10_240;

/// The archive containers Grape reads in this slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Container {
    /// `PK\x03\x04`, or `PK\x05\x06` for an empty ZIP.
    Zip,
    /// `1F 8B`.
    Gzip,
    /// A tar header block: `ustar` at offset 257, or an old v7 header with
    /// a valid checksum.
    Tar,
}

/// The container `head` (a file's first [`HEAD`] bytes, or all of a
/// shorter file) starts, if any.
pub(crate) fn sniff(head: &[u8]) -> Option<Container> {
    if head.starts_with(b"PK\x03\x04") || head.starts_with(b"PK\x05\x06") {
        Some(Container::Zip)
    } else if head.starts_with(&[0x1F, 0x8B]) {
        Some(Container::Gzip)
    } else if is_tar_header(head) {
        Some(Container::Tar)
    } else {
        None
    }
}

/// True when `block` starts with a tar header: a non-empty name and a valid
/// header checksum (summed unsigned or, as some old tars did, signed). That
/// covers POSIX and GNU headers (`ustar` at offset 257) and old v7 ones
/// (no magic). An all-zero block (an empty archive's end marker) is not a
/// header: an empty tar is found by [`is_empty_tar`] instead.
pub(crate) fn is_tar_header(block: &[u8]) -> bool {
    let Some(block) = block.get(..HEAD) else {
        return false;
    };
    if block[0] == 0 {
        return false;
    }
    let Some(stored) = octal(&block[148..156]) else {
        return false;
    };
    let mut unsigned: u64 = 0;
    let mut signed: i64 = 0;
    for (i, &byte) in block.iter().enumerate() {
        // The checksum field counts as eight spaces.
        let byte = if (148..156).contains(&i) { b' ' } else { byte };
        unsigned += u64::from(byte);
        signed += i64::from(i8::from_ne_bytes([byte]));
    }
    stored == unsigned || i64::try_from(stored).is_ok_and(|s| s == signed)
}

/// True for all of an empty tar: only zero bytes, whole 512-byte blocks,
/// at least the two-block end marker and at most [`EMPTY_TAR_MAX`]. Its
/// first block isn't a header, so [`sniff`] can't tell; the caller checks
/// the whole file (or the whole decompressed stream) with this.
pub(crate) fn is_empty_tar(bytes: &[u8]) -> bool {
    (2 * HEAD..=EMPTY_TAR_MAX).contains(&bytes.len())
        && bytes.len().is_multiple_of(HEAD)
        && is_zeros(bytes)
}

/// True when every byte is zero.
pub(crate) fn is_zeros(bytes: &[u8]) -> bool {
    bytes.iter().all(|&b| b == 0)
}

/// An octal number field: optional leading spaces, at least one octal
/// digit, then only spaces or NULs.
fn octal(field: &[u8]) -> Option<u64> {
    let start = field.iter().position(|&b| b != b' ')?;
    let digits = field[start..]
        .iter()
        .take_while(|b| (b'0'..=b'7').contains(*b))
        .count();
    if digits == 0 || !field[start + digits..].iter().all(|&b| b == b' ' || b == 0) {
        return None;
    }
    field[start..start + digits].iter().try_fold(0u64, |n, &b| {
        n.checked_mul(8)?.checked_add(u64::from(b - b'0'))
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn tar_block(ustar: bool) -> Vec<u8> {
        let mut header = if ustar {
            tar::Header::new_ustar()
        } else {
            tar::Header::new_old()
        };
        header.set_path("a.txt").unwrap();
        header.set_size(3);
        header.set_mode(0o644);
        header.set_cksum();
        header.as_bytes().to_vec()
    }

    #[test]
    fn sniffs_each_container() {
        assert_eq!(sniff(b"PK\x03\x04rest"), Some(Container::Zip));
        assert_eq!(sniff(b"PK\x05\x06"), Some(Container::Zip));
        assert_eq!(sniff(&[0x1F, 0x8B, 8, 0]), Some(Container::Gzip));
        assert_eq!(sniff(&tar_block(true)), Some(Container::Tar));
        assert_eq!(sniff(&tar_block(false)), Some(Container::Tar));
        // A GNU header has "ustar  " at 257 too.
        let mut gnu = tar::Header::new_gnu();
        gnu.set_path("b").unwrap();
        gnu.set_cksum();
        assert_eq!(sniff(gnu.as_bytes()), Some(Container::Tar));
    }

    #[test]
    fn rejects_what_is_no_archive() {
        assert_eq!(sniff(b""), None);
        assert_eq!(sniff(b"PK"), None);
        assert_eq!(sniff(b"hello, world"), None);
        assert_eq!(sniff(&[0u8; 1024]), None);
        // A header with a broken checksum.
        let mut block = tar_block(true);
        block[0] = b'b';
        assert_eq!(sniff(&block), None);
        // Too short to be a header.
        assert_eq!(sniff(&tar_block(true)[..511]), None);
    }

    #[test]
    fn knows_an_empty_tar_by_all_of_it() {
        assert!(is_empty_tar(&[0u8; 1024]));
        assert!(is_empty_tar(&[0u8; 10_240]));
        assert!(!is_empty_tar(&[0u8; 512]));
        assert!(!is_empty_tar(&[0u8; 1000]));
        assert!(!is_empty_tar(&[0u8; 10_752]));
        let mut almost = vec![0u8; 2048];
        almost[2047] = 1;
        assert!(!is_empty_tar(&almost));
        let empty = tar::Builder::new(Vec::new()).into_inner().unwrap();
        assert!(is_empty_tar(&empty));
    }

    #[test]
    fn reads_octal_fields() {
        assert_eq!(octal(b"0000644\0"), Some(0o644));
        assert_eq!(octal(b"  12345 "), Some(0o12345));
        assert_eq!(octal(b"        "), None);
        assert_eq!(octal(b"12x45\0\0\0"), None);
        assert_eq!(octal(b"9\0\0\0\0\0\0\0"), None);
    }
}
