//! Linux pipe I/O without tokio::io::stdin's uninterruptible blocking thread.
//! Reopen /proc/self/fd instead of changing the caller's shared OFD flags.
use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::OpenOptionsExt,
    pin::Pin,
    task::{Context, Poll, ready},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf, unix::AsyncFd};

enum Descriptor {
    Pollable(AsyncFd<File>),
    Regular(File),
}

impl Descriptor {
    fn open(number: u8, writing: bool) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(!writing)
            .write(writing)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(format!("/proc/self/fd/{number}"))?;
        if file.metadata()?.is_file() {
            return Ok(Self::Regular(file));
        }
        // SAFETY: we own the File and never close, replace or expose its fd
        // while registered. AsyncFd owns it until reactor deregistration.
        let registered = unsafe { AsyncFd::register(file) }.map_err(io::Error::from)?;
        Ok(Self::Pollable(registered))
    }
}

pub struct Input(Descriptor);
impl Input {
    pub fn new() -> io::Result<Self> {
        Ok(Self(Descriptor::open(0, false)?))
    }
}

impl AsyncRead for Input {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if buffer.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        let result = match &mut self.get_mut().0 {
            Descriptor::Regular(file) => file.read(buffer.initialize_unfilled()),
            Descriptor::Pollable(fd) => loop {
                let mut guard = ready!(fd.poll_read_ready(cx))?;
                match guard.try_io(|fd| fd.get_ref().read(buffer.initialize_unfilled())) {
                    Ok(result) => break result,
                    Err(_) => continue,
                }
            },
        };
        match result {
            Ok(count) => {
                buffer.advance(count);
                Poll::Ready(Ok(()))
            }
            Err(error) => Poll::Ready(Err(error)),
        }
    }
}

pub struct Output(Descriptor);
impl Output {
    pub fn new() -> io::Result<Self> {
        Ok(Self(Descriptor::open(1, true)?))
    }
}

impl AsyncWrite for Output {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        let result = match &mut self.get_mut().0 {
            Descriptor::Regular(file) => file.write(buffer),
            Descriptor::Pollable(fd) => loop {
                let mut guard = ready!(fd.poll_write_ready(cx))?;
                match guard.try_io(|fd| fd.get_ref().write(buffer)) {
                    Ok(result) => break result,
                    Err(_) => continue,
                }
            },
        };
        Poll::Ready(result)
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
