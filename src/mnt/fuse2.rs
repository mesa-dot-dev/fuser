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
        // fuser unmounts with unmount(2). fuse_unmount() would also ask
        // DiskArbitration to unmount the channel's disk, which can block
        // forever once the volume is gone. Destroying the channel closes
        // libfuse's descriptor and marks the daemon dead, so a volume that
        // is still mounted stops serving requests and needs a forced unmount.
        unsafe { fuse_chan_destroy(self.channel.as_ptr()) };
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    /// Mounting tests run one at a time so that a closed descriptor number
    /// cannot be reused by another test's device while it is being checked.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    #[ignore = "requires installed and approved macFUSE kernel backend"]
    fn mount_unmount_and_remount() -> std::io::Result<()> {
        let _serial = serial();
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

    unsafe extern "C" {
        fn fuse_chan_disk(channel: *mut libc::c_void) -> *const libc::c_void;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRelease(object: *const libc::c_void);
    }

    struct Filesystem;
    impl crate::Filesystem for Filesystem {
        /// Opening the root, which keeps the volume busy, needs its attributes.
        fn getattr(
            &self,
            _: &crate::Request,
            ino: crate::INodeNo,
            _: Option<crate::FileHandle>,
            reply: crate::ReplyAttr,
        ) {
            let now = std::time::SystemTime::now();
            reply.attr(
                &std::time::Duration::ZERO,
                &crate::FileAttr {
                    ino,
                    size: 0,
                    blocks: 0,
                    atime: now,
                    mtime: now,
                    ctime: now,
                    crtime: now,
                    kind: crate::FileType::Directory,
                    perm: 0o700,
                    nlink: 2,
                    uid: nix::unistd::getuid().as_raw(),
                    gid: nix::unistd::getgid().as_raw(),
                    rdev: 0,
                    blksize: 4096,
                    flags: 0,
                },
            );
        }
    }

    struct Mounted {
        mount: crate::mnt::Mount,
        worker: std::thread::JoinHandle<std::io::Result<()>>,
        libfuse_fd: libc::c_int,
        libfuse_device: (libc::dev_t, libc::ino_t, libc::dev_t),
    }

    fn mount_at(path: &std::path::Path) -> std::io::Result<Mounted> {
        let (device, mount) = crate::mnt::Mount::new(path, &[], crate::SessionACL::Owner)?;
        let Some(crate::mnt::MountImpl::Fuse2(inner)) = &mount.mount_impl else {
            unreachable!("macOS mounts through libfuse2");
        };
        let channel = inner.channel;
        let libfuse_fd = unsafe { super::fuse_chan_fd(channel.as_ptr()) };
        let libfuse_device = device_identity(libfuse_fd).expect("libfuse descriptor is open");
        let device = std::sync::Arc::try_unwrap(device).expect("device is uniquely owned");
        let session = crate::Session::from_fd(
            Filesystem,
            device.0.into(),
            crate::SessionACL::Owner,
            crate::Config::default(),
        )?;
        let worker = std::thread::spawn(move || session.run());
        // macFUSE attaches the volume's DiskArbitration disk to the channel
        // asynchronously after mount(2) completes. The old teardown only
        // reached DiskArbitration once the disk was attached.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10); // 10 seconds
        loop {
            let disk = unsafe { fuse_chan_disk(channel.as_ptr()) };
            if !disk.is_null() {
                unsafe { CFRelease(disk) };
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "macFUSE never attached a disk to the channel"
            );
            std::thread::sleep(std::time::Duration::from_millis(1)); // 1 millisecond
        }
        Ok(Mounted {
            mount,
            worker,
            libfuse_fd,
            libfuse_device,
        })
    }

    fn device_identity(fd: libc::c_int) -> Option<(libc::dev_t, libc::ino_t, libc::dev_t)> {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::EBADF)
            );
            return None;
        }
        let stat = unsafe { stat.assume_init() };
        Some((stat.st_dev, stat.st_ino, stat.st_rdev))
    }

    /// Runs a teardown step off the test thread so a stuck unmount fails the
    /// test instead of hanging it.
    fn bounded<T: Send + 'static>(step: impl FnOnce() -> T + Send + 'static) -> T {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || sender.send(step()));
        receiver
            .recv_timeout(std::time::Duration::from_secs(30)) // 30 seconds
            .expect("teardown did not finish")
    }

    /// Mounts a directory repeatedly and tears each mount down with
    /// `teardown`. Every path must destroy libfuse's channel, closing its
    /// original device descriptor, and leave the directory free to remount.
    fn check_teardown(
        teardown: fn(&std::path::Path, Mounted) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        use std::os::unix::fs::MetadataExt;

        let _serial = serial();
        // A failed teardown can leave the directory mounted, and removing it
        // would traverse the volume, so it is only removed after success.
        let directory = std::mem::ManuallyDrop::new(tempfile::tempdir()?);
        let path = directory.path().canonicalize()?;
        eprintln!("mounting at {}", path.display());
        let parent_device = std::fs::metadata(path.parent().unwrap())?.dev();
        for _ in 0..3 {
            let mounted = mount_at(&path)?;
            let (fd, device) = (mounted.libfuse_fd, mounted.libfuse_device);
            teardown(&path, mounted)?;
            assert_ne!(
                device_identity(fd),
                Some(device),
                "libfuse descriptor leaked"
            );
            assert_eq!(
                std::fs::metadata(&path)?.dev(),
                parent_device,
                "still mounted"
            );
        }
        drop(std::mem::ManuallyDrop::into_inner(directory));
        Ok(())
    }

    #[test]
    #[ignore = "requires installed and approved macFUSE kernel backend"]
    fn owner_unmount_destroys_libfuse_channel() -> std::io::Result<()> {
        check_teardown(|_, Mounted { mount, worker, .. }| {
            bounded(move || {
                mount.umount()?;
                worker.join().expect("session thread panicked")
            })
        })
    }

    /// Finder or umount(8) unmounts the volume behind the session's back.
    #[test]
    #[ignore = "requires installed and approved macFUSE kernel backend"]
    fn external_unmount_destroys_libfuse_channel() -> std::io::Result<()> {
        check_teardown(|path, Mounted { mount, worker, .. }| {
            nix::mount::unmount(path, nix::mount::MntFlags::empty())?;
            bounded(move || {
                let result = worker.join().expect("session thread panicked");
                drop(mount);
                result
            })
        })
    }

    /// MountImpl::new drops the mount without unmounting it when it fails
    /// after fuse_mount(). This drops it the same way, but after the
    /// handshake rather than through an injected failure. The dead volume
    /// stays mounted until it is forced off.
    #[test]
    #[ignore = "requires installed and approved macFUSE kernel backend"]
    fn dropped_mount_destroys_libfuse_channel() -> std::io::Result<()> {
        check_teardown(|path, mounted| {
            let Mounted {
                mut mount, worker, ..
            } = mounted;
            let mount_impl = mount.mount_impl.take();
            bounded(move || {
                drop(mount_impl);
                worker.join().expect("session thread panicked")
            })?;
            drop(mount);
            nix::mount::unmount(path, nix::mount::MntFlags::MNT_FORCE)?;
            Ok(())
        })
    }

    /// A busy volume refuses the owner's unmount, so the caller forces it.
    #[test]
    #[ignore = "requires installed and approved macFUSE kernel backend"]
    fn forced_unmount_destroys_libfuse_channel() -> std::io::Result<()> {
        check_teardown(|path, Mounted { mount, worker, .. }| {
            let busy = std::fs::File::open(path)?;
            let error = bounded(move || mount.umount()).expect_err("busy unmount succeeded");
            assert_eq!(error.raw_os_error(), Some(libc::EBUSY));
            nix::mount::unmount(path, nix::mount::MntFlags::MNT_FORCE)?;
            drop(busy);
            bounded(move || worker.join().expect("session thread panicked"))
        })
    }
}
