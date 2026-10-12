//! Install progress observer: byte counts from the streaming fetch sink and
//! the coarse phases that follow the download. Observation only — an observer
//! can never fail or slow an install decision.

use std::io::{self, Write};

/// Post-download install phases, reported in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallPhase {
    BuildingRuntime,
    CheckingGpu,
}

pub trait InstallProgress {
    /// `bytes` were just written to disk by the streaming fetch sink. Called
    /// once per successful write, so the running sum is monotonic.
    fn downloaded(&self, _bytes: u64) {}
    fn phase(&self, _phase: InstallPhase) {}
}

/// Observer that ignores every event.
pub struct SilentProgress;

impl InstallProgress for SilentProgress {}

/// Forwards writes to `inner` and reports how many bytes each write accepted.
pub(crate) struct ProgressSink<'a, W: Write + ?Sized> {
    inner: &'a mut W,
    progress: &'a dyn InstallProgress,
}

impl<'a, W: Write + ?Sized> ProgressSink<'a, W> {
    pub(crate) fn new(inner: &'a mut W, progress: &'a dyn InstallProgress) -> Self {
        Self { inner, progress }
    }
}

impl<W: Write + ?Sized> Write for ProgressSink<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buf)?;
        if written > 0 {
            self.progress.downloaded(written as u64);
        }
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    struct Recorder(RefCell<Vec<u64>>);
    impl InstallProgress for Recorder {
        fn downloaded(&self, bytes: u64) {
            self.0.borrow_mut().push(bytes);
        }
    }

    #[test]
    fn sink_reports_only_bytes_the_inner_writer_accepted() {
        struct Short(Vec<u8>);
        impl Write for Short {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                let n = buf.len().min(3);
                self.0.extend_from_slice(&buf[..n]);
                Ok(n)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let recorder = Recorder::default();
        let mut inner = Short(Vec::new());
        ProgressSink::new(&mut inner, &recorder)
            .write_all(b"abcdefgh")
            .unwrap();
        assert_eq!(inner.0, b"abcdefgh");
        assert_eq!(*recorder.0.borrow(), vec![3, 3, 2]);
    }

    #[test]
    fn sink_reports_nothing_when_the_write_fails() {
        struct Full;
        impl Write for Full {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::from_raw_os_error(libc::ENOSPC))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let recorder = Recorder::default();
        assert!(ProgressSink::new(&mut Full, &recorder).write(b"x").is_err());
        assert!(recorder.0.borrow().is_empty());
    }
}
