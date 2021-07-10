/*
 * Copyright 2019-2021 Wren Powell
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use std::io;
use std::path::Path;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

#[cfg(all(any(unix, doc), feature = "file-metadata"))]
use {
    bitflags::bitflags,
    nix::unistd::{chown, Gid, Uid},
    posix_acl::{PosixACL, Qualifier as PosixQualifier, ACL_EXECUTE, ACL_READ, ACL_WRITE},
    std::collections::HashMap,
    std::fs::set_permissions,
    std::os::unix::fs::{MetadataExt, PermissionsExt},
    std::time::{Duration, UNIX_EPOCH},
};
#[cfg(feature = "file-metadata")]
use {filetime::set_file_times, std::time::SystemTime};

/// The metadata for a file in the file system.
///
/// This trait can be implemented to customize how [`FileRepo`] handles file metadata.
///
/// [`FileRepo`]: crate::repo::file::FileRepo
pub trait FileMetadata: Serialize + DeserializeOwned {
    /// Read the metadata from the file at `path` and create a new instance.
    fn from_file(path: &Path) -> io::Result<Self>;

    /// Write this metadata to the file at `path`.
    fn write_metadata(&self, path: &Path) -> io::Result<()>;
}

/// A `FileMetadata` which stores no metadata.
#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy, Default, Serialize, Deserialize)]
pub struct NoMetadata;

impl FileMetadata for NoMetadata {
    fn from_file(_path: &Path) -> io::Result<Self> {
        Ok(NoMetadata)
    }

    fn write_metadata(&self, _path: &Path) -> io::Result<()> {
        Ok(())
    }
}

/// A qualifier which determines who is granted a set of permissions in an access control list.
#[cfg(all(any(unix, doc), feature = "file-metadata"))]
#[cfg_attr(docsrs, doc(cfg(all(unix, feature = "file-metadata"))))]
#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy, Serialize, Deserialize)]
pub enum AccessQualifier {
    /// The user with a given UID.
    User(u32),

    /// The group with a given GID.
    Group(u32),
}

#[cfg(all(any(unix, doc), feature = "file-metadata"))]
bitflags! {
    /// The permission mode for an access control list.
    #[cfg_attr(docsrs, doc(cfg(all(unix, feature = "file-metadata"))))]
    #[derive(Serialize, Deserialize)]
    pub struct AccessMode: u32 {
        const READ = ACL_READ;
        const WRITE = ACL_WRITE;
        const EXECUTE = ACL_EXECUTE;
    }

}

/// Construct a `SystemTime` from a unix timestamp.
#[cfg(all(any(unix, doc), feature = "file-metadata"))]
fn unix_file_time(secs: i64, nsec: i64) -> SystemTime {
    let file_time = if secs.is_positive() {
        UNIX_EPOCH + Duration::from_secs(secs as u64)
    } else {
        UNIX_EPOCH - Duration::from_secs(secs as u64)
    };
    if nsec.is_positive() {
        file_time + Duration::from_nanos(nsec as u64)
    } else {
        file_time - Duration::from_nanos(nsec as u64)
    }
}

/// A `FileMetadata` for unix-like operating systems.
///
/// Extended attributes and access control lists may not work on all platforms. If a platform is
/// unsupported, [`from_file`] will acts as if files have no extended attributes or ACL entries and
/// [`write_metadata`] will not attempt to write them.
///
/// If the current user does not have the necessary permissions to set the UID/GID of the file,
/// [`write_metadata`] will silently ignore the error and return `Ok`.
///
/// [`from_file`]: crate::repo::file::FileMetadata::from_file
/// [`write_metadata`]: crate::repo::file::FileMetadata::write_metadata
#[cfg(all(any(unix, doc), feature = "file-metadata"))]
#[cfg_attr(docsrs, doc(cfg(all(unix, feature = "file-metadata"))))]
#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
pub struct UnixMetadata {
    /// The file mode (st_mode).
    pub mode: u32,

    /// The time the file was last modified (st_mtime).
    pub modified: SystemTime,

    /// The time the file was last accessed (st_atime).
    pub accessed: SystemTime,

    /// The time the file metadata was last changed (st_ctime).
    pub changed: SystemTime,

    /// The UID of the user which owns the file (st_uid).
    pub user: u32,

    /// The GID of the group which owns the file (st_gid).
    pub group: u32,

    /// The extended attributes of the file.
    pub attributes: HashMap<String, Vec<u8>>,

    /// The access control list for the file.
    ///
    /// This is a map of qualifiers to their associated permissions.
    pub acl: HashMap<AccessQualifier, AccessMode>,
}

#[cfg(all(any(unix, doc), feature = "file-metadata"))]
impl FileMetadata for UnixMetadata {
    fn from_file(path: &Path) -> io::Result<Self> {
        let metadata = path.metadata()?;

        let mut attributes = HashMap::new();
        if xattr::SUPPORTED_PLATFORM {
            for attr_name in xattr::list(&path)? {
                if let Some(attr_value) = xattr::get(&path, &attr_name)? {
                    attributes.insert(attr_name.to_string_lossy().to_string(), attr_value);
                }
            }
        }

        #[cfg(not(target_os = "linux"))]
        let acl = HashMap::new();

        // This ACL library only supports Linux.
        #[cfg(target_os = "linux")]
        let acl = PosixACL::read_acl(path)
            .map_err(|error| io::Error::from(error.kind()))?
            .entries()
            .into_iter()
            .filter_map(|entry| match entry.qual {
                PosixQualifier::User(uid) => Some((
                    AccessQualifier::User(uid),
                    AccessMode::from_bits(entry.perm).unwrap(),
                )),
                PosixQualifier::Group(gid) => Some((
                    AccessQualifier::Group(gid),
                    AccessMode::from_bits(entry.perm).unwrap(),
                )),
                _ => None,
            })
            .collect();

        Ok(Self {
            mode: metadata.mode(),
            modified: unix_file_time(metadata.mtime(), metadata.mtime_nsec()),
            accessed: unix_file_time(metadata.atime(), metadata.atime_nsec()),
            changed: unix_file_time(metadata.ctime(), metadata.ctime_nsec()),
            user: metadata.uid(),
            group: metadata.gid(),
            attributes,
            acl,
        })
    }

    fn write_metadata(&self, path: &Path) -> io::Result<()> {
        if xattr::SUPPORTED_PLATFORM {
            for (attr_name, attr_value) in self.attributes.iter() {
                xattr::set(&path, &attr_name, &attr_value)?;
            }
        }

        set_permissions(path, PermissionsExt::from_mode(self.mode))?;

        // This ACL library only supports Linux.
        #[cfg(target_os = "linux")]
        if !self.acl.is_empty() {
            let mut acl = PosixACL::new(self.mode);
            for (qualifier, permissions) in self.acl.iter() {
                let posix_qualifier = match qualifier {
                    AccessQualifier::User(uid) => PosixQualifier::User(*uid),
                    AccessQualifier::Group(gid) => PosixQualifier::Group(*gid),
                };
                acl.set(posix_qualifier, permissions.bits());
            }
            acl.write_acl(path)
                .map_err(|error| io::Error::from(error.kind()))?;
        }

        match chown(
            path,
            Some(Uid::from_raw(self.user)),
            Some(Gid::from_raw(self.group)),
        ) {
            Err(nix::Error::Sys(nix::errno::Errno::EPERM)) => (),
            Err(error) => return Err(io::Error::new(io::ErrorKind::Other, error)),
            _ => (),
        };

        set_file_times(path, self.accessed.into(), self.modified.into())?;

        Ok(())
    }
}

/// A `FileMetadata` for metadata that is common to most platforms.
#[cfg(feature = "file-metadata")]
#[cfg_attr(docsrs, doc(cfg(feature = "file-metadata")))]
#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
pub struct CommonMetadata {
    /// The time the file was last modified.
    pub modified: SystemTime,

    /// The time the file was last accessed.
    pub accessed: SystemTime,
}

#[cfg(feature = "file-metadata")]
impl FileMetadata for CommonMetadata {
    fn from_file(path: &Path) -> io::Result<Self> {
        let metadata = path.metadata()?;
        Ok(Self {
            modified: metadata.modified()?,
            accessed: metadata.accessed()?,
        })
    }

    fn write_metadata(&self, path: &Path) -> io::Result<()> {
        set_file_times(path, self.accessed.into(), self.modified.into())
    }
}
