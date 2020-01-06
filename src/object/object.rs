/*
 * Copyright 2019 Wren Powell
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

use std::cmp::min;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::mem::replace;

use blake2::digest::{Input, VariableOutput};
use blake2::VarBlake2b;
use cdchunking::ZPAQ;
use serde::{Deserialize, Serialize};

use crate::DataStore;

use super::chunking::IncrementalChunker;
use super::header::Key;
use super::repository::ObjectRepository;

/// The size of the checksums used for uniquely identifying chunks.
pub const CHUNK_HASH_SIZE: usize = 32;

/// A 256-bit checksum used for uniquely identifying a chunk.
pub type ChunkHash = [u8; CHUNK_HASH_SIZE];

/// Compute the BLAKE2 checksum of the given `data` and return the result.
pub fn chunk_hash(data: &[u8]) -> ChunkHash {
    let mut hasher = VarBlake2b::new(CHUNK_HASH_SIZE).unwrap();
    hasher.input(data);
    let mut checksum = [0u8; CHUNK_HASH_SIZE];
    hasher.variable_result(|result| checksum.copy_from_slice(result));
    checksum
}

/// A chunk of data generated by the chunking algorithm.
#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Chunk {
    /// The size of the chunk in bytes.
    pub size: usize,

    /// The checksum of the chunk.
    pub hash: ChunkHash,
}

/// The location of a chunk in a stream of bytes.
#[derive(Debug, PartialEq, Eq, Clone, Default)]
struct ChunkLocation {
    /// The chunk itself.
    pub chunk: Chunk,

    /// The offset of the start of the chunk from the beginning of the object.
    pub start: u64,

    /// The offset of the end of the chunk from the beginning of the object.
    pub end: u64,

    /// The offset of the seek position from the beginning of the object.
    pub position: u64,

    /// The index of the chunk in the list of chunks.
    pub index: usize,
}

impl ChunkLocation {
    /// The offset of the seek position from the beginning of the chunk.
    fn relative_position(&self) -> usize {
        (self.position - self.start) as usize
    }
}

/// A handle for accessing data in a repository.
///
/// An `Object` doesn't own or store data itself, but references data stored in a repository.
#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
pub struct ObjectHandle {
    /// The original size of the data in bytes.
    pub size: u64,

    /// The checksums of the chunks which make up the data.
    pub chunks: Vec<Chunk>,
}

impl Default for ObjectHandle {
    fn default() -> Self {
        Self {
            size: 0,
            chunks: Vec::new(),
        }
    }
}

/// A handle for accessing data in a repository.
///
/// An `Object` represents the data associated with a key in an `ObjectRepository`. It implements
/// `Read`, `Write`, and `Seek` for reading and writing the data in the repository.
///
/// Data written to an `Object` is automatically flushed when the value is dropped, but any errors
/// that occur in the `Drop` implementation are ignored. If these errors need to be handled, it is
/// advisable to call `flush` explicitly after all data has been written.
pub struct Object<'a, K: Key, S: DataStore> {
    /// The repository where chunks are stored.
    repository: &'a mut ObjectRepository<K, S>,

    /// The key which represents this object.
    key: K,

    /// A value responsible for buffering and chunking data which has been written.
    chunker: IncrementalChunker<ZPAQ>,

    /// The list of chunks which have been written since `flush` was last called.
    new_chunks: Vec<Chunk>,

    /// The location of the first chunk written to since `flush` was last called.
    start_location: ChunkLocation,

    /// The current seek position of the object.
    position: u64,
}

impl<'a, K: Key, S: DataStore> Object<'a, K, S> {
    pub(super) fn new(repository: &'a mut ObjectRepository<K, S>, key: K, chunker_bits: usize) -> Self {
        Self {
            repository,
            key,
            chunker: IncrementalChunker::new(ZPAQ::new(chunker_bits)),
            new_chunks: Vec::new(),
            // The initial value is unimportant.
            start_location: Default::default(),
            position: 0,
        }
    }

    /// Get the object handle for the object associated with `key`.
    fn get_handle(&self) -> &ObjectHandle {
        self.repository.get_handle(&self.key)
    }

    /// Get the object handle for the object associated with `key`.
    fn get_handle_mut(&mut self) -> &mut ObjectHandle {
        self.repository.get_handle_mut(&self.key)
    }

    /// Return the size of the object in bytes.
    pub fn size(&self) -> u64 {
        self.repository.get_handle(&self.key).size
    }

    /// Truncate the object to the given `length`.
    ///
    /// If the given `length` is greater than or equal to the current size of the object, this does
    /// nothing. This moves the seek position to the new end of the object.
    pub fn truncate(&mut self, length: u64) -> io::Result<()> {
        // We need to flush changes before truncating the object.
        self.flush()?;

        if length >= self.get_handle().size {
            return Ok(());
        }

        // Truncating the object may mean slicing a chunk in half. Because we can't edit chunks
        // in-place, we need to read the final chunk, slice it, and write it back.
        self.position = length;
        let end_location = self.current_chunk();
        let last_chunk = self.repository.read_chunk(&end_location.chunk.hash)?;
        let new_last_chunk = &last_chunk[..end_location.relative_position()];
        let new_last_chunk = Chunk {
            hash: self.repository.write_chunk(&new_last_chunk)?,
            size: new_last_chunk.len(),
        };

        // Remove all chunks including and after the final chunk.
        self.get_handle_mut().chunks.drain(end_location.index..);

        // Append the new final chunk which has been sliced.
        self.get_handle_mut().chunks.push(new_last_chunk);

        Ok(())
    }

    /// Verify the integrity of the data in this object.
    ///
    /// This returns `true` if the object is valid and `false` if it is corrupt.
    pub fn verify(&self) -> io::Result<bool> {
        for expected_chunk in &self.get_handle().chunks {
            let data = self.repository.read_chunk(&expected_chunk.hash)?;
            let actual_checksum = chunk_hash(&data);
            if expected_chunk.hash != actual_checksum {
                return Ok(false);
            }
        }

        Ok(true)
    }

    /// Return the location of the chunk at the current seek position.
    fn current_chunk(&self) -> ChunkLocation {
        let mut chunk_start = 0u64;
        let mut chunk_end = 0u64;

        for (index, chunk) in self.get_handle().chunks.iter().enumerate() {
            chunk_end += chunk.size as u64;
            if self.position >= chunk_start && self.position <= chunk_end {
                return ChunkLocation {
                    chunk: *chunk,
                    start: chunk_start,
                    end: chunk_end,
                    position: self.position,
                    index,
                };
            }
            chunk_start += chunk.size as u64;
        }

        panic!("The current seek position is past the end of the object.")
    }

    /// Return the slice of bytes between the current seek position and the end of the chunk.
    ///
    /// The returned slice will be no longer than `size`.
    fn read_chunk(&self, size: usize) -> io::Result<Vec<u8>> {
        let chunk_location = self.current_chunk();
        let start = chunk_location.relative_position();
        let end = min(start + size, chunk_location.chunk.size as usize);
        let chunk_data = self.repository.read_chunk(&chunk_location.chunk.hash)?;
        Ok(chunk_data[start..end].to_vec())
    }

    /// Write chunks stored in the chunker to the repository.
    fn write_chunks(&mut self) -> io::Result<()> {
        for chunk in self.chunker.chunks() {
            let hash = self.repository.write_chunk(&chunk)?;
            self.new_chunks.push(Chunk {
                hash,
                size: chunk.len(),
            });
        }
        Ok(())
    }
}

impl<'a, K: Key, S: DataStore> Seek for Object<'a, K, S> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        // We need to flush changes before writing to a different part of the file.
        self.flush()?;
        let object_size = self.get_handle().size;

        let new_position = match pos {
            SeekFrom::Start(offset) => min(object_size, offset),
            SeekFrom::End(offset) => {
                if offset > object_size as i64 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "Attempted to seek to a negative offset.",
                    ));
                } else {
                    min(object_size, (object_size as i64 - offset) as u64)
                }
            }
            SeekFrom::Current(offset) => {
                if self.position as i64 + offset < 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "Attempted to seek to a negative offset.",
                    ));
                } else {
                    min(object_size, (self.position as i64 + offset) as u64)
                }
            }
        };

        self.position = new_position;
        Ok(new_position)
    }
}

// Content-defined chunking makes writing and seeking more complicated. Chunks can't be modified
// in-place; they can only be read or written in their entirety. This means we need to do a lot of
// buffering to wait for a chunk boundary before writing a chunk to the repository. It also means we
// need to flush changes before writing to a different part of the file.
impl<'a, K: Key, S: DataStore> Write for Object<'a, K, S> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Check if this is the first time `write` is being called after calling `flush`.
        if self.new_chunks.is_empty() {
            // We need to make sure the data before the seek position is saved when we replace the
            // chunk. Read this data from the repository and write it to the chunker.
            self.start_location = self.current_chunk();
            let first_chunk = self
                .repository
                .read_chunk(&self.start_location.chunk.hash)?;
            self.chunker
                .write_all(&first_chunk[..self.start_location.relative_position()])?;
        }

        // Chunk the data and write any complete chunks to the repository.
        self.chunker.write_all(buf)?;
        self.write_chunks()?;

        // Advance the seek position.
        self.position += buf.len() as u64;

        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        // We need to make sure the data after the seek position is saved when we replace the chunk.
        // Read this data from the repository and write it to the chunker.
        let end_location = self.current_chunk();
        let last_chunk = self.repository.read_chunk(&end_location.chunk.hash)?;
        self.chunker
            .write_all(&last_chunk[end_location.relative_position()..])?;

        // Write all the remaining data in the chunker to the repository.
        self.chunker.flush()?;
        self.write_chunks()?;

        // Replace the chunk references in the object handle to reflect the changes.
        let chunk_range = self.start_location.index..end_location.index;
        let remaining_chunks = replace(&mut self.new_chunks, Vec::new());
        self
            .get_handle_mut()
            .chunks
            .splice(chunk_range, remaining_chunks);

        // Update the size of the object in the object handle to reflect changes.
        self.get_handle_mut().size = self
            .get_handle_mut()
            .chunks
            .iter()
            .fold(0, |sum, chunk| sum + chunk.size as u64);

        Ok(())
    }
}

impl<'a, K: Key, S: DataStore> Read for Object<'a, K, S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let next_chunk = self.read_chunk(buf.len())?;
        buf.copy_from_slice(next_chunk.as_slice());
        self.position += next_chunk.len() as u64;
        Ok(next_chunk.len())
    }
}

impl<'a, K: Key, S: DataStore> Drop for Object<'a, K, S> {
    fn drop(&mut self) {
        // Explicitly discard the `Err` because we can't handle it here.
        // This behavior is documented.
        self.flush().ok();
    }
}
