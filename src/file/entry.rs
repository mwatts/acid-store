/*
 * Copyright 2019 Garrett Powell
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

use std::collections::HashMap;
use std::path::PathBuf;

use chrono::NaiveDateTime;

use crate::DataHandle;

/// A type of file which can be stored in an archive.
pub enum EntryType {
    /// A regular file.
    File {
        /// The contents of the file.
        data: DataHandle,
    },

    /// A directory.
    Directory,

    /// A symbolic link.
    ///
    /// The link target is stored as a platform-dependent path, so a symlink archived on one system
    /// may not be able to be extracted on another.
    Link {
        /// The file the symbolic link points to.
        target: PathBuf,
    },
}

/// Metadata about a file stored in an archive.
pub struct ArchiveEntry {
    /// The time the file was last modified.
    pub modified_time: NaiveDateTime,

    /// The POSIX permissions of the file, or `None` if POSIX permissions are not applicable.
    pub permissions: Option<i32>,

    /// The file's extended attributes.
    pub attributes: HashMap<String, Vec<u8>>,

    /// The type of file this entry represents.
    pub entry_type: EntryType,
}
