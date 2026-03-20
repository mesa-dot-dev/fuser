fn main() {
    // Register rustc cfg for switching between mount implementations.
    // When fuser MSRV is updated to v1.77 or above, we should switch from 'cargo:' to 'cargo::' syntax.
    println!(
        "cargo:rustc-check-cfg=cfg(fuser_mount_impl, values(\"pure-rust\", \"libfuse2\", \"libfuse3\"))"
    );
    println!("cargo:rustc-check-cfg=cfg(fuse_t)");

    #[cfg(all(not(feature = "libfuse"), not(target_os = "linux")))]
    unimplemented!("Building without libfuse is only supported on Linux");

    #[cfg(not(feature = "libfuse"))]
    {
        println!("cargo:rustc-cfg=fuser_mount_impl=\"pure-rust\"");
    }
    #[cfg(feature = "libfuse")]
    {
        if cfg!(target_os = "macos") {
            // macOS: use FUSE-T exclusively (kext-less FUSE implementation).
            // FUSE-T uses a stream socket instead of /dev/macfuseN, so we emit
            // cfg(fuse_t) to gate the stream-based channel receive path.
            pkg_config::Config::new()
                .atleast_version("1.0.0")
                .probe("fuse-t")
                .map_err(|e| {
                    eprintln!("{e}");
                    eprintln!(
                        "FUSE-T is required on macOS. Install it:\n  brew install macos-fuse-t/homebrew-cask/fuse-t"
                    );
                })
                .unwrap();
            println!("cargo:rustc-cfg=fuser_mount_impl=\"libfuse2\"");
            println!("cargo:rustc-cfg=fuse_t");
        } else {
            // First try to link with libfuse3
            if pkg_config::Config::new()
                .atleast_version("3.0.0")
                .probe("fuse3")
                .map_err(|e| eprintln!("{e}"))
                .is_ok()
            {
                println!("cargo:rustc-cfg=fuser_mount_impl=\"libfuse3\"");
            } else {
                // Fallback to libfuse
                pkg_config::Config::new()
                    .atleast_version("2.6.0")
                    .probe("fuse")
                    .map_err(|e| eprintln!("{e}"))
                    .unwrap();
                println!("cargo:rustc-cfg=fuser_mount_impl=\"libfuse2\"");
            }
        }
    }
}
