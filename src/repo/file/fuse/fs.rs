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

#![cfg(all(any(unix, doc), feature = "fuse-mount"))]

use std::collections::{hash_map::Entry as HashMapEntry, HashMap};
use std::ffi::OsStr;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::{Duration, SystemTime};

use fuse::{
    FileAttr, FileType as FuseFileType, Filesystem, ReplyAttr, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request,
};
use nix::fcntl::OFlag;
use nix::libc;
use nix::sys::stat;
use once_cell::sync::Lazy;
use relative_path::RelativePathBuf;
use time::Timespec;

use super::handle::{HandleInfo, HandleTable, HandleType};
use super::inode::InodeTable;

use crate::repo::file::{
    entry::{Entry, FileType},
    metadata::UnixMetadata,
    repository::{FileRepo, EMPTY_PARENT},
    special::UnixSpecialType,
};
use crate::repo::{Commit, Object};

/// The block size used to calculate `st_blocks`.
const BLOCK_SIZE: u64 = 512;

/// The default TTL value to use in FUSE replies.
///
/// Because the backing `FileRepo` can only be safely modified through the FUSE file system, while
/// it is mounted, we can set this to an arbitrarily large value.
const DEFAULT_TTL: Timespec = Timespec {
    sec: i64::MAX,
    nsec: i32::MAX,
};

/// The value of `st_rdev` value to use if the file is not a character or block device.
const NON_SPECIAL_RDEV: u32 = 0;

/// The default permissions bits for a directory.
const DEFAULT_DIR_MODE: u32 = 0o775;

/// The default permissions bits for a file.
const DEFAULT_FILE_MODE: u32 = 0o664;

/// The set of `open` flags which are not supported by this file system.
const UNSUPPORTED_OPEN_FLAGS: Lazy<OFlag> = Lazy::new(|| OFlag::O_DIRECT | OFlag::O_TMPFILE);

/// Handle a `crate::Result` in a FUSE method.
macro_rules! try_result {
    ($result:expr, $reply:expr) => {
        match $result {
            Ok(result) => result,
            Err(error) => {
                $reply.error(crate::Error::from(error).to_errno());
                return;
            }
        }
    };
}

/// Handle an `Option` in a FUSE method.
macro_rules! try_option {
    ($result:expr, $reply:expr, $error:expr) => {
        match $result {
            Some(result) => result,
            None => {
                $reply.error($error);
                return;
            }
        }
    };
}

impl crate::Error {
    /// Get the libc errno for this error.
    fn to_errno(&self) -> i32 {
        match self {
            crate::Error::AlreadyExists => libc::EEXIST,
            crate::Error::NotFound => libc::ENOENT,
            crate::Error::InvalidPath => libc::ENOENT,
            crate::Error::NotEmpty => libc::ENOTEMPTY,
            crate::Error::NotDirectory => libc::ENOTDIR,
            crate::Error::NotFile => libc::EISDIR,
            crate::Error::Io(error) => match error.raw_os_error() {
                Some(errno) => errno,
                None => libc::EIO,
            },
            _ => libc::EIO,
        }
    }
}

/// Convert the given `time` to a `SystemTime`.
fn to_system_time(time: Timespec) -> SystemTime {
    let duration = Duration::new(time.sec.abs() as u64, time.nsec.abs() as u32);
    if time.sec.is_positive() {
        SystemTime::UNIX_EPOCH + duration
    } else {
        SystemTime::UNIX_EPOCH - duration
    }
}

/// Convert the given `time` to a `Timespec`.
fn to_timespec(time: SystemTime) -> Timespec {
    match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(duration) => Timespec {
            sec: duration.as_secs() as i64,
            nsec: duration.subsec_nanos() as i32,
        },
        Err(error) => Timespec {
            sec: -(error.duration().as_secs() as i64),
            nsec: -(error.duration().subsec_nanos() as i32),
        },
    }
}

impl Entry<UnixSpecialType, UnixMetadata> {
    /// Create a new `Entry` of the given `file_type` with default metadata.
    fn new(file_type: FileType<UnixSpecialType>, req: &Request) -> Self {
        let mut entry = Self {
            file_type,
            metadata: None,
        };
        entry.metadata = Some(entry.default_metadata(req));
        entry
    }

