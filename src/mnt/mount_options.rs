use std::collections::HashSet;
use std::io;
use std::io::ErrorKind;

use nix::unistd::Uid;

use crate::SessionACL;

/// Fuser session configuration, including mount options.
#[derive(Debug, Clone, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct Config {
    /// Mount options.
    pub mount_options: Vec<MountOption>,
    /// Who can access the filesystem.
    pub acl: SessionACL,
    /// Number of event loop threads. If unspecified, one thread is used.
    pub n_threads: Option<usize>,
    /// Use `FUSE_DEV_IOC_CLONE` to give each worker thread its own fd.
    /// This enables more efficient request processing
    /// when multiple threads are used. Requires Linux 4.5+.
    pub clone_fd: bool,
}

/// Mount options accepted by the FUSE filesystem type
/// See 'man mount.fuse' for details
// TODO: add all options that 'man mount.fuse' documents and libfuse supports
#[derive(Debug, Eq, PartialEq, Hash, Clone)]
pub enum MountOption {
    /// Set the name of the source in mtab
    FSName(String),
    /// Set the filesystem subtype in mtab
    Subtype(String),
    /// Allows passing an option which is not otherwise supported in these enums
    #[allow(clippy::upper_case_acronyms)]
    CUSTOM(String),

    /// Select macFUSE's FSKit backend.
    ///
    /// This option is accepted only by the macOS libfuse2 mount path. Use of
    /// `backend=fskit` through [`MountOption::CUSTOM`] is rejected.
    MacFsKit,

    /* Parameterless options */
    /// Automatically unmount when the mounting process exits
    ///
    /// `AutoUnmount` requires `AllowOther` or `AllowRoot`. If `AutoUnmount` is set and neither `Allow...` is set, the FUSE configuration must permit `allow_other`, otherwise mounting will fail.
    AutoUnmount,
    /// Enable permission checking in the kernel
    DefaultPermissions,

    /* Flags */
    /// Enable special character and block devices
    Dev,
    /// Disable special character and block devices
    NoDev,
    /// Honor set-user-id and set-groupd-id bits on files
    Suid,
    /// Don't honor set-user-id and set-groupd-id bits on files
    NoSuid,
    /// Read-only filesystem
    RO,
    /// Read-write filesystem
    RW,
    /// Allow execution of binaries
    Exec,
    /// Don't allow execution of binaries
    NoExec,
    /// Support inode access time
    Atime,
    /// Don't update inode access time
    NoAtime,
    /// All modifications to directories will be done synchronously
    DirSync,
    /// All I/O will be done synchronously
    Sync,
    /// All I/O will be done asynchronously
    Async,
    /* libfuse library options, such as "direct_io", are not included since they are specific
    to libfuse, and not part of the kernel ABI */
}

impl MountOption {
    #[cfg(test)]
    fn from_str(s: &str) -> MountOption {
        match s {
            "auto_unmount" => MountOption::AutoUnmount,
            "default_permissions" => MountOption::DefaultPermissions,
            "dev" => MountOption::Dev,
            "nodev" => MountOption::NoDev,
            "suid" => MountOption::Suid,
            "nosuid" => MountOption::NoSuid,
            "ro" => MountOption::RO,
            "rw" => MountOption::RW,
            "exec" => MountOption::Exec,
            "noexec" => MountOption::NoExec,
            "atime" => MountOption::Atime,
            "noatime" => MountOption::NoAtime,
            "dirsync" => MountOption::DirSync,
            "sync" => MountOption::Sync,
            "async" => MountOption::Async,
            "backend=fskit" => MountOption::MacFsKit,
            x if x.starts_with("fsname=") => MountOption::FSName(x[7..].into()),
            x if x.starts_with("subtype=") => MountOption::Subtype(x[8..].into()),
            x => MountOption::CUSTOM(x.into()),
        }
    }
}

