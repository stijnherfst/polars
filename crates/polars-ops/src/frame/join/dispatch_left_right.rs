use super::*;
use crate::prelude::*;

pub(super) fn left_join_from_series(
    left: DataFrame,
    right: &DataFrame,
    s_left: &Series,
    s_right: &Series,
    args: JoinArgs,
    verbose: bool,
    drop_names: Option<Vec<PlSmallStr>>,
) -> PolarsResult<DataFrame> {
    let (df_left, df_right) = materialize_left_join_from_series(
        left, right, s_left, s_right, &args, verbose, drop_names,
    )?;
    _finish_join(df_left, df_right, args.suffix)
}

pub(super) fn right_join_from_series(
    left: &DataFrame,
    right: DataFrame,
    s_left: &Series,
    s_right: &Series,
    args: JoinArgs,
    verbose: bool,
    drop_names: Option<Vec<PlSmallStr>>,
) -> PolarsResult<DataFrame> {
    // Swap the order of tables to do a right join.
    let (df_right, df_left) = materialize_left_join_from_series(
        right, left, s_right, s_left, &args, verbose, drop_names,
    )?;
    _finish_join(df_left, df_right, args.suffix)
}

pub fn materialize_left_join_from_series(
    mut left: DataFrame,
    right_: &DataFrame,
    s_left: &Series,
    s_right: &Series,
    args: &JoinArgs,
    verbose: bool,
    drop_names: Option<Vec<PlSmallStr>>,
) -> PolarsResult<(DataFrame, DataFrame)> {
    #[cfg(feature = "dtype-categorical")]
    _check_categorical_src(s_left.dtype(), s_right.dtype())?;

    let mut s_left = s_left.clone();
    // Eagerly limit left if possible.
    if let Some((offset, len)) = args.slice {
        if offset == 0 {
            left = left.slice(0, len);
            s_left = s_left.slice(0, len);
        }
    }

    // Ensure that the chunks are aligned otherwise we go OOB.
    let mut right = Cow::Borrowed(right_);
    let mut s_right = s_right.clone();
    if left.should_rechunk() {
        left.as_single_chunk_par();
        s_left = s_left.rechunk();
    }
    if right.should_rechunk() {
        let mut other = right_.clone();
        other.as_single_chunk_par();
        right = Cow::Owned(other);
        s_right = s_right.rechunk();
    }

    let (left_idx, right_idx) =
        sort_or_hash_left(&s_left, &s_right, verbose, args.validation, args.join_nulls)?;

    let right = if let Some(drop_names) = drop_names {
        right.drop_many(drop_names)
    } else {
        right.drop(s_right.name()).unwrap()
    };

    // The current sort_or_hash_left implementation preserves the Left DataFrame order so skip left for now.
    let requires_ordering = matches!(
        args.maintain_order,
        MaintainOrderJoin::Right | MaintainOrderJoin::RightLeft
    );

    #[cfg(feature = "chunked_ids")]
    match (left_idx, right_idx) {
        (ChunkJoinIds::Left(left_idx), ChunkJoinOptIds::Left(right_idx)) => {
            if requires_ordering {
                Ok(maintain_order_idx(
                    &left,
                    &right,
                    left_idx.as_slice(),
                    right_idx.as_slice(),
                    args,
                ))
            } else {
                Ok(materialize_left_join_idx(
                    &left,
                    &right,
                    left_idx.as_slice(),
                    right_idx.as_slice(),
                    args,
                ))
            }
        },
        (ChunkJoinIds::Right(left_idx), ChunkJoinOptIds::Right(right_idx)) => {
            if requires_ordering {
                Ok(maintain_order_chunkid(
                    &left,
                    &right,
                    left_idx.as_slice(),
                    right_idx.as_slice(),
                    args,
                ))
            } else {
                Ok(materialize_left_join_chunked(
                    &left,
                    &right,
                    left_idx.as_slice(),
                    right_idx.as_slice(),
                    args,
                ))
            }
        },
        (_, _) => unreachable!(),
    }

    #[cfg(not(feature = "chunked_ids"))]
    if requires_ordering {
        Ok(maintain_order_idx(
            &left,
            &right,
            left_idx.as_slice(),
            right_idx.as_slice(),
            args,
        ))
    } else {
        Ok(materialize_left_join_idx(
            &left,
            &right,
            left_idx.as_slice(),
            right_idx.as_slice(),
            args,
        ))
    }
}