    /// The default `UnixMetadata` for an entry that has no metadata.
    fn default_metadata(&self, req: &Request) -> UnixMetadata {
        UnixMetadata {
            mode: if self.is_directory() {
                DEFAULT_DIR_MODE
            } else {
                DEFAULT_FILE_MODE
            },
            modified: SystemTime::now(),
            accessed: SystemTime::now(),
            user: req.uid(),
            group: req.gid(),
            attributes: HashMap::new(),
            acl: HashMap::new(),
        }
    }

    /// Return this entry's metadata or the default metadata if it's `None`.
    fn metadata_or_default(self, req: &Request) -> UnixMetadata {
        match self.metadata {
            Some(metadata) => metadata,
            None => self.default_metadata(req),
        }
    }
}

impl FileType<UnixSpecialType> {
    /// Convert this `FileType` to a `fuse`-compatible file type.
    pub fn to_file_type(&self) -> FuseFileType {
        match self {
            FileType::File => FuseFileType::RegularFile,
            FileType::Directory => FuseFileType::Directory,
            FileType::Special(UnixSpecialType::BlockDevice { .. }) => FuseFileType::BlockDevice,
            FileType::Special(UnixSpecialType::CharacterDevice { .. }) => FuseFileType::CharDevice,
            FileType::Special(UnixSpecialType::SymbolicLink { .. }) => FuseFileType::Symlink,
            FileType::Special(UnixSpecialType::NamedPipe { .. }) => FuseFileType::NamedPipe,
        }
    }
}

/// A directory entry for an open file handle.
#[derive(Debug)]
pub struct DirectoryEntry {
    pub file_name: String,
    pub file_type: FuseFileType,
    pub inode: u64,
}

#[derive(Debug)]
pub struct FuseAdapter<'a> {
    /// The repository which contains the virtual file system.
    repo: &'a mut FileRepo<UnixSpecialType, UnixMetadata>,

    /// A table for allocating inodes.
    inodes: InodeTable,

    /// A table for allocating file handles.
    handles: HandleTable,

    /// A map of inodes to currently open file objects.
    objects: HashMap<u64, Object>,

    /// A map of open directory handles to lists of their child entries.
    directories: HashMap<u64, Vec<DirectoryEntry>>,
}

impl<'a> FuseAdapter<'a> {
    /// Create a new `FuseAdapter` from the given `repo`.
    pub fn new(repo: &'a mut FileRepo<UnixSpecialType, UnixMetadata>) -> Self {
        let mut inodes = InodeTable::new();

        for (path, _) in repo.0.state().walk(&*EMPTY_PARENT).unwrap() {
            inodes.insert(path);
        }

        Self {
            repo,
            inodes,
            handles: HandleTable::new(),
            objects: HashMap::new(),
            directories: HashMap::new(),
        }
    }

    /// Return the path of the entry with the given `name` and `parent_inode`.
    ///
    /// If there is no such entry, this returns `None`.
    fn child_path(&self, parent_inode: u64, name: &OsStr) -> Option<RelativePathBuf> {
        Some(
            self.inodes
                .path(parent_inode)?
                .join(name.to_string_lossy().as_ref()),
        )
    }

    /// Get the `FileAttr` for the `entry` with the given `inode`.
    fn entry_attr(
        &mut self,
        entry: &Entry<UnixSpecialType, UnixMetadata>,
        inode: u64,
        req: &Request,
    ) -> crate::Result<FileAttr> {
        let entry_path = self.inodes.path(inode).ok_or(crate::Error::NotFound)?;
        let default_metadata = entry.default_metadata(req);
        let metadata = entry.metadata.as_ref().unwrap_or(&default_metadata);

        let size = match &entry.file_type {
            FileType::File => match self.objects.entry(inode) {
                HashMapEntry::Occupied(mut entry) => {
                    let object = entry.get_mut();
                    // We must commit changes in case this object has a transaction in progress.
                    object.commit()?;
                    object.size().unwrap()
                }
                HashMapEntry::Vacant(entry) => {
                    let object = self.repo.open(entry_path)?;
                    entry.insert(object).size().unwrap()
                }
            },
            FileType::Directory => 0,
            FileType::Special(special) => match special {
                // The `st_size` of a symlink should be the length of the pathname it contains.
                UnixSpecialType::SymbolicLink { target } => target.as_os_str().len() as u64,
                _ => 0,
            },
        };

        Ok(FileAttr {
            ino: inode,
            size,
            blocks: size / BLOCK_SIZE,
            atime: to_timespec(metadata.accessed),
            mtime: to_timespec(metadata.modified),
            ctime: to_timespec(SystemTime::now()),
            crtime: to_timespec(SystemTime::now()),
            kind: match &entry.file_type {
                FileType::File => fuse::FileType::RegularFile,
                FileType::Directory => fuse::FileType::Directory,
                FileType::Special(special) => match special {
                    UnixSpecialType::SymbolicLink { .. } => fuse::FileType::Symlink,
                    UnixSpecialType::NamedPipe => fuse::FileType::NamedPipe,
                    UnixSpecialType::BlockDevice { .. } => fuse::FileType::BlockDevice,
                    UnixSpecialType::CharacterDevice { .. } => fuse::FileType::CharDevice,
                },
            },
            perm: metadata.mode as u16,
            nlink: 0,
            uid: metadata.user,
            gid: metadata.group,
            rdev: match &entry.file_type {
                FileType::Special(special) => match special {
                    UnixSpecialType::BlockDevice { major, minor } => {
                        stat::makedev(*major, *minor) as u32
                    }
                    UnixSpecialType::CharacterDevice { major, minor } => {
                        stat::makedev(*major, *minor) as u32
                    }
                    _ => NON_SPECIAL_RDEV,
                },
                _ => NON_SPECIAL_RDEV,
            },
            flags: 0,
        })
    }
}

