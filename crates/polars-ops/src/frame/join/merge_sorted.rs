use arrow::legacy::utils::{CustomIterTools, FromTrustedLenIterator};
use polars_core::prelude::*;
use polars_core::with_match_physical_numeric_polars_type;

fn check_and_union_revmaps(
    lhs_revmap: &Option<Arc<RevMapping>>,
    rhs_revmap: &Option<Arc<RevMapping>>,
) -> PolarsResult<Option<Arc<RevMapping>>> {
    // Ensure we are operating on either identical locals, or compatible globals.
    let lhs_revmap = lhs_revmap.as_ref().unwrap();
    let rhs_revmap = rhs_revmap.as_ref().unwrap();
    match (&**lhs_revmap, &**rhs_revmap) {
        (RevMapping::Local(_, l_hash), RevMapping::Local(_, r_hash)) => {
            // Same local categoricals, we return immediately
            polars_ensure!(l_hash == r_hash, ComputeError: "cannot merge-sort incompatible categoricals");
            Ok(None)
        },
        // Return revmap that is the union of the two revmaps.
        (RevMapping::Global(_, _, l), RevMapping::Global(_, _, r)) => {
            polars_ensure!(l == r, ComputeError: "cannot merge-sort incompatible categoricals");
            let mut rev_map_merger = GlobalRevMapMerger::new(lhs_revmap.clone());
            rev_map_merger.merge_map(rhs_revmap)?;
            let new_map = rev_map_merger.finish();
            Ok(Some(new_map))
        },
        _ => unreachable!(),
    }
}

pub fn _merge_sorted_dfs(
    left: &DataFrame,
    right: &DataFrame,
    left_s: &Series,
    right_s: &Series,
    check_schema: bool,
) -> PolarsResult<DataFrame> {
    if check_schema {
        left.schema_equal(right)?;
    }
    let dtype_lhs = left_s.dtype();
    let dtype_rhs = right_s.dtype();

    polars_ensure!(
        dtype_lhs == dtype_rhs,
        ComputeError: "merge-sort datatype mismatch: {} != {}", dtype_lhs, dtype_rhs
    );

    if dtype_lhs.is_categorical() {
        let rev_map_lhs = left_s.categorical().unwrap().get_rev_map();
        let rev_map_rhs = right_s.categorical().unwrap().get_rev_map();
        polars_ensure!(
            rev_map_lhs.same_src(rev_map_rhs),
            ComputeError: "can only merge-sort categoricals with the same categories"
        );
    }

    // If one frame is empty, we can return the other immediately.
    if right_s.is_empty() {
        return Ok(left.clone());
    } else if left_s.is_empty() {
        return Ok(right.clone());
    }

    let merge_indicator = series_to_merge_indicator(left_s, right_s)?;
    let new_columns = left
        .get_columns()
        .iter()
        .zip(right.get_columns())
        .map(|(lhs, rhs)| {
            let lhs_phys = lhs.to_physical_repr();
            let rhs_phys = rhs.to_physical_repr();

            let out = Column::from(merge_series(
                lhs_phys.as_materialized_series(),
                rhs_phys.as_materialized_series(),
                &merge_indicator,
            )?);

            let lhs_dt = lhs.dtype();
            let dtype_out = match (lhs_dt, rhs.dtype()) {
                // Global categorical revmaps must be merged for the output.
                (DataType::Categorical(lhs_revmap, ord), DataType::Categorical(rhs_revmap, _)) => {
                    if let Some(new_revmap) = check_and_union_revmaps(lhs_revmap, rhs_revmap)? {
                        &DataType::Categorical(Some(new_revmap), *ord)
                    } else {
                        lhs_dt
                    }
                },
                _ => lhs_dt,
            };

            let mut out = unsafe { out.from_physical_unchecked(dtype_out) }.unwrap();
            out.rename(lhs.name().clone());
            Ok(out)
        })
        .collect::<PolarsResult<_>>()?;

    Ok(unsafe { DataFrame::new_no_checks(left.height() + right.height(), new_columns) })
}

