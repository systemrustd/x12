use x12_protocol::x11::AtomId;

pub const MAX_PROPERTY_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PropertyValue {
    pub r#type: AtomId,
    pub format: PropertyFormat,
    pub data: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PropertyFormat {
    F8,
    F16,
    F32,
}

impl PropertyFormat {
    #[must_use]
    pub fn from_protocol(v: u8) -> Option<Self> {
        match v {
            8 => Some(Self::F8),
            16 => Some(Self::F16),
            32 => Some(Self::F32),
            _ => None,
        }
    }

    #[must_use]
    pub fn bytes(self) -> usize {
        match self {
            Self::F8 => 1,
            Self::F16 => 2,
            Self::F32 => 4,
        }
    }

    #[must_use]
    pub fn protocol_value(self) -> u8 {
        match self {
            Self::F8 => 8,
            Self::F16 => 16,
            Self::F32 => 32,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChangeMode {
    Replace,
    Prepend,
    Append,
}

impl ChangeMode {
    #[must_use]
    pub fn from_protocol(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Replace),
            1 => Some(Self::Prepend),
            2 => Some(Self::Append),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChangePropertyError {
    BadValue,
    BadMatch,
    BadAlloc,
}

/// Compute the new property value for a `ChangeProperty` request.
///
/// # Errors
/// - `BadMatch` when Prepend/Append type or format don't match existing.
/// - `BadAlloc` when the resulting data would exceed `MAX_PROPERTY_BYTES`.
pub fn apply_change(
    existing: Option<&PropertyValue>,
    mode: ChangeMode,
    new_type: AtomId,
    format: PropertyFormat,
    data: &[u8],
) -> Result<PropertyValue, ChangePropertyError> {
    let combined: Vec<u8> = match (mode, existing) {
        (ChangeMode::Replace, _) | (_, None) => data.to_vec(),
        (ChangeMode::Prepend | ChangeMode::Append, Some(v)) => {
            if v.r#type != new_type || v.format != format {
                return Err(ChangePropertyError::BadMatch);
            }
            let mut combined = Vec::with_capacity(v.data.len() + data.len());
            match mode {
                ChangeMode::Prepend => {
                    combined.extend_from_slice(data);
                    combined.extend_from_slice(&v.data);
                }
                ChangeMode::Append => {
                    combined.extend_from_slice(&v.data);
                    combined.extend_from_slice(data);
                }
                ChangeMode::Replace => unreachable!(),
            }
            combined
        }
    };
    if combined.len() > MAX_PROPERTY_BYTES {
        return Err(ChangePropertyError::BadAlloc);
    }
    Ok(PropertyValue {
        r#type: new_type,
        format,
        data: combined,
    })
}

#[derive(Debug)]
pub struct GetPropertySlice<'a> {
    pub r#type: AtomId,
    pub format: u8,
    pub bytes_after: u32,
    pub value: &'a [u8],
}

/// Compute the partial slice and metadata for a `GetProperty` reply.
///
/// `long_offset` and `long_length` are in 4-byte units (X11 convention).
///
/// # Errors
/// - `BadValue` when `long_offset * 4` exceeds the property's size.
#[allow(clippy::cast_possible_truncation, clippy::elidable_lifetime_names)]
pub fn slice_for_get<'a>(
    property: Option<&'a PropertyValue>,
    requested_type: AtomId,
    long_offset: u32,
    long_length: u32,
) -> Result<GetPropertySlice<'a>, ChangePropertyError> {
    let Some(p) = property else {
        return Ok(GetPropertySlice {
            r#type: AtomId(0),
            format: 0,
            bytes_after: 0,
            value: &[],
        });
    };
    let total = p.data.len() as u64;

    let any = requested_type.0 == 0;
    let matches = any || requested_type == p.r#type;
    if !matches {
        return Ok(GetPropertySlice {
            r#type: p.r#type,
            format: p.format.protocol_value(),
            bytes_after: total as u32,
            value: &[],
        });
    }

    let offset_bytes = u64::from(long_offset)
        .checked_mul(4)
        .ok_or(ChangePropertyError::BadValue)?;
    if offset_bytes > total {
        return Err(ChangePropertyError::BadValue);
    }
    let remaining = total - offset_bytes;
    let want_bytes = u64::from(long_length)
        .checked_mul(4)
        .ok_or(ChangePropertyError::BadValue)?;
    let mut len_to_return = remaining.min(want_bytes);

    let unit = p.format.bytes() as u64;
    len_to_return -= len_to_return % unit;

    let start = offset_bytes as usize;
    let end = start + len_to_return as usize;
    let bytes_after = (remaining - len_to_return) as u32;
    Ok(GetPropertySlice {
        r#type: p.r#type,
        format: p.format.protocol_value(),
        bytes_after,
        value: &p.data[start..end],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    mod apply_change_tests {
        use super::*;
        use x12_protocol::x11::AtomId;

        fn val(t: u32, f: PropertyFormat, data: Vec<u8>) -> PropertyValue {
            PropertyValue {
                r#type: AtomId(t),
                format: f,
                data,
            }
        }

        #[test]
        fn replace_on_empty() {
            let result = apply_change(
                None,
                ChangeMode::Replace,
                AtomId(31),
                PropertyFormat::F8,
                b"hello",
            )
            .unwrap();
            assert_eq!(result, val(31, PropertyFormat::F8, b"hello".to_vec()));
        }

        #[test]
        fn replace_ignores_existing_type_and_format() {
            let existing = val(31, PropertyFormat::F8, b"old".to_vec());
            let result = apply_change(
                Some(&existing),
                ChangeMode::Replace,
                AtomId(4),
                PropertyFormat::F32,
                &[1, 2, 3, 4],
            )
            .unwrap();
            assert_eq!(result, val(4, PropertyFormat::F32, vec![1, 2, 3, 4]));
        }

        #[test]
        fn append_on_empty_acts_like_replace() {
            let result = apply_change(
                None,
                ChangeMode::Append,
                AtomId(31),
                PropertyFormat::F8,
                b"hi",
            )
            .unwrap();
            assert_eq!(result.data, b"hi".to_vec());
        }

        #[test]
        fn prepend_on_empty_acts_like_replace() {
            let result = apply_change(
                None,
                ChangeMode::Prepend,
                AtomId(31),
                PropertyFormat::F8,
                b"hi",
            )
            .unwrap();
            assert_eq!(result.data, b"hi".to_vec());
        }

        #[test]
        fn append_concatenates() {
            let existing = val(31, PropertyFormat::F8, b"hello ".to_vec());
            let result = apply_change(
                Some(&existing),
                ChangeMode::Append,
                AtomId(31),
                PropertyFormat::F8,
                b"world",
            )
            .unwrap();
            assert_eq!(result.data, b"hello world".to_vec());
        }

        #[test]
        fn prepend_concatenates() {
            let existing = val(31, PropertyFormat::F8, b"world".to_vec());
            let result = apply_change(
                Some(&existing),
                ChangeMode::Prepend,
                AtomId(31),
                PropertyFormat::F8,
                b"hello ",
            )
            .unwrap();
            assert_eq!(result.data, b"hello world".to_vec());
        }

        #[test]
        fn append_type_mismatch_is_bad_match() {
            let existing = val(31, PropertyFormat::F8, b"hi".to_vec());
            let err = apply_change(
                Some(&existing),
                ChangeMode::Append,
                AtomId(4),
                PropertyFormat::F8,
                b"yo",
            )
            .unwrap_err();
            assert_eq!(err, ChangePropertyError::BadMatch);
        }

        #[test]
        fn append_format_mismatch_is_bad_match() {
            let existing = val(31, PropertyFormat::F8, b"hi".to_vec());
            let err = apply_change(
                Some(&existing),
                ChangeMode::Append,
                AtomId(31),
                PropertyFormat::F32,
                &[1, 2, 3, 4],
            )
            .unwrap_err();
            assert_eq!(err, ChangePropertyError::BadMatch);
        }

        #[test]
        fn replace_at_max_succeeds() {
            let data = vec![0u8; MAX_PROPERTY_BYTES];
            let result = apply_change(
                None,
                ChangeMode::Replace,
                AtomId(31),
                PropertyFormat::F8,
                &data,
            )
            .unwrap();
            assert_eq!(result.data.len(), MAX_PROPERTY_BYTES);
        }

        #[test]
        fn replace_above_max_is_bad_alloc() {
            let data = vec![0u8; MAX_PROPERTY_BYTES + 1];
            let err = apply_change(
                None,
                ChangeMode::Replace,
                AtomId(31),
                PropertyFormat::F8,
                &data,
            )
            .unwrap_err();
            assert_eq!(err, ChangePropertyError::BadAlloc);
        }

        #[test]
        fn append_at_cumulative_max_succeeds() {
            let existing = val(31, PropertyFormat::F8, vec![0u8; MAX_PROPERTY_BYTES - 1]);
            let result = apply_change(
                Some(&existing),
                ChangeMode::Append,
                AtomId(31),
                PropertyFormat::F8,
                &[0],
            )
            .unwrap();
            assert_eq!(result.data.len(), MAX_PROPERTY_BYTES);
        }

        #[test]
        fn append_above_cumulative_max_is_bad_alloc() {
            let existing = val(31, PropertyFormat::F8, vec![0u8; MAX_PROPERTY_BYTES]);
            let err = apply_change(
                Some(&existing),
                ChangeMode::Append,
                AtomId(31),
                PropertyFormat::F8,
                &[0],
            )
            .unwrap_err();
            assert_eq!(err, ChangePropertyError::BadAlloc);
        }

        #[test]
        fn prepend_at_cumulative_max_succeeds() {
            let existing = val(31, PropertyFormat::F8, vec![0u8; MAX_PROPERTY_BYTES - 1]);
            let result = apply_change(
                Some(&existing),
                ChangeMode::Prepend,
                AtomId(31),
                PropertyFormat::F8,
                &[0],
            )
            .unwrap();
            assert_eq!(result.data.len(), MAX_PROPERTY_BYTES);
        }
    }

    mod apply_change_props {
        use super::*;
        use proptest::prelude::*;

        fn arb_format() -> impl Strategy<Value = PropertyFormat> {
            prop_oneof![
                Just(PropertyFormat::F8),
                Just(PropertyFormat::F16),
                Just(PropertyFormat::F32)
            ]
        }

        fn arb_aligned_data(
            format: PropertyFormat,
            max_units: usize,
        ) -> impl Strategy<Value = Vec<u8>> {
            let bytes = format.bytes();
            prop::collection::vec(any::<u8>(), 0..max_units).prop_map(move |v| {
                let len = v.len() - (v.len() % bytes);
                v.into_iter().take(len).collect()
            })
        }

        proptest! {
            #[test]
            fn replace_round_trip(t in 1u32..1000, f in arb_format(), data in any::<Vec<u8>>().prop_filter("len bound", |v| v.len() <= 4096)) {
                let aligned: Vec<u8> = data.iter().take(data.len() - (data.len() % f.bytes())).copied().collect();
                let v = apply_change(None, ChangeMode::Replace, AtomId(t), f, &aligned).unwrap();
                prop_assert_eq!(v.data, aligned);
                prop_assert_eq!(v.r#type, AtomId(t));
                prop_assert_eq!(v.format, f);
            }

            #[test]
            fn append_additivity(t in 1u32..1000, f in arb_format(), a in arb_aligned_data(PropertyFormat::F8, 1024), b in arb_aligned_data(PropertyFormat::F8, 1024)) {
                let trimmed_a: Vec<u8> = a.iter().take(a.len() - (a.len() % f.bytes())).copied().collect();
                let trimmed_b: Vec<u8> = b.iter().take(b.len() - (b.len() % f.bytes())).copied().collect();
                let existing = PropertyValue { r#type: AtomId(t), format: f, data: trimmed_a.clone() };
                let result = apply_change(Some(&existing), ChangeMode::Append, AtomId(t), f, &trimmed_b).unwrap();
                prop_assert_eq!(result.data.len(), trimmed_a.len() + trimmed_b.len());
                prop_assert_eq!(&result.data[..trimmed_a.len()], &trimmed_a[..]);
                prop_assert_eq!(&result.data[trimmed_a.len()..], &trimmed_b[..]);
            }

            #[test]
            fn prepend_concat_order(t in 1u32..1000, f in arb_format(), a in arb_aligned_data(PropertyFormat::F8, 1024), b in arb_aligned_data(PropertyFormat::F8, 1024)) {
                let trimmed_a: Vec<u8> = a.iter().take(a.len() - (a.len() % f.bytes())).copied().collect();
                let trimmed_b: Vec<u8> = b.iter().take(b.len() - (b.len() % f.bytes())).copied().collect();
                let existing = PropertyValue { r#type: AtomId(t), format: f, data: trimmed_a.clone() };
                let result = apply_change(Some(&existing), ChangeMode::Prepend, AtomId(t), f, &trimmed_b).unwrap();
                prop_assert_eq!(&result.data[..trimmed_b.len()], &trimmed_b[..]);
                prop_assert_eq!(&result.data[trimmed_b.len()..], &trimmed_a[..]);
            }

            #[test]
            fn append_type_mismatch_always_bad_match(t1 in 1u32..1000, t2 in 1u32..1000, f in arb_format()) {
                prop_assume!(t1 != t2);
                let existing = PropertyValue { r#type: AtomId(t1), format: f, data: vec![] };
                let err = apply_change(Some(&existing), ChangeMode::Append, AtomId(t2), f, &[]).unwrap_err();
                prop_assert_eq!(err, ChangePropertyError::BadMatch);
            }
        }
    }

    mod slice_for_get_tests {
        use super::*;
        use x12_protocol::x11::AtomId;

        #[test]
        fn absent_property_returns_none_metadata() {
            let s = slice_for_get(None, AtomId(0), 0, 1024).unwrap();
            assert_eq!(s.r#type, AtomId(0));
            assert_eq!(s.format, 0);
            assert!(s.value.is_empty());
            assert_eq!(s.bytes_after, 0);
        }

        #[test]
        fn type_mismatch_returns_metadata_no_data() {
            let p = PropertyValue {
                r#type: AtomId(31),
                format: PropertyFormat::F8,
                data: b"hello".to_vec(),
            };
            let s = slice_for_get(Some(&p), AtomId(4), 0, 1024).unwrap();
            assert_eq!(s.r#type, AtomId(31));
            assert_eq!(s.format, 8);
            assert!(s.value.is_empty());
            assert_eq!(s.bytes_after, 5);
        }

        #[test]
        fn read_format32_long_length_one_returns_4_bytes() {
            let p = PropertyValue {
                r#type: AtomId(31),
                format: PropertyFormat::F32,
                data: vec![1, 2, 3, 4, 5, 6, 7, 8],
            };
            let s = slice_for_get(Some(&p), AtomId(31), 0, 1).unwrap();
            assert_eq!(s.value, [1, 2, 3, 4]);
            assert_eq!(s.bytes_after, 4);
        }

        #[test]
        fn read_format8_long_length_one_returns_4_bytes() {
            let p = PropertyValue {
                r#type: AtomId(31),
                format: PropertyFormat::F8,
                data: b"hello world!".to_vec(),
            };
            let s = slice_for_get(Some(&p), AtomId(31), 0, 1).unwrap();
            assert_eq!(s.value, b"hell");
        }

        #[test]
        fn offset_past_end_is_bad_value() {
            let p = PropertyValue {
                r#type: AtomId(31),
                format: PropertyFormat::F8,
                data: b"hi".to_vec(),
            };
            let err = slice_for_get(Some(&p), AtomId(31), 1, 1).unwrap_err();
            assert_eq!(err, ChangePropertyError::BadValue);
        }

        #[test]
        fn offset_at_end_is_valid_empty_slice() {
            let p = PropertyValue {
                r#type: AtomId(31),
                format: PropertyFormat::F8,
                data: b"abcd".to_vec(),
            };
            let s = slice_for_get(Some(&p), AtomId(31), 1, 0).unwrap();
            assert!(s.value.is_empty());
            assert_eq!(s.bytes_after, 0);
        }
    }

    mod slice_for_get_props {
        use super::*;
        use proptest::prelude::*;

        fn arb_property() -> impl Strategy<Value = PropertyValue> {
            let format_strat = prop_oneof![
                Just(PropertyFormat::F8),
                Just(PropertyFormat::F16),
                Just(PropertyFormat::F32)
            ];
            (1u32..1000, format_strat, 0usize..512).prop_flat_map(|(t, f, len_units)| {
                let bytes = f.bytes();
                let total = len_units * bytes;
                prop::collection::vec(any::<u8>(), total..=total).prop_map(move |data| {
                    PropertyValue {
                        r#type: AtomId(t),
                        format: f,
                        data,
                    }
                })
            })
        }

        proptest! {
            #[test]
            fn read_all_recovers_data(p in arb_property()) {
                let s = slice_for_get(Some(&p), p.r#type, 0, u32::MAX / 4).unwrap();
                prop_assert_eq!(s.value, &p.data[..]);
                prop_assert_eq!(s.bytes_after, 0);
            }

            #[test]
            fn value_len_in_format_units(p in arb_property(), off_units in 0u32..32) {
                let off_bytes = u64::from(off_units) * 4;
                prop_assume!(off_bytes <= p.data.len() as u64);
                let s = slice_for_get(Some(&p), p.r#type, off_units, 8).unwrap();
                let unit = p.format.bytes();
                prop_assert_eq!(s.value.len() % unit, 0);
            }

            #[test]
            fn bytes_after_invariant(p in arb_property(), off_units in 0u32..32, len_units in 0u32..32) {
                let off_bytes = u64::from(off_units) * 4;
                prop_assume!(off_bytes <= p.data.len() as u64);
                let s = slice_for_get(Some(&p), p.r#type, off_units, len_units).unwrap();
                let remaining = (p.data.len() as u64) - off_bytes;
                prop_assert_eq!(s.value.len() as u64 + s.bytes_after as u64, remaining);
            }

            #[test]
            fn any_type_matches(p in arb_property()) {
                let s = slice_for_get(Some(&p), AtomId(0), 0, u32::MAX / 4).unwrap();
                prop_assert_eq!(s.r#type, p.r#type);
                prop_assert_eq!(s.value, &p.data[..]);
            }

            #[test]
            fn type_mismatch_metadata(p in arb_property(), other_t in 1u32..1000) {
                prop_assume!(other_t != p.r#type.0);
                let s = slice_for_get(Some(&p), AtomId(other_t), 0, u32::MAX / 4).unwrap();
                prop_assert!(s.value.is_empty());
                prop_assert_eq!(s.bytes_after as usize, p.data.len());
                prop_assert_eq!(s.r#type, p.r#type);
            }

            #[test]
            fn offset_past_end_is_bad_value(p in arb_property()) {
                prop_assume!(!p.data.is_empty());
                let off_units = ((p.data.len() / 4) as u32) + 1;
                let err = slice_for_get(Some(&p), p.r#type, off_units, 1).unwrap_err();
                prop_assert_eq!(err, ChangePropertyError::BadValue);
            }
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(1000))]
            #[test]
            fn chunked_reassembles(p in arb_property()) {
                let mut acc: Vec<u8> = Vec::new();
                let mut offset = 0u32;
                loop {
                    let s = slice_for_get(Some(&p), p.r#type, offset, 8).unwrap();
                    acc.extend_from_slice(s.value);
                    if s.bytes_after == 0 { break; }
                    offset += (s.value.len() / 4) as u32;
                }
                prop_assert_eq!(acc, p.data.clone());
            }
        }
    }
}