impl<'a> Filesystem for FuseAdapter<'a> {
    fn lookup(&mut self, req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let entry_path = try_option!(self.child_path(parent, name), reply, libc::ENOENT);
        let entry_inode = self.inodes.inode(&entry_path).unwrap();
        let entry = try_result!(self.repo.entry(&entry_path), reply);

        let attr = try_result!(self.entry_attr(&entry, entry_inode, req), reply);

        let generation = self.inodes.generation(entry_inode);

        reply.entry(&DEFAULT_TTL, &attr, generation);
    }

    fn getattr(&mut self, req: &Request, ino: u64, reply: ReplyAttr) {
        let entry_path = try_option!(self.inodes.path(ino), reply, libc::ENOENT);
        let entry = try_result!(self.repo.entry(&entry_path), reply);
        let attr = try_result!(self.entry_attr(&entry, ino, req), reply);

        reply.attr(&DEFAULT_TTL, &attr);
    }

    fn setattr(
        &mut self,
        req: &Request,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        _size: Option<u64>,
        atime: Option<Timespec>,
        mtime: Option<Timespec>,
        _fh: Option<u64>,
        _crtime: Option<Timespec>,
        _chgtime: Option<Timespec>,
        _bkuptime: Option<Timespec>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        let entry_path = try_option!(self.inodes.path(ino), reply, libc::ENOENT);

        let mut entry = try_result!(self.repo.entry(&entry_path), reply);

        let default_metadata = entry.default_metadata(req);
        let metadata = entry.metadata.get_or_insert(default_metadata);

        if let Some(mode) = mode {
            metadata.mode = mode;
        }

        if let Some(uid) = uid {
            metadata.user = uid;
        }

        if let Some(gid) = gid {
            metadata.group = gid;
        }

        if let Some(atime) = atime {
            metadata.accessed = to_system_time(atime);
        }

        if let Some(mtime) = mtime {
            metadata.modified = to_system_time(mtime);
        }

        try_result!(
            self.repo.set_metadata(entry_path, entry.metadata.clone()),
            reply
        );

        let attr = try_result!(self.entry_attr(&entry, ino, req), reply);
        reply.attr(&DEFAULT_TTL, &attr);
    }

    fn readlink(&mut self, _req: &Request, ino: u64, reply: ReplyData) {
        let entry_path = try_option!(self.inodes.path(ino), reply, libc::ENOENT);
        let entry = try_result!(self.repo.entry(&entry_path), reply);
        match &entry.file_type {
            FileType::Special(UnixSpecialType::SymbolicLink { target }) => {
                reply.data(target.as_os_str().as_bytes());
            }
            _ => {
                reply.error(libc::EINVAL);
            }
        };
    }