fn maintain_order_idx(
    left: &DataFrame,
    other: &DataFrame,
    left_idx: &[IdxSize],
    right_idx: &[NullableIdxSize],
    args: &JoinArgs,
) -> (DataFrame, DataFrame) {
    let mut df = {
        let left = unsafe { IdxCa::mmap_slice("a".into(), left_idx) };
        let reference: &[IdxSize] = unsafe { std::mem::transmute(right_idx) };
        let right = unsafe { IdxCa::mmap_slice("b".into(), reference) };
        DataFrame::new(vec![left.into_series().into(), right.into_series().into()]).unwrap()
    };

    let options = SortMultipleOptions::new()
        .with_order_descending(false)
        .with_maintain_order(true);

    let columns = match args.maintain_order {
        // If the left order is preserved then there are no unsorted right rows
        // So Left and LeftRight are equal
        MaintainOrderJoin::Left | MaintainOrderJoin::LeftRight => vec!["a"],
        MaintainOrderJoin::Right => vec!["b"],
        MaintainOrderJoin::RightLeft => vec!["b", "a"],
        _ => unreachable!(),
    };

    df.sort_in_place(columns, options).unwrap();
    df.rechunk_mut();

    // SAFETY: We just made the slice continuous by rechunking
    let join_tuples_left = df
        .column("a")
        .unwrap()
        .as_series()
        .unwrap()
        .idx()
        .unwrap()
        .cont_slice()
        .unwrap();

    // SAFETY: We just made the slice continuous by rechunking
    let join_tuples_right = df
        .column("b")
        .unwrap()
        .as_series()
        .unwrap()
        .idx()
        .unwrap()
        .cont_slice()
        .unwrap();

    let join_tuples_right: &[NullableIdxSize] = unsafe { std::mem::transmute(join_tuples_right) };

    materialize_left_join_idx(left, other, join_tuples_left, join_tuples_right, args)
}

#[cfg(feature = "chunked_ids")]
fn maintain_order_chunkid(
    left: &DataFrame,
    other: &DataFrame,
    left_idx: &[ChunkId],
    right_idx: &[ChunkId],
    args: &JoinArgs,
) -> (DataFrame, DataFrame) {
    (DataFrame::empty(), DataFrame::empty())
}

fn materialize_left_join_idx(
    left: &DataFrame,
    other: &DataFrame,
    left_idx: &[IdxSize],
    right_idx: &[NullableIdxSize],
    args: &JoinArgs,
) -> (DataFrame, DataFrame) {
    let materialize_left = || unsafe {
        let left_idx = if let Some((offset, len)) = args.slice {
            slice_slice(left_idx, offset, len)
        } else {
            left_idx
        };
        left._create_left_df_from_slice(
            left_idx,
            true,
            args.slice.is_some(),
            matches!(
                args.maintain_order,
                MaintainOrderJoin::Left | MaintainOrderJoin::LeftRight
            ),
        )
    };

    let materialize_right = || unsafe {
        let right_idx = if let Some((offset, len)) = args.slice {
            slice_slice(right_idx, offset, len)
        } else {
            right_idx
        };
        IdxCa::with_nullable_idx(right_idx, |idx| other.take_unchecked(idx))
    };
    POOL.join(materialize_left, materialize_right)
}

#[cfg(feature = "chunked_ids")]
fn materialize_left_join_chunked(
    left: &DataFrame,
    other: &DataFrame,
    left_idx: &[ChunkId],
    right_idx: &[ChunkId],
    args: &JoinArgs,
) -> (DataFrame, DataFrame) {
    let materialize_left = || unsafe {
        let left_idx = if let Some((offset, len)) = args.slice {
            slice_slice(left_idx, offset, len)
        } else {
            left_idx
        };
        left.create_left_df_chunked(left_idx, true, args.slice.is_some())
    };

    let materialize_right = || unsafe {
        let right_idx = if let Some((offset, len)) = args.slice {
            slice_slice(right_idx, offset, len)
        } else {
            right_idx
        };
        other._take_opt_chunked_unchecked_hor_par(right_idx)
    };
    POOL.join(materialize_left, materialize_right)
}
