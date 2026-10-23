//! I/O that stops once the job is cancelled.
//!
//! Decoders and encoders read and write as they go, so wrapping the source
//! reader and the temp file's writer makes a cancel take effect at their
//! next read or write, instead of after the whole library call.

use std::io::{self, Read, Seek, SeekFrom, Write};

use crate::CancelToken;

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

impl<T: Read> Read for Cancellable<T> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.check()?;
        self.inner.read(buf)
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
}
