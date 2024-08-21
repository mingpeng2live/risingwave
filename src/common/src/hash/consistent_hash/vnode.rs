// Copyright 2024 RisingWave Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::num::NonZero;
use std::sync::LazyLock;

use itertools::Itertools;
use parse_display::Display;

use crate::array::{Array, ArrayImpl, DataChunk};
use crate::hash::Crc32HashCode;
use crate::row::{Row, RowExt};
use crate::types::{DataType, Datum, DatumRef, ScalarImpl, ScalarRefImpl};
use crate::util::hash_util::Crc32FastBuilder;
use crate::util::row_id::extract_vnode_id_from_row_id;

/// `VirtualNode` (a.k.a. Vnode) is a minimal partition that a set of keys belong to. It is used for
/// consistent hashing.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Display)]
#[display("{0}")]
pub struct VirtualNode(VirtualNodeInner);

/// The internal representation of a virtual node id.
type VirtualNodeInner = u16;
// static_assertions::const_assert!(VirtualNodeInner::BITS >= VirtualNode::BITS as u32);

impl From<Crc32HashCode> for VirtualNode {
    fn from(hash_code: Crc32HashCode) -> Self {
        // Take the least significant bits of the hash code.
        // TODO: should we use the most significant bits?
        let inner = (hash_code.value() % Self::count() as u64) as VirtualNodeInner;
        VirtualNode(inner)
    }
}

impl VirtualNode {
    /// We may use `VirtualNode` as a datum in a stream, or store it as a column.
    /// Hence this reifies it as a RW datatype.
    pub const RW_TYPE: DataType = DataType::Int16;
    /// The size of a virtual node in bytes, in memory or serialized representation.
    pub const SIZE: usize = std::mem::size_of::<Self>();
    /// The minimum (zero) value of the virtual node.
    pub const ZERO: VirtualNode = unsafe { VirtualNode::from_index_unchecked(0) };
}

impl VirtualNode {
    /// The default count of virtual nodes.
    const DEFAULT_COUNT: usize = 1 << 8;
    /// The maximum count of virtual nodes, limited by the size of the inner type [`VirtualNodeInner`].
    const MAX_COUNT: usize = 1 << VirtualNodeInner::BITS;

    /// The total count of virtual nodes.
    ///
    /// It can be customized by the environment variable `RW_VNODE_COUNT`, or defaults to [`Self::DEFAULT_COUNT`].
    pub fn count() -> usize {
        // Cache the value to avoid repeated env lookups and parsing.
        static COUNT: LazyLock<usize> = LazyLock::new(|| {
            if let Ok(count) = std::env::var("RW_VNODE_COUNT") {
                let count = count
                    .parse::<NonZero<usize>>()
                    .expect("`RW_VNODE_COUNT` must be a positive integer")
                    .get();
                assert!(
                    count <= VirtualNode::MAX_COUNT,
                    "`RW_VNODE_COUNT` should not exceed maximum value {}",
                    VirtualNode::MAX_COUNT
                );
                // TODO(var-vnode): shall we enforce it to be a power of 2?
                count
            } else {
                VirtualNode::DEFAULT_COUNT
            }
        });

        *COUNT
    }

    /// The last virtual node in the range. It's derived from [`Self::count`].
    pub fn max() -> VirtualNode {
        VirtualNode::from_index(Self::count() - 1)
    }
}

/// An iterator over all virtual nodes.
pub type AllVirtualNodeIter = std::iter::Map<std::ops::Range<usize>, fn(usize) -> VirtualNode>;

impl VirtualNode {
    /// Creates a virtual node from the `usize` index.
    pub fn from_index(index: usize) -> Self {
        debug_assert!(index < Self::count());
        Self(index as _)
    }

    /// Creates a virtual node from the `usize` index without bounds checking.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the index is within the range of virtual nodes,
    /// i.e., less than [`Self::count`].
    pub const unsafe fn from_index_unchecked(index: usize) -> Self {
        Self(index as _)
    }

    /// Returns the `usize` the virtual node used for indexing.
    pub const fn to_index(self) -> usize {
        self.0 as _
    }

    /// Creates a virtual node from the given scalar representation. Used by `VNODE` expression.
    pub fn from_scalar(scalar: i16) -> Self {
        debug_assert!((scalar as usize) < Self::count());
        Self(scalar as _)
    }

    pub fn from_datum(datum: DatumRef<'_>) -> Self {
        Self::from_scalar(datum.expect("should not be none").into_int16())
    }

    /// Returns the scalar representation of the virtual node. Used by `VNODE` expression.
    pub const fn to_scalar(self) -> i16 {
        self.0 as _
    }