#[allow(dead_code)]
#[derive(PartialEq)]
pub(crate) enum MountOptionGroup {
    KernelOption,
    KernelFlag,
    Fusermount,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AclRequestIdentity {
    Direct,
    MacFsKitRootProxy,
}

impl AclRequestIdentity {
    pub(crate) fn matches_owner_acl(self, request_uid: Uid, session_owner: Uid) -> bool {
        request_uid == session_owner
            || (self == AclRequestIdentity::MacFsKitRootProxy && request_uid.is_root())
    }
}

pub(crate) fn validate_config(options: &Config) -> Result<AclRequestIdentity, io::Error> {
    let mut options_set = HashSet::new();
    options_set.extend(options.mount_options.iter().cloned());
    let conflicting: HashSet<MountOption> = options
        .mount_options
        .iter()
        .flat_map(conflicts_with)
        .collect();
    let intersection: Vec<MountOption> = conflicting.intersection(&options_set).cloned().collect();
    if !intersection.is_empty() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!("Conflicting mount options found: {intersection:?}"),
        ));
    }

    if options.mount_options.iter().any(|option| {
        matches!(
            option,
            MountOption::CUSTOM(value)
                if value.split(',').any(|token| token == "backend=fskit")
        )
    }) {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "backend=fskit must use MountOption::MacFsKit",
        ));
    }

    if options.mount_options.contains(&MountOption::MacFsKit) {
        if !cfg!(all(target_os = "macos", fuser_mount_impl = "libfuse2")) {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "MountOption::MacFsKit requires the macOS libfuse2 mount path",
            ));
        }
        return Ok(AclRequestIdentity::MacFsKitRootProxy);
    }

    Ok(AclRequestIdentity::Direct)
}

fn conflicts_with(option: &MountOption) -> Vec<MountOption> {
    match option {
        MountOption::FSName(_)
        | MountOption::Subtype(_)
        | MountOption::CUSTOM(_)
        | MountOption::MacFsKit
        | MountOption::DirSync
        | MountOption::AutoUnmount
        | MountOption::DefaultPermissions => vec![],
        MountOption::Dev => vec![MountOption::NoDev],
        MountOption::NoDev => vec![MountOption::Dev],
        MountOption::Suid => vec![MountOption::NoSuid],
        MountOption::NoSuid => vec![MountOption::Suid],
        MountOption::RO => vec![MountOption::RW],
        MountOption::RW => vec![MountOption::RO],
        MountOption::Exec => vec![MountOption::NoExec],
        MountOption::NoExec => vec![MountOption::Exec],
        MountOption::Atime => vec![MountOption::NoAtime],
        MountOption::NoAtime => vec![MountOption::Atime],
        MountOption::Sync => vec![MountOption::Async],
        MountOption::Async => vec![MountOption::Sync],
    }
}

// Format option to be passed to libfuse or kernel
#[allow(dead_code)]
pub(crate) fn option_to_string(option: &MountOption) -> String {
    match option {
        MountOption::FSName(name) => format!("fsname={name}"),
        MountOption::Subtype(subtype) => format!("subtype={subtype}"),
        MountOption::CUSTOM(value) => value.to_string(),
        MountOption::MacFsKit => "backend=fskit".to_string(),
        MountOption::AutoUnmount => "auto_unmount".to_string(),
        MountOption::DefaultPermissions => "default_permissions".to_string(),
        MountOption::Dev => "dev".to_string(),
        MountOption::NoDev => "nodev".to_string(),
        MountOption::Suid => "suid".to_string(),
        MountOption::NoSuid => "nosuid".to_string(),
        MountOption::RO => "ro".to_string(),
        MountOption::RW => "rw".to_string(),
        MountOption::Exec => "exec".to_string(),
        MountOption::NoExec => "noexec".to_string(),
        MountOption::Atime => "atime".to_string(),
        MountOption::NoAtime => "noatime".to_string(),
        MountOption::DirSync => "dirsync".to_string(),
        MountOption::Sync => "sync".to_string(),
        MountOption::Async => "async".to_string(),
    }
}