fn merge_series(lhs: &Series, rhs: &Series, merge_indicator: &[bool]) -> PolarsResult<Series> {
    use DataType::*;
    let out = match lhs.dtype() {
        Boolean => {
            let lhs = lhs.bool().unwrap();
            let rhs = rhs.bool().unwrap();

            merge_ca(lhs, rhs, merge_indicator).into_series()
        },
        String => {
            // dispatch via binary
            let lhs = lhs.str().unwrap().as_binary();
            let rhs = rhs.str().unwrap().as_binary();
            let out = merge_ca(&lhs, &rhs, merge_indicator);
            unsafe { out.to_string_unchecked() }.into_series()
        },
        Binary => {
            let lhs = lhs.binary().unwrap();
            let rhs = rhs.binary().unwrap();
            merge_ca(lhs, rhs, merge_indicator).into_series()
        },
        #[cfg(feature = "dtype-struct")]
        Struct(_) => {
            let lhs = lhs.struct_().unwrap();
            let rhs = rhs.struct_().unwrap();
            polars_ensure!(lhs.null_count() + rhs.null_count() == 0, InvalidOperation: "merge sorted with structs with outer nulls not yet supported");

            let new_fields = lhs
                .fields_as_series()
                .iter()
                .zip(rhs.fields_as_series())
                .map(|(lhs, rhs)| {
                    merge_series(lhs, &rhs, merge_indicator)
                        .map(|merged| merged.with_name(lhs.name().clone()))
                })
                .collect::<PolarsResult<Vec<_>>>()?;
            StructChunked::from_series(PlSmallStr::EMPTY, new_fields[0].len(), new_fields.iter())
                .unwrap()
                .into_series()
        },
        List(_) => {
            let lhs = lhs.list().unwrap();
            let rhs = rhs.list().unwrap();
            merge_ca(lhs, rhs, merge_indicator).into_series()
        },
        dt => {
            with_match_physical_numeric_polars_type!(dt, |$T| {
                    let lhs: &ChunkedArray<$T> = lhs.as_ref().as_ref().as_ref();
                    let rhs: &ChunkedArray<$T> = rhs.as_ref().as_ref().as_ref();
                    merge_ca(lhs, rhs, merge_indicator).into_series()
            })
        },
    };
    Ok(out)
}

fn merge_ca<'a, T>(
    a: &'a ChunkedArray<T>,
    b: &'a ChunkedArray<T>,
    merge_indicator: &[bool],
) -> ChunkedArray<T>
where
    T: PolarsDataType + 'static,
    &'a ChunkedArray<T>: IntoIterator,
    ChunkedArray<T>:
        FromTrustedLenIterator<<<&'a ChunkedArray<T> as IntoIterator>::IntoIter as Iterator>::Item>,
{
    let total_len = a.len() + b.len();
    let mut a = a.into_iter();
    let mut b = b.into_iter();

    let iter = merge_indicator.iter().map(|a_indicator| {
        if *a_indicator {
            a.next().unwrap()
        } else {
            b.next().unwrap()
        }
    });

    // SAFETY: length is correct
    unsafe { iter.trust_my_length(total_len).collect_trusted() }
}