    pub const fn to_datum(self) -> Datum {
        Some(ScalarImpl::Int16(self.to_scalar()))
    }

    /// Creates a virtual node from the given big-endian bytes representation.
    pub fn from_be_bytes(bytes: [u8; Self::SIZE]) -> Self {
        let inner = VirtualNodeInner::from_be_bytes(bytes);
        debug_assert!((inner as usize) < Self::count());
        Self(inner)
    }

    /// Returns the big-endian bytes representation of the virtual node.
    pub const fn to_be_bytes(self) -> [u8; Self::SIZE] {
        self.0.to_be_bytes()
    }

    /// Iterates over all virtual nodes.
    pub fn all() -> AllVirtualNodeIter {
        (0..Self::count()).map(Self::from_index)
    }
}

impl VirtualNode {
    // `compute_chunk` is used to calculate the `VirtualNode` for the columns in the
    // chunk. When only one column is provided and its type is `Serial`, we consider the column to
    // be the one that contains RowId, and use a special method to skip the calculation of Hash
    // and directly extract the `VirtualNode` from `RowId`.
    pub fn compute_chunk(data_chunk: &DataChunk, keys: &[usize]) -> Vec<VirtualNode> {
        if let Ok(idx) = keys.iter().exactly_one()
            && let ArrayImpl::Serial(serial_array) = &**data_chunk.column_at(*idx)
        {
            return serial_array
                .iter()
                .enumerate()
                .map(|(idx, serial)| {
                    if let Some(serial) = serial {
                        extract_vnode_id_from_row_id(serial.as_row_id())
                    } else {
                        // NOTE: here it will hash the entire row when the `_row_id` is missing,
                        // which could result in rows from the same chunk being allocated to different chunks.
                        // This process doesn’t guarantee the order of rows, producing indeterminate results in some cases,
                        // such as when `distinct on` is used without an `order by`.
                        let (row, _) = data_chunk.row_at(idx);
                        row.hash(Crc32FastBuilder).into()
                    }
                })
                .collect();
        }

        data_chunk
            .get_hash_values(keys, Crc32FastBuilder)
            .into_iter()
            .map(|hash| hash.into())
            .collect()
    }

    // `compute_row` is used to calculate the `VirtualNode` for the corresponding columns in a
    // `Row`. Similar to `compute_chunk`, it also contains special handling for serial columns.
    pub fn compute_row(row: impl Row, indices: &[usize]) -> VirtualNode {
        let project = row.project(indices);
        if let Ok(Some(ScalarRefImpl::Serial(s))) = project.iter().exactly_one().as_ref() {
            return extract_vnode_id_from_row_id(s.as_row_id());
        }

        project.hash(Crc32FastBuilder).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::array::DataChunkTestExt;
    use crate::row::OwnedRow;
    use crate::util::row_id::RowIdGenerator;

    #[test]
    fn test_serial_key_chunk() {
        let mut gen = RowIdGenerator::new([VirtualNode::from_index(100)]);
        let chunk = format!(
            "SRL I
             {} 1
             {} 2",
            gen.next(),
            gen.next(),
        );

        let chunk = DataChunk::from_pretty(chunk.as_str());
        let vnodes = VirtualNode::compute_chunk(&chunk, &[0]);

        assert_eq!(
            vnodes.as_slice(),
            &[VirtualNode::from_index(100), VirtualNode::from_index(100)]
        );
    }

    #[test]
    fn test_serial_key_row() {
        let mut gen = RowIdGenerator::new([VirtualNode::from_index(100)]);
        let row = OwnedRow::new(vec![
            Some(ScalarImpl::Serial(gen.next().into())),
            Some(ScalarImpl::Int64(12345)),
        ]);

        let vnode = VirtualNode::compute_row(&row, &[0]);

        assert_eq!(vnode, VirtualNode::from_index(100));
    }

    #[test]
    fn test_serial_key_chunk_multiple_vnodes() {
        let mut gen = RowIdGenerator::new([100, 200].map(VirtualNode::from_index));
        let chunk = format!(
            "SRL I
             {} 1
             {} 2
             {} 3
             {} 4",
            gen.next(),
            gen.next(),
            gen.next(),
            gen.next(),
        );

        let chunk = DataChunk::from_pretty(chunk.as_str());
        let vnodes = VirtualNode::compute_chunk(&chunk, &[0]);

        assert_eq!(
            vnodes.as_slice(),
            &[
                VirtualNode::from_index(100),
                VirtualNode::from_index(200),
                VirtualNode::from_index(100),
                VirtualNode::from_index(200),
            ]
        );
    }
}