#[allow(dead_code)]
pub(crate) fn option_group(option: &MountOption) -> MountOptionGroup {
    match option {
        MountOption::FSName(_) => MountOptionGroup::Fusermount,
        MountOption::Subtype(_) => MountOptionGroup::Fusermount,
        MountOption::CUSTOM(_) => MountOptionGroup::KernelOption,
        MountOption::MacFsKit => MountOptionGroup::KernelOption,
        MountOption::AutoUnmount => MountOptionGroup::Fusermount,
        MountOption::Dev => MountOptionGroup::KernelFlag,
        MountOption::NoDev => MountOptionGroup::KernelFlag,
        MountOption::Suid => MountOptionGroup::KernelFlag,
        MountOption::NoSuid => MountOptionGroup::KernelFlag,
        MountOption::RO => MountOptionGroup::KernelFlag,
        MountOption::RW => MountOptionGroup::KernelFlag,
        MountOption::Exec => MountOptionGroup::KernelFlag,
        MountOption::NoExec => MountOptionGroup::KernelFlag,
        MountOption::Atime => MountOptionGroup::KernelFlag,
        MountOption::NoAtime => MountOptionGroup::KernelFlag,
        MountOption::DirSync => MountOptionGroup::KernelFlag,
        MountOption::Sync => MountOptionGroup::KernelFlag,
        MountOption::Async => MountOptionGroup::KernelFlag,
        MountOption::DefaultPermissions => MountOptionGroup::KernelOption,
    }
}

#[cfg(target_os = "linux")]
#[allow(dead_code)]
pub(crate) fn option_to_flag(option: &MountOption) -> io::Result<nix::mount::MsFlags> {
    match option {
        MountOption::Dev => Ok(nix::mount::MsFlags::empty()), // There is no option for dev. It's the absence of NoDev
        MountOption::NoDev => Ok(nix::mount::MsFlags::MS_NODEV),
        MountOption::Suid => Ok(nix::mount::MsFlags::empty()),
        MountOption::NoSuid => Ok(nix::mount::MsFlags::MS_NOSUID),
        MountOption::RW => Ok(nix::mount::MsFlags::empty()),
        MountOption::RO => Ok(nix::mount::MsFlags::MS_RDONLY),
        MountOption::Exec => Ok(nix::mount::MsFlags::empty()),
        MountOption::NoExec => Ok(nix::mount::MsFlags::MS_NOEXEC),
        MountOption::Atime => Ok(nix::mount::MsFlags::empty()),
        MountOption::NoAtime => Ok(nix::mount::MsFlags::MS_NOATIME),
        MountOption::Async => Ok(nix::mount::MsFlags::empty()),
        MountOption::Sync => Ok(nix::mount::MsFlags::MS_SYNCHRONOUS),
        MountOption::DirSync => Ok(nix::mount::MsFlags::MS_DIRSYNC),
        option => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Invalid mount option for flag conversion: {option:?}"),
        )),
    }
}

#[cfg(target_os = "macos")]
#[allow(dead_code)]
pub(crate) fn option_to_flag(option: &MountOption) -> io::Result<nix::mount::MntFlags> {
    match option {
        MountOption::Dev => Ok(nix::mount::MntFlags::empty()), // There is no option for dev. It's the absence of NoDev
        MountOption::NoDev => Ok(nix::mount::MntFlags::MNT_NODEV),
        MountOption::Suid => Ok(nix::mount::MntFlags::empty()),
        MountOption::NoSuid => Ok(nix::mount::MntFlags::MNT_NOSUID),
        MountOption::RW => Ok(nix::mount::MntFlags::empty()),
        MountOption::RO => Ok(nix::mount::MntFlags::MNT_RDONLY),
        MountOption::Exec => Ok(nix::mount::MntFlags::empty()),
        MountOption::NoExec => Ok(nix::mount::MntFlags::MNT_NOEXEC),
        MountOption::Atime => Ok(nix::mount::MntFlags::empty()),
        MountOption::NoAtime => Ok(nix::mount::MntFlags::MNT_NOATIME),
        MountOption::Async => Ok(nix::mount::MntFlags::empty()),
        MountOption::Sync => Ok(nix::mount::MntFlags::MNT_SYNCHRONOUS),
        option => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Invalid mount option for flag conversion: {option:?}"),
        )),
    }
}

