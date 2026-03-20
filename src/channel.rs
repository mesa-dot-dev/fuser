use std::{
    fs::File,
    io,
    os::{
        fd::{AsFd, BorrowedFd},
        unix::prelude::AsRawFd,
    },
    sync::Arc,
};

use libc::{c_int, c_void, size_t};

#[cfg(feature = "abi-7-40")]
use crate::passthrough::BackingId;
use crate::reply::ReplySender;

// ---------------------------------------------------------------------------
// FUSE-T stream-based receive
// ---------------------------------------------------------------------------
//
// FUSE-T communicates over a stream socket instead of /dev/macfuseN.
// Messages are length-prefixed: [4-byte LE length][payload].
// The length includes the 4-byte header itself.
//
// A mutex serializes reads because the stream has no message boundaries
// and concurrent reads would interleave bytes.

#[cfg(fuse_t)]
use std::sync::Mutex;

#[cfg(fuse_t)]
static FUSE_T_READ_MUTEX: Mutex<()> = Mutex::new(());

/// Read exactly `buf.len()` bytes from `fd`, looping on partial reads.
#[cfg(fuse_t)]
fn read_exact_raw(fd: c_int, buf: &mut [u8]) -> io::Result<usize> {
    let mut total = 0;
    while total < buf.len() {
        let r = unsafe {
            libc::read(
                fd,
                buf[total..].as_mut_ptr() as *mut c_void,
                (buf.len() - total) as size_t,
            )
        };
        if r == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "FUSE-T: unexpected EOF while reading",
            ));
        } else if r < 0 {
            return Err(io::Error::last_os_error());
        } else {
            total += r as usize;
        }
    }
    Ok(total)
}

/// Receive a complete length-prefixed FUSE-T message from the stream socket.
#[cfg(fuse_t)]
fn receive_stream(fd: c_int, buffer: &mut [u8]) -> io::Result<usize> {
    let _guard = FUSE_T_READ_MUTEX
        .lock()
        .expect("FUSE-T read mutex poisoned");

    // Read the 4-byte little-endian length header.
    read_exact_raw(fd, &mut buffer[..4])?;
    let msg_len = u32::from_le_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]) as usize;

    if msg_len < 4 || msg_len > buffer.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("FUSE-T: invalid message length {msg_len}"),
        ));
    }

    // Read the remaining payload.
    read_exact_raw(fd, &mut buffer[4..msg_len])?;
    Ok(msg_len)
}

/// A raw communication channel to the FUSE kernel driver
#[derive(Debug)]
pub struct Channel(Arc<File>);

impl AsFd for Channel {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl Channel {
    /// Create a new communication channel to the kernel driver by mounting the
    /// given path. The kernel driver will delegate filesystem operations of
    /// the given path to the channel.
    pub(crate) fn new(device: Arc<File>) -> Self {
        Self(device)
    }

    /// Receives data up to the capacity of the given buffer (can block).
    pub fn receive(&self, buffer: &mut [u8]) -> io::Result<usize> {
        // FUSE-T uses a stream socket with length-prefixed messages.
        #[cfg(fuse_t)]
        {
            return receive_stream(self.0.as_raw_fd(), buffer);
        }

        // macFUSE / Linux: the kernel device delivers one complete message per read().
        #[cfg(not(fuse_t))]
        {
            let rc = unsafe {
                libc::read(
                    self.0.as_raw_fd(),
                    buffer.as_ptr() as *mut c_void,
                    buffer.len() as size_t,
                )
            };
            if rc < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(rc as usize)
            }
        }
    }

    /// Returns a sender object for this channel. The sender object can be
    /// used to send to the channel. Multiple sender objects can be used
    /// and they can safely be sent to other threads.
    pub fn sender(&self) -> ChannelSender {
        // Since write/writev syscalls are threadsafe, we can simply create
        // a sender by using the same file and use it in other threads.
        ChannelSender(self.0.clone())
    }
}

#[derive(Clone, Debug)]
pub struct ChannelSender(Arc<File>);

impl ReplySender for ChannelSender {
    fn send(&self, bufs: &[io::IoSlice<'_>]) -> io::Result<()> {
        let rc = unsafe {
            libc::writev(
                self.0.as_raw_fd(),
                bufs.as_ptr() as *const libc::iovec,
                bufs.len() as c_int,
            )
        };
        if rc < 0 {
            Err(io::Error::last_os_error())
        } else {
            debug_assert_eq!(bufs.iter().map(|b| b.len()).sum::<usize>(), rc as usize);
            Ok(())
        }
    }

    #[cfg(feature = "abi-7-40")]
    fn open_backing(&self, fd: BorrowedFd<'_>) -> std::io::Result<BackingId> {
        BackingId::create(&self.0, fd)
    }
}
