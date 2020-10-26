/*
 * Copyright 2019-2020 Wren Powell
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

//! An object store with support for content versioning.
//!
//! This module contains the `VersionRepo` repository type.
//!
//! This repository is an object store like `KeyRepo`, except it supports storing multiple
//! versions of each object. The current version of each object is mutable, while past versions are
//! read-only.
//!
//! Like other repositories, changes made to the repository are not persisted to the data store
//! until `VersionRepo::commit` is called. For details about deduplication, compression, encryption,
//! and locking, see the module-level documentation for `acid_store::repo`.
//!
//! # Examples
//! Create a version of an object, delete the object's contents, and then restore from the version.
//! ```
//!     use std::io::{Read, Write};
//!
//!     use acid_store::repo::{OpenOptions, Object, version::VersionRepo, RepoConfig};
//!     use acid_store::store::MemoryStore;
//!
//!     fn main() -> acid_store::Result<()> {
//!         let mut repository = OpenOptions::new(MemoryStore::new())
//!             .create_new::<VersionRepo<String, _>>()?;
//!
//!         // Insert a new object and write some data to it.
//!         let mut object = repository.insert(String::from("Key")).unwrap();
//!         object.write_all(b"Original data")?;
//!         object.flush()?;
//!         drop(object);
//!
//!         // Create a new, read-only version of this object.
//!         let version = repository.create_version("Key").unwrap();
//!
//!         // Modify the current version of the object.
//!         let mut object = repository.object_mut("Key").unwrap();
//!         object.truncate(0)?;
//!         drop(object);
//!
//!         // Restore from the version we created earlier.
//!         repository.restore_version("Key", version.id());
//!
//!         // Check the contents.
//!         let mut object = repository.object("Key").unwrap();
//!         let mut contents = Vec::new();
//!         object.read_to_end(&mut contents)?;
//!
//!         assert_eq!(contents, b"Original data");
//!         Ok(())
//!     }
//!
//! ```

pub use self::repository::VersionRepo;
pub use self::version::Version;

mod repository;
mod version;