    fn mknod(
        &mut self,
        req: &Request,
        parent: u64,
        name: &OsStr,
        mode: u32,
        rdev: u32,
        reply: ReplyEntry,
    ) {
        let entry_path = try_option!(self.child_path(parent, name), reply, libc::ENOENT);

        let file_type = match stat::SFlag::from_bits(mode) {
            Some(s_flag) => {
                if s_flag.contains(stat::SFlag::S_IFREG) {
                    FileType::File
                } else if s_flag.contains(stat::SFlag::S_IFCHR) {
                    let major = stat::major(rdev as u64);
                    let minor = stat::minor(rdev as u64);
                    FileType::Special(UnixSpecialType::CharacterDevice { major, minor })
                } else if s_flag.contains(stat::SFlag::S_IFBLK) {
                    let major = stat::major(rdev as u64);
                    let minor = stat::minor(rdev as u64);
                    FileType::Special(UnixSpecialType::BlockDevice { major, minor })
                } else if s_flag.contains(stat::SFlag::S_IFIFO) {
                    FileType::Special(UnixSpecialType::NamedPipe)
                } else if s_flag.contains(stat::SFlag::S_IFSOCK) {
                    // Sockets aren't supported by `FileRepo`. `mknod(2)` specifies that `EPERM`
                    // should be returned if the file system doesn't support the type of node being
                    // requested.
                    reply.error(libc::EPERM);
                    return;
                } else {
                    // Other file types aren't supported by `mknod`.
                    reply.error(libc::EINVAL);
                    return;
                }
            }
            None => {
                // The file mode could not be parsed as a valid file type.
                reply.error(libc::EINVAL);
                return;
            }
        };

        let entry = Entry::new(file_type, req);

        try_result!(self.repo.create(&entry_path, &entry), reply);

        let entry_inode = self.inodes.insert(entry_path);
        let attr = try_result!(self.entry_attr(&entry, entry_inode, req), reply);
        let generation = self.inodes.generation(entry_inode);

        reply.entry(&DEFAULT_TTL, &attr, generation);
    }

    fn mkdir(&mut self, req: &Request, parent: u64, name: &OsStr, mode: u32, reply: ReplyEntry) {
        let entry_path = try_option!(self.child_path(parent, name), reply, libc::ENOENT);

        let mut entry = Entry::new(FileType::Directory, req);
        let metadata = entry.metadata.as_mut().unwrap();
        metadata.mode = mode;

        try_result!(self.repo.create(&entry_path, &entry), reply);

        let entry_inode = self.inodes.insert(entry_path);
        let attr = try_result!(self.entry_attr(&entry, entry_inode, req), reply);
        let generation = self.inodes.generation(entry_inode);

        reply.entry(&DEFAULT_TTL, &attr, generation);
    }

    fn unlink(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let entry_path = try_option!(self.child_path(parent, name), reply, libc::ENOENT);

        if self.repo.is_directory(&entry_path) {
            reply.error(libc::EISDIR);
            return;
        }

        try_result!(self.repo.remove(&entry_path), reply);

        let entry_inode = self.inodes.inode(&entry_path).unwrap();
        self.inodes.remove(entry_inode);

        reply.ok();
    }

    fn rmdir(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let entry_path = try_option!(self.child_path(parent, name), reply, libc::ENOENT);

        if !self.repo.is_directory(&entry_path) {
            reply.error(libc::ENOTDIR);
            return;
        }

        // `FileRepo::remove` method checks that the directory entry is empty.
        try_result!(self.repo.remove(&entry_path), reply);

        let entry_inode = self.inodes.inode(&entry_path).unwrap();
        self.inodes.remove(entry_inode);

        reply.ok();
    }

    fn symlink(
        &mut self,
        req: &Request,
        parent: u64,
        name: &OsStr,
        link: &Path,
        reply: ReplyEntry,
    ) {
        let entry_path = try_option!(self.child_path(parent, name), reply, libc::ENOENT);

        let entry = Entry::new(
            FileType::Special(UnixSpecialType::SymbolicLink {
                target: link.to_owned(),
            }),
            req,
        );

        try_result!(self.repo.create(&entry_path, &entry), reply);

        let entry_inode = self.inodes.insert(entry_path);
        let attr = try_result!(self.entry_attr(&entry, entry_inode, req), reply);
        let generation = self.inodes.generation(entry_inode);

        reply.entry(&DEFAULT_TTL, &attr, generation);
    }

    fn rename(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        reply: ReplyEmpty,
    ) {
        let source_path = try_option!(self.child_path(parent, name), reply, libc::ENOENT);
        let dest_path = try_option!(self.child_path(newparent, newname), reply, libc::ENOENT);

        if !self.repo.exists(&source_path) {
            reply.error(libc::ENOENT);
            return;
        }

        // We cannot make a directory a subdirectory of itself.
        if dest_path.starts_with(&source_path) {
            reply.error(libc::EINVAL);
            return;
        }

        // Check if the parent of the destination path is not a directory.
        if !self.repo.is_directory(&dest_path.parent().unwrap()) {
            reply.error(libc::ENOTDIR);
            return;
        }

        // Remove the destination path unless it is a non-empty directory.
        if let Err(error @ crate::Error::NotEmpty) = self.repo.remove(&dest_path) {
            reply.error(error.to_errno());
            return;
        }

        // We've already checked all the possible error conditions.
        self.repo.copy(&source_path, &dest_path).ok();

        reply.ok();
    }

