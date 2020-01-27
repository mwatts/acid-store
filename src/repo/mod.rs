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

//! High-level abstractions for data storage.

pub use file::{FileMetadata, FileRepository, FileType};
pub use object::{
    Compression, ContentId, Encryption, Key, LockStrategy, Object, ObjectRepository,
    RepositoryConfig, RepositoryInfo, RepositoryStats, ResourceLimit,
};
pub use value::{ValueKey, ValueRepository};
pub use version::{ReadOnlyObject, Version, VersionRepository};

mod file;
mod object;
mod value;
mod version;