#[cfg_attr(
    any(
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "openbsd",
        target_os = "netbsd"
    ),
    allow(dead_code)
)]
#[cfg(any(
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "openbsd",
    target_os = "netbsd"
))]
pub(crate) fn option_to_flag(option: &MountOption) -> io::Result<nix::mount::MntFlags> {
    match option {
        MountOption::Dev => Ok(nix::mount::MntFlags::empty()),
        #[cfg(target_os = "freebsd")]
        MountOption::NoDev => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "NoDev option is not supported on FreeBSD",
        )),
        #[cfg(not(target_os = "freebsd"))]
        MountOption::NoDev => Ok(nix::mount::MntFlags::MNT_NODEV),
        MountOption::Suid => Ok(nix::mount::MntFlags::empty()),
        MountOption::NoSuid => Ok(nix::mount::MntFlags::MNT_NOSUID),
        MountOption::RW => Ok(nix::mount::MntFlags::empty()),
        MountOption::RO => Ok(nix::mount::MntFlags::MNT_RDONLY),
        MountOption::Exec => Ok(nix::mount::MntFlags::empty()),
        MountOption::NoExec => Ok(nix::mount::MntFlags::MNT_NOEXEC),
        MountOption::Atime => Ok(nix::mount::MntFlags::empty()),
        MountOption::NoAtime => Ok(nix::mount::MntFlags::MNT_NOATIME),
        MountOption::Async => Ok(nix::mount::MntFlags::MNT_ASYNC),
        MountOption::Sync => Ok(nix::mount::MntFlags::MNT_SYNCHRONOUS),
        MountOption::DirSync => Ok(nix::mount::MntFlags::MNT_SYNCHRONOUS),
        option => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Invalid mount option for flag conversion: {option:?}"),
        )),
    }
}

#[cfg(test)]
mod test {
    use crate::mnt::mount_options::*;

    #[test]
    fn option_validation() {
        assert!(
            validate_config(&Config {
                mount_options: vec![MountOption::Suid, MountOption::NoSuid],
                ..Config::default()
            })
            .is_err()
        );
        assert_eq!(
            validate_config(&Config {
                mount_options: vec![MountOption::Suid, MountOption::NoExec],
                ..Config::default()
            })
            .unwrap(),
            AclRequestIdentity::Direct
        );
    }

    #[test]
    fn mac_fskit_requires_typed_transport_selection() {
        for value in [
            "backend=fskit",
            "backend=fskit,foo",
            "foo,backend=fskit",
            "foo,backend=fskit,bar",
        ] {
            let error = validate_config(&Config {
                mount_options: vec![MountOption::CUSTOM(value.to_owned())],
                ..Config::default()
            })
            .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidInput);
        }

        for value in ["xbackend=fskit", "backend=fskitx", "backend=fskit=1"] {
            assert_eq!(
                validate_config(&Config {
                    mount_options: vec![MountOption::CUSTOM(value.to_owned())],
                    ..Config::default()
                })
                .unwrap(),
                AclRequestIdentity::Direct
            );
        }

        let result = validate_config(&Config {
            mount_options: vec![MountOption::MacFsKit],
            ..Config::default()
        });
        if cfg!(all(target_os = "macos", fuser_mount_impl = "libfuse2")) {
            assert_eq!(result.unwrap(), AclRequestIdentity::MacFsKitRootProxy);
        } else {
            assert_eq!(result.unwrap_err().kind(), ErrorKind::InvalidInput);
        }
    }

    #[test]
    fn mac_fskit_root_proxy_only_changes_owner_acl_comparison() {
        let owner = Uid::from_raw(501);
        let root = Uid::from_raw(0);
        let other = Uid::from_raw(502);

        assert!(AclRequestIdentity::Direct.matches_owner_acl(owner, owner));
        assert!(!AclRequestIdentity::Direct.matches_owner_acl(root, owner));
        assert!(AclRequestIdentity::MacFsKitRootProxy.matches_owner_acl(root, owner));
        assert!(AclRequestIdentity::MacFsKitRootProxy.matches_owner_acl(owner, owner));
        assert!(!AclRequestIdentity::MacFsKitRootProxy.matches_owner_acl(other, owner));
        assert_eq!((root.as_raw(), other.as_raw()), (0, 502));
    }

    #[test]
    fn option_round_trip() {
        use crate::mnt::mount_options::MountOption::*;
        for x in &[
            FSName("Blah".to_owned()),
            Subtype("Bloo".to_owned()),
            CUSTOM("bongos".to_owned()),
            MacFsKit,
            AutoUnmount,
            DefaultPermissions,
            Dev,
            NoDev,
            Suid,
            NoSuid,
            RO,
            RW,
            Exec,
            NoExec,
            Atime,
            NoAtime,
            DirSync,
            Sync,
            Async,
        ] {
            assert_eq!(*x, MountOption::from_str(option_to_string(x).as_ref()));
        }
    }
}
