use std::fmt::Debug;
use std::io;
use std::num::NonZeroU64;

use common::{BinarySerializable, VInt};
use log::warn;

use super::monotonic_mapping::{
    StrictlyMonotonicFn, StrictlyMonotonicMappingToInternal,
    StrictlyMonotonicMappingToInternalGCDBaseval,
};
use super::{
    monotonic_map_column, u64_based, ColumnValues, MonotonicallyMappableToU64,
    U128FastFieldCodecType,
};
use crate::column_values::compact_space::CompactSpaceCompressor;
use crate::column_values::u64_based::CodecType;
use crate::iterable::Iterable;

/// The normalized header gives some parameters after applying the following
/// normalization of the vector:
/// `val -> (val - min_value) / gcd`
///
/// By design, after normalization, `min_value = 0` and `gcd = 1`.
#[derive(Debug, Copy, Clone)]
pub struct NormalizedHeader {
    /// The number of values in the underlying column.
    pub num_vals: u32,
    /// The max value of the underlying column.
    pub max_value: u64,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct U128Header {
    pub num_vals: u32,
    pub codec_type: U128FastFieldCodecType,
}

impl BinarySerializable for U128Header {
    fn serialize<W: io::Write + ?Sized>(&self, writer: &mut W) -> io::Result<()> {
        VInt(self.num_vals as u64).serialize(writer)?;
        self.codec_type.serialize(writer)?;
        Ok(())
    }

    fn deserialize<R: io::Read>(reader: &mut R) -> io::Result<Self> {
        let num_vals = VInt::deserialize(reader)?.0 as u32;
        let codec_type = U128FastFieldCodecType::deserialize(reader)?;
        Ok(U128Header {
            num_vals,
            codec_type,
        })
    }
}

fn normalize_column<C: ColumnValues>(
    from_column: C,
    min_value: u64,
    gcd: Option<NonZeroU64>,
) -> impl ColumnValues {
    let gcd = gcd.map(|gcd| gcd.get()).unwrap_or(1);
    let mapping = StrictlyMonotonicMappingToInternalGCDBaseval::new(gcd, min_value);
    monotonic_map_column(from_column, mapping)
}

/// Serializes u128 values with the compact space codec.
pub fn serialize_column_values_u128(
    iterable: &dyn Iterable<u128>,
    num_vals: u32,
    output: &mut impl io::Write,
) -> io::Result<()> {
    let header = U128Header {
        num_vals,
        codec_type: U128FastFieldCodecType::CompactSpace,
    };
    header.serialize(output)?;
    let compressor = CompactSpaceCompressor::train_from(iterable.boxed_iter(), num_vals);
    compressor.compress_into(iterable.boxed_iter(), output)?;

    Ok(())
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::column_values::u64_based::{
        self, serialize_and_load_u64_based_column_values, serialize_u64_based_column_values,
        ALL_U64_CODEC_TYPES,
    };

    #[test]
    fn test_serialize_deserialize_u128_header() {
        let original = U128Header {
            num_vals: 11,
            codec_type: U128FastFieldCodecType::CompactSpace,
        };
        let mut out = Vec::new();
        original.serialize(&mut out).unwrap();
        let restored = U128Header::deserialize(&mut &out[..]).unwrap();
        assert_eq!(restored, original);
    }

    #[test]
    fn test_serialize_deserialize() {
        let original = [1u64, 5u64, 10u64];
        let restored: Vec<u64> =
            serialize_and_load_u64_based_column_values(&&original[..], &ALL_U64_CODEC_TYPES)
                .iter()
                .collect();
        assert_eq!(&restored, &original[..]);
    }

    #[test]
    fn test_fastfield_bool_size_bitwidth_1() {
        let mut buffer = Vec::new();
        serialize_u64_based_column_values(
            || [false, true].into_iter(),
            &ALL_U64_CODEC_TYPES,
            &mut buffer,
        )
        .unwrap();
        // TODO put the header as a footer so that it serves as a padding.
        // 5 bytes of header, 1 byte of value, 7 bytes of padding.
        assert_eq!(buffer.len(), 5 + 1);
    }

    #[test]
    fn test_fastfield_bool_bit_size_bitwidth_0() {
        let mut buffer = Vec::new();
        serialize_u64_based_column_values(
            || [false, true].into_iter(),
            &ALL_U64_CODEC_TYPES,
            &mut buffer,
        )
        .unwrap();
        // 6 bytes of header, 0 bytes of value, 7 bytes of padding.
        assert_eq!(buffer.len(), 6);
    }

    #[test]
    fn test_fastfield_gcd() {
        let mut buffer = Vec::new();
        let vals: Vec<u64> = (0..80).map(|val| (val % 7) * 1_000u64).collect();
        serialize_u64_based_column_values(
            || vals.iter().cloned(),
            &[CodecType::Bitpacked],
            &mut buffer,
        )
        .unwrap();
        // Values are stored over 3 bits.
        assert_eq!(buffer.len(), 6 + (3 * 80 / 8));
    }
}
