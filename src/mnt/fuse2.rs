use std::ffi::CString;
use std::fs::File;
use std::io;
#[cfg(target_os = "macos")]
use std::os::fd::BorrowedFd;
#[cfg(not(target_os = "macos"))]
use std::os::unix::prelude::FromRawFd;
use std::os::unix::prelude::OsStrExt;
use std::path::Path;
use std::sync::Arc;

use crate::SessionACL;
use crate::dev_fuse::DevFuse;
use crate::mnt::MountOption;
use crate::mnt::fuse2_sys::*;
use crate::mnt::with_fuse_args;

/// Ensures that an os error is never 0/Success
fn ensure_last_os_error() -> io::Error {
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        Some(0) => io::Error::new(io::ErrorKind::Other, "Unspecified Error"),
        _ => err,
    }
}

#[derive(Debug)]
pub(crate) struct MountImpl {
    mountpoint: CString,
    #[cfg(target_os = "macos")]
    channel: std::ptr::NonNull<libc::c_void>,
}
impl MountImpl {
    pub(crate) fn new(
        mountpoint: &Path,
        options: &[MountOption],
        acl: SessionACL,
    ) -> io::Result<(Arc<DevFuse>, MountImpl)> {
        let mountpoint = CString::new(mountpoint.as_os_str().as_bytes()).unwrap();
        with_fuse_args(options, acl, |args| {
            #[cfg(target_os = "macos")]
            {
                let mut args = fuse_args {
                    argc: args.argc,
                    argv: args.argv,
                    allocated: 0,
                };
                let channel = unsafe { fuse_mount(mountpoint.as_ptr(), &mut args) };
                let error = ensure_last_os_error();
                unsafe { fuse_opt_free_args(&mut args) };
                let channel = std::ptr::NonNull::new(channel).ok_or(error)?;
                let mount = MountImpl {
                    mountpoint,
                    channel,
                };
                let fd = unsafe { fuse_chan_fd(mount.channel.as_ptr()) };
                if fd < 0 {
                    return Err(io::Error::other(
                        "macFUSE did not provide a device descriptor; the kernel backend is required",
                    ));
                }
                // libfuse owns its descriptor; the session must own a separate one.
                let fd = unsafe { BorrowedFd::borrow_raw(fd) }.try_clone_to_owned()?;
                Ok((Arc::new(DevFuse(File::from(fd))), mount))
            }
            #[cfg(not(target_os = "macos"))]
            {
                let fd = unsafe { fuse_mount_compat25(mountpoint.as_ptr(), args) };
                if fd < 0 {
                    Err(ensure_last_os_error())
                } else {
                    let file = unsafe { File::from_raw_fd(fd) };
                    Ok((Arc::new(DevFuse(file)), MountImpl { mountpoint }))
                }
            }
        })
    }

    pub(crate) fn umount_impl(&mut self) -> io::Result<()> {
        // fuse_unmount_compat22 unfortunately doesn't return a status. Additionally,
        // it attempts to call realpath, which in turn calls into the filesystem. So
        // if the filesystem returns an error, the unmount does not take place, with
        // no indication of the error available to the caller. So we call unmount
        // directly, which is what osxfuse does anyway, since we already converted
        // to the real path when we first mounted.
        if let Err(err) = crate::mnt::libc_umount(&self.mountpoint) {
            // Linux always returns EPERM for non-root users.  We have to let the
            // library go through the setuid-root "fusermount -u" to unmount.
            if err == nix::errno::Errno::EPERM {
                #[cfg(not(any(
                    target_os = "macos",
                    target_os = "freebsd",
                    target_os = "dragonfly",
                    target_os = "openbsd",
                    target_os = "netbsd"
                )))]
                unsafe {
                    fuse_unmount_compat22(self.mountpoint.as_ptr());
                    return Ok(());
                }
            }
            return Err(err.into());
        }
        Ok(())
    }
}

// The channel is exclusively owned; unmount access is serialized by the session.
#[cfg(target_os = "macos")]
unsafe impl Send for MountImpl {}

#[cfg(target_os = "macos")]
impl Drop for MountImpl {
    fn drop(&mut self) {
        unsafe { fuse_unmount(self.mountpoint.as_ptr(), self.channel.as_ptr()) };
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    #[test]
    #[ignore = "requires installed and approved macFUSE kernel backend"]
    fn mount_unmount_and_remount() -> std::io::Result<()> {
        struct Filesystem(std::sync::mpsc::Sender<()>);
        impl crate::Filesystem for Filesystem {
            fn lookup(
                &self,
                _: &crate::Request,
                _: crate::INodeNo,
                _: &std::ffi::OsStr,
                reply: crate::ReplyEntry,
            ) {
                self.0.send(()).unwrap();
                reply.error(crate::Errno::ENOENT);
            }
        }
        let directory = tempfile::tempdir()?;
        for _ in 0..2 {
            let (sender, receiver) = std::sync::mpsc::channel();
            let mut session = crate::Session::new(
                Filesystem(sender),
                directory.path(),
                &crate::Config::default(),
            )?;
            let mut unmounter = session.unmount_callable();
            let worker = std::thread::spawn(move || session.run());
            // Wait for a real kernel request before unmounting: the mount helper
            // completes asynchronously after the FUSE initialization exchange.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let observed = loop {
                let _ = std::fs::metadata(directory.path().join("probe"));
                if receiver.try_recv().is_ok() {
                    break true;
                }
                if std::time::Instant::now() >= deadline {
                    break false;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            };
            unmounter.unmount()?;
            worker.join().expect("session thread panicked")?;
            assert!(observed, "mounted filesystem never received a request");
        }
        Ok(())
    }
}
