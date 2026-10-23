//! I/O that stops once the job is cancelled.
//!
//! Decoders and encoders read and write as they go, so wrapping the source
//! reader and the temp file's writer makes a cancel take effect at their
//! next read or write, instead of after the whole library call.

use std::io::{self, Read, Seek, SeekFrom, Write};

use crate::CancelToken;

/// The most [`Cancellable::read_to_end`] reads between cancel checks.
const READ_STEP: usize = 64 * 1024;

/// `inner`, with every read, write and flush failing once the token is
/// cancelled. Seeking is passed through.
#[derive(Debug)]
pub(crate) struct Cancellable<T> {
    inner: T,
    cancel: CancelToken,
}

impl<T> Cancellable<T> {
    /// Wraps `inner`; `cancel` is shared with the caller's token.
    pub(crate) fn new(inner: T, cancel: &CancelToken) -> Self {
        Cancellable {
            inner,
            cancel: cancel.clone(),
        }
    }

    /// The wrapped value.
    pub(crate) fn into_inner(self) -> T {
        self.inner
    }

    fn check(&self) -> io::Result<()> {
        if self.cancel.is_cancelled() {
            // Not `Interrupted`: `read_exact` and `write_all` retry that.
            Err(io::Error::other("cancelled"))
        } else {
            Ok(())
        }
    }
}

impl<T: Read + Seek> Read for Cancellable<T> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.check()?;
        self.inner.read(buf)
    }

    /// Reads the rest of the stream in steps of [`READ_STEP`], checking the
    /// token before each. What is left is reserved first, as
    /// `File::read_to_end` does: the default grows the buffer by doubling,
    /// so a decoder that reads a whole file into memory (JPEG) could hold
    /// up to twice its size.
    fn read_to_end(&mut self, buf: &mut Vec<u8>) -> io::Result<usize> {
        let here = self.inner.stream_position()?;
        let end = self.inner.seek(SeekFrom::End(0))?;
        self.inner.seek(SeekFrom::Start(here))?;
        let left = usize::try_from(end.saturating_sub(here))
            .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
        buf.try_reserve_exact(left)
            .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
        let start = buf.len();
        let mut step = [0u8; READ_STEP];
        loop {
            match self.read(&mut step) {
                Ok(0) => return Ok(buf.len() - start),
                Ok(n) => buf.extend_from_slice(&step[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }
}

impl<T: Write> Write for Cancellable<T> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.check()?;
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.check()?;
        self.inner.flush()
    }
}

impl<T: Seek> Seek for Cancellable<T> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.inner.seek(pos)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::io::Cursor;

    use super::*;

    #[test]
    fn reads_and_writes_stop_once_cancelled() {
        let token = CancelToken::new();
        let mut reader = Cancellable::new(Cursor::new(vec![1u8, 2, 3, 4]), &token);
        let mut writer = Cancellable::new(Cursor::new(Vec::new()), &token);
        let mut buf = [0u8; 2];
        reader.read_exact(&mut buf).unwrap();
        writer.write_all(&buf).unwrap();
        writer.flush().unwrap();

        token.cancel();

        assert!(reader.read_exact(&mut buf).is_err());
        assert!(writer.write_all(&buf).is_err());
        assert!(writer.flush().is_err());
        // Seeking still works, and nothing more was written.
        assert_eq!(reader.seek(SeekFrom::Start(0)).unwrap(), 0);
        assert_eq!(writer.into_inner().into_inner(), [1, 2]);
    }

    #[test]
    fn reading_to_the_end_holds_the_file_size_not_the_next_power_of_two() {
        // One byte past a power of two is the worst case: a buffer grown by
        // doubling (by std's probe read, or by appending step by step)
        // would end at twice the size.
        let bytes: Vec<u8> = (0..(1u32 << 20) + 1).map(|i| (i % 251) as u8).collect();
        let token = CancelToken::new();
        let mut reader = io::BufReader::new(Cancellable::new(Cursor::new(bytes.clone()), &token));
        // Like a decoder: sniff the first bytes, then seek back.
        let mut magic = [0u8; 4];
        reader.read_exact(&mut magic).unwrap();
        reader.seek(SeekFrom::Start(0)).unwrap();

        let mut all = Vec::new();
        assert_eq!(reader.read_to_end(&mut all).unwrap(), bytes.len());

        assert_eq!(all, bytes);
        assert!(
            all.capacity() < bytes.len() + 64 * 1024,
            "{}",
            all.capacity()
        );

        // Cancelled, it stops at the next step.
        let mut reader = Cancellable::new(Cursor::new(bytes), &token);
        token.cancel();
        assert!(reader.read_to_end(&mut Vec::new()).is_err());
    }
}