    fn link(
        &mut self,
        _req: &Request,
        _ino: u64,
        _newparent: u64,
        _newname: &OsStr,
        reply: ReplyEntry,
    ) {
        reply.error(libc::ENOSYS);
    }

    fn open(&mut self, _req: &Request, ino: u64, flags: u32, reply: ReplyOpen) {
        let flags = try_option!(OFlag::from_bits(flags as i32), reply, libc::EINVAL);

        if flags.intersects(*UNSUPPORTED_OPEN_FLAGS) {
            reply.error(libc::ENOTSUP);
            return;
        }

        let entry_path = try_option!(self.inodes.path(ino), reply, libc::ENOENT);

        if !self.repo.is_file(&entry_path) {
            reply.error(libc::ENOTSUP);
            return;
        }

        let fh = self.handles.open(flags, HandleType::File);
        reply.opened(fh, 0);
    }

    fn read(&mut self, req: &Request, ino: u64, fh: u64, offset: i64, size: u32, reply: ReplyData) {
        let flags = match self.handles.info(fh) {
            Some(HandleInfo {
                handle_type: HandleType::Directory,
                ..
            }) => {
                reply.error(libc::EISDIR);
                return;
            }
            Some(info) => info.flags,
            None => {
                reply.error(libc::EBADF);
                return;
            }
        };

        // Technically, on Unix systems, a file should still be accessible via its file descriptor
        // once it's been unlinked. Because this isn't how repositories work, we will return `EBADF`
        // if the user tries to read from a file which has been unlinked since it was opened.
        let entry_path = match self.inodes.path(ino) {
            Some(path) => path.to_owned(),
            None => {
                self.handles.close(fh);
                reply.error(libc::EBADF);
                return;
            }
        };

        let object = match self.objects.entry(ino) {
            HashMapEntry::Occupied(entry) => {
                let object = entry.into_mut();
                // We must commit changes in case this object has a transaction in progress.
                try_result!(object.commit(), reply);
                object
            }
            HashMapEntry::Vacant(entry) => entry.insert(self.repo.open(&entry_path).unwrap()),
        };

        try_result!(object.seek(SeekFrom::Start(offset as u64)), reply);

        // `Filesystem::read` should read the exact number of bytes requested except on EOF or error.
        let mut buffer = Vec::with_capacity(size as usize);
        let mut bytes_read;
        let mut total_bytes_read = 0;
        loop {
            bytes_read = try_result!(
                object.read(&mut buffer[total_bytes_read..size as usize]),
                reply
            );
            total_bytes_read += bytes_read;

            if bytes_read == 0 {
                // Either the object has reached EOF or we've already read `size` bytes from it.
                break;
            }
        }

        // Update the file's `st_atime` unless the `O_NOATIME` flag was passed.
        if !flags.contains(OFlag::O_NOATIME) {
            let mut metadata =
                try_result!(self.repo.entry(&entry_path), reply).metadata_or_default(req);
            metadata.accessed = SystemTime::now();
            try_result!(self.repo.set_metadata(&entry_path, Some(metadata)), reply);
        }

        reply.data(&buffer[..total_bytes_read]);
    }

    fn write(
        &mut self,
        req: &Request,
        ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _flags: u32,
        reply: ReplyWrite,
    ) {
        let flags = match self.handles.info(fh) {
            Some(HandleInfo {
                handle_type: HandleType::Directory,
                ..
            }) => {
                reply.error(libc::EISDIR);
                return;
            }
            Some(info) => info.flags,
            None => {
                reply.error(libc::EBADF);
                return;
            }
        };

        // Technically, on Unix systems, a file should still be accessible via its file descriptor
        // once it's been unlinked. Because this isn't how repositories work, we will return `EBADF`
        // if the user tries to read from a file which has been unlinked since it was opened.
        let entry_path = match self.inodes.path(ino) {
            Some(path) => path.to_owned(),
            None => {
                self.handles.close(fh);
                reply.error(libc::EBADF);
                return;
            }
        };

        let object = match self.objects.entry(ino) {
            HashMapEntry::Occupied(entry) => entry.into_mut(),
            HashMapEntry::Vacant(entry) => entry.insert(self.repo.open(&entry_path).unwrap()),
        };

        if flags.contains(OFlag::O_APPEND) {
            try_result!(object.seek(SeekFrom::End(0)), reply);
        } else {
            try_result!(object.seek(SeekFrom::Start(offset as u64)), reply);
        }

        let mut metadata =
            try_result!(self.repo.entry(&entry_path), reply).metadata_or_default(req);

        let bytes_written = try_result!(object.write(data), reply);

        // After this point, we need to be more careful about error handling. Because bytes have
        // been written to the object, if an error occurs, we need to drop the `Object` to discard
        // any uncommitted changes before returning so that bytes will only have been written to the
        // object if this method returns successfully.

        // Update the `st_atime` and `st_mtime` for the entry.
        metadata.accessed = SystemTime::now();
        metadata.modified = SystemTime::now();
        if let Err(error) = self.repo.set_metadata(&entry_path, Some(metadata)) {
            self.objects.remove(&ino);
            reply.error(error.to_errno());
            return;
        }

        // If the `O_SYNC` or `O_DSYNC` flags were passed, we need to commit changes to the object
        // *and* commit changes to the repository after each write.
        if flags.intersects(OFlag::O_SYNC | OFlag::O_DSYNC) {
            if let Err(error) = object.commit() {
                self.objects.remove(&ino);
                reply.error(error.to_errno());
                return;
            }

            if let Err(error) = self.repo.commit() {
                self.objects.remove(&ino);
                reply.error(error.to_errno());
                return;
            }
        }

        reply.written(bytes_written as u32);
    }

    fn flush(&mut self, _req: &Request, ino: u64, _fh: u64, _lock_owner: u64, reply: ReplyEmpty) {
        if let Some(object) = self.objects.get_mut(&ino) {
            try_result!(object.commit(), reply);
        }
        reply.ok()
    }

    fn release(
        &mut self,
        _req: &Request,
        _ino: u64,
        fh: u64,
        _flags: u32,
        _lock_owner: u64,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.handles.close(fh);
        reply.ok()
    }

    fn fsync(&mut self, _req: &Request, ino: u64, _fh: u64, _datasync: bool, reply: ReplyEmpty) {
        if let Some(object) = self.objects.get_mut(&ino) {
            try_result!(object.commit(), reply);
        }
        try_result!(self.repo.commit(), reply);
        reply.ok();
    }

    fn opendir(&mut self, _req: &Request, ino: u64, flags: u32, reply: ReplyOpen) {
        let flags = try_option!(OFlag::from_bits(flags as i32), reply, libc::EINVAL);

        let entry_path = try_option!(self.inodes.path(ino), reply, libc::ENOENT);

        if !self.repo.is_directory(entry_path) {
            reply.error(libc::ENOTDIR);
            return;
        }

        let mut children = Vec::new();
        for child_path in try_result!(self.repo.list(entry_path), reply) {
            let file_name = child_path.file_name().unwrap().to_string();
            let inode = self.inodes.inode(&child_path).unwrap();
            let file_type = try_result!(self.repo.entry(&child_path), reply)
                .file_type
                .to_file_type();
            children.push(DirectoryEntry {
                file_name,
                file_type,
                inode,
            })
        }

        let fh = self.handles.open(flags, HandleType::Directory);
        self.directories.insert(fh, children);

        reply.opened(fh, 0);
    }

    fn readdir(
        &mut self,
        _req: &Request,
        _ino: u64,
        fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        match self.handles.info(fh) {
            Some(HandleInfo {
                handle_type: HandleType::File,
                ..
            }) => {
                reply.error(libc::ENOTDIR);
                return;
            }
            None => {
                reply.error(libc::EBADF);
                return;
            }
            _ => {}
        }

        let children = self.directories.get(&fh).unwrap();

        for (i, dir_entry) in children[offset as usize..].iter().enumerate() {
            if reply.add(
                dir_entry.inode,
                (i + 1) as i64,
                dir_entry.file_type,
                &dir_entry.file_name,
            ) {
                break;
            }
        }

        reply.ok();
    }

    fn releasedir(&mut self, _req: &Request, _ino: u64, fh: u64, _flags: u32, reply: ReplyEmpty) {
        self.handles.close(fh);
        reply.ok()
    }

    fn fsyncdir(
        &mut self,
        _req: &Request,
        _ino: u64,
        _fh: u64,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        try_result!(self.repo.commit(), reply);
        reply.ok();
    }
}