fn series_to_merge_indicator(lhs: &Series, rhs: &Series) -> PolarsResult<Vec<bool>> {
    if lhs.dtype().is_categorical() {
        let lhs_ca = lhs.categorical().unwrap();
        if lhs_ca.uses_lexical_ordering() {
            let rhs_ca = rhs.categorical().unwrap();
            let out = get_merge_indicator(lhs_ca.iter_str(), rhs_ca.iter_str());
            return Ok(out);
        }
    }

    let lhs_s = lhs.to_physical_repr().into_owned();
    let rhs_s = rhs.to_physical_repr().into_owned();

    let out = match lhs_s.dtype() {
        DataType::Boolean => {
            let lhs = lhs_s.bool().unwrap();
            let rhs = rhs_s.bool().unwrap();
            get_merge_indicator(lhs.into_iter(), rhs.into_iter())
        },
        DataType::String => {
            let lhs = lhs.str().unwrap().as_binary();
            let rhs = rhs.str().unwrap().as_binary();
            get_merge_indicator(lhs.into_iter(), rhs.into_iter())
        },
        DataType::Binary => {
            let lhs = lhs_s.binary().unwrap();
            let rhs = rhs_s.binary().unwrap();
            get_merge_indicator(lhs.into_iter(), rhs.into_iter())
        },
        #[cfg(feature = "dtype-struct")]
        DataType::Struct(_) => {
            let options = SortOptions::default();
            let lhs = lhs_s.struct_().unwrap().get_row_encoded(options)?;
            let rhs = rhs_s.struct_().unwrap().get_row_encoded(options)?;
            get_merge_indicator(lhs.into_iter(), rhs.into_iter())
        },
        _ => {
            with_match_physical_numeric_polars_type!(lhs_s.dtype(), |$T| {
                    let lhs: &ChunkedArray<$T> = lhs_s.as_ref().as_ref().as_ref();
                    let rhs: &ChunkedArray<$T> = rhs_s.as_ref().as_ref().as_ref();

                    get_merge_indicator(lhs.into_iter(), rhs.into_iter())

            })
        },
    };
    Ok(out)
}

// get a boolean values, left: true, right: false
// that indicate from which side we should take a value
fn get_merge_indicator<T>(
    mut a_iter: impl ExactSizeIterator<Item = T>,
    mut b_iter: impl ExactSizeIterator<Item = T>,
) -> Vec<bool>
where
    T: PartialOrd + Default + Copy,
{
    const A_INDICATOR: bool = true;
    const B_INDICATOR: bool = false;

    let a_len = a_iter.size_hint().0;
    let b_len = b_iter.size_hint().0;
    if a_len == 0 {
        return vec![true; b_len];
    };
    if b_len == 0 {
        return vec![false; a_len];
    }

    let mut current_a = T::default();
    let cap = a_len + b_len;
    let mut out = Vec::with_capacity(cap);

    let mut current_b = b_iter.next().unwrap();

    for a in &mut a_iter {
        current_a = a;
        if a <= current_b {
            out.push(A_INDICATOR);
            continue;
        }
        out.push(B_INDICATOR);

        loop {
            if let Some(b) = b_iter.next() {
                current_b = b;
                if b >= a {
                    out.push(A_INDICATOR);
                    break;
                }
                out.push(B_INDICATOR);
                continue;
            }
            // b is depleted fill with a indicator
            let remaining = cap - out.len();
            out.extend(std::iter::repeat_n(A_INDICATOR, remaining));
            return out;
        }
    }
    if current_a < current_b {
        out.push(B_INDICATOR);
    }
    // check if current value already is added
    if *out.last().unwrap() == A_INDICATOR {
        out.push(B_INDICATOR);
    }
    // take remaining
    out.extend(b_iter.map(|_| B_INDICATOR));
    assert_eq!(out.len(), b_len + a_len);

    out
}

#[test]
fn test_merge_sorted() {
    fn get_merge_indicator_sliced<T: PartialOrd + Default + Copy>(a: &[T], b: &[T]) -> Vec<bool> {
        get_merge_indicator(a.iter().copied(), b.iter().copied())
    }

    let a = [1, 2, 4, 6, 9];
    let b = [2, 3, 4, 5, 10];

    let out = get_merge_indicator_sliced(&a, &b);
    let expected = [
        true, true, false, false, true, false, false, true, true, false,
    ];
    //                       1     2     2      3      4     4      5      6     9     10
    assert_eq!(out, expected);

    // swap
    // it is not the inverse because left is preferred when both are equal
    let out = get_merge_indicator_sliced(&b, &a);
    let expected = [
        false, true, false, true, true, false, true, false, false, true,
    ];
    assert_eq!(out, expected);

    let a = [5, 6, 7, 10];
    let b = [1, 2, 5];
    let out = get_merge_indicator_sliced(&a, &b);
    let expected = [false, false, true, false, true, true, true];
    assert_eq!(out, expected);

    // swap
    // it is not the inverse because left is preferred when both are equal
    let out = get_merge_indicator_sliced(&b, &a);
    let expected = [true, true, true, false, false, false, false];
    assert_eq!(out, expected);
}
