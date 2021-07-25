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

use std::collections::hash_map;
use std::hash::Hash;
use std::iter::{ExactSizeIterator, FusedIterator};
use std::sync::{Arc, RwLock};

use serde::de::DeserializeOwned;
use serde::Serialize;

use super::handle::ObjectHandle;

/// A type which can be used as a key in a [`KeyRepo`].
///
/// [`KeyRepo`]: crate::repo::key::KeyRepo
pub trait Key: Eq + Hash + Clone + Serialize + DeserializeOwned {}

impl<T> Key for T where T: Eq + Hash + Clone + Serialize + DeserializeOwned {}

/// An iterator over the keys in a [`KeyRepo`].
///
/// This value is created by [`KeyRepo::keys`].
///
/// [`KeyRepo`]: crate::repo::key::KeyRepo
/// [`KeyRepo::keys`]: crate::repo::key::KeyRepo::keys
#[derive(Debug, Clone)]
pub struct Keys<'a, K>(pub(super) hash_map::Keys<'a, K, Arc<RwLock<ObjectHandle>>>);

impl<'a, K> Iterator for Keys<'a, K> {
    type Item = &'a K;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl<'a, K> FusedIterator for Keys<'a, K> {}

impl<'a, K> ExactSizeIterator for Keys<'a, K> {}
