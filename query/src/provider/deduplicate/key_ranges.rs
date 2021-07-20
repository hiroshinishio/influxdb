//! Implement iterator and comparator to split data into distinct ranges

use arrow::array::{build_compare, ArrayData, DynComparator};
use arrow::compute::{SortColumn, SortOptions};
use arrow::error::{ArrowError, Result as ArrowResult};

// use snafu::Snafu;
use std::cmp::Ordering;
use std::iter::Iterator;
use std::ops::Range;

// #[derive(Debug, Snafu)]
// pub enum Error {
//     #[snafu(display(
//         "Sort requires at least one column"
//     ))]
//     EmptyColumns {},

//     #[snafu(display(
//         "Sort columns have different row counts"
//     ))]
//     DifferentRowCounts{},
// }

// pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Given a list of key columns, find partition ranges that would partition
/// equal values across columns
///
/// The returned vec would be of size k where k is cardinality of the values; Consecutive
/// values will be connected: (a, b) and (b, c), where start = 0 and end = n for the first and last
/// range.
///
/// The algorithms here work with any set of columns but it is implemented to optimize the input columns
/// Example Input columns:
/// Invisible Index |  Highest_Cardinality | Time | Second_Highest_Cardinality | Lowest_Cardinality
/// --------------- | -------------------- | ---- | -------------------------- | --------------------
///         0       |          1           |  1   |             1              |            1
///         1       |          1           |  10  |             1              |            1
///         2       |          3           |  8   |             1              |            1
///         3       |          4           |  9   |             1              |            1
///         4       |          4           |  9   |             1              |            1
///         5       |          5           |  1   |             1              |            1
///         6       |          5           |  15  |             1              |            1
///         7       |          5           |  15  |             2              |            1
///         8       |          5           |  15  |             2              |            1
///         9       |          5           |  15  |             2              |            2
///  The columns are sorted (and RLE) on this sort order:
///    (Lowest_Cardinality, Second_Highest_Cardinality, Highest_Cardinality, Time)
/// Out put ranges: 8 ranges on their invisible indices
///   [0, 1],
///   [1, 2],
///   [2, 3],
///   [3, 5],  -- 2 rows with same values (4, 9, 1, 1)
///   [5, 6],
///   [6, 7],
///   [7, 9],  -- 2 rows with same values (5, 15, 2, 1)
///   [9, 10],

pub fn key_ranges(columns: &[SortColumn]) -> ArrowResult<impl Iterator<Item = Range<usize>> + '_> {
    KeyRangeIterator::try_new(columns)
}

struct KeyRangeIterator<'a> {
    // function to compare values of columns
    // Todo: this is the same as LexicographicalComparator.
    // Either use it or make it like https://github.com/apache/arrow-rs/issues/563
    comparator: KeyRangeComparator<'a>,
    // Number of rows of the columns
    num_rows: usize,
    // end index of previous range which will be used as starting index of the next computing range
    start_range_idx: usize,
    //
    // current_range_idx: usize,
    //value_indices: Vec<usize>,
}

impl<'a> KeyRangeIterator<'a> {
    fn try_new(columns: &'a [SortColumn]) -> ArrowResult<KeyRangeIterator<'a>> {
        if columns.is_empty() {
            return Err(ArrowError::InvalidArgumentError(
                "Key range requires at least one column".to_string(),
            ));
        }
        let num_rows = columns[0].values.len();
        if columns.iter().any(|item| item.values.len() != num_rows) {
            return Err(ArrowError::ComputeError(
                "Sort columns have different row counts".to_string(),
            ));
        };

        let comparator = KeyRangeComparator::try_new(columns)?;
        Ok(KeyRangeIterator {
            comparator,
            num_rows,
            start_range_idx: 0,
        })
    }
}

impl<'a> Iterator for KeyRangeIterator<'a> {
    type Item = Range<usize>;

    fn next(&mut self) -> Option<Self::Item> {
        // End of the row
        if self.start_range_idx >= self.num_rows {
            return None;
        }

        let mut idx = self.start_range_idx + 1;
        while idx < self.num_rows {
            if self.comparator.compare(&self.start_range_idx, &idx) == Ordering::Equal {
                idx += 1;
            } else {
                break;
            }
        }
        let start = self.start_range_idx;
        self.start_range_idx = idx;
        Some(Range { start, end: idx })
    }
}

type KeyRangeCompareItem<'a> = (
    &'a ArrayData, // data
    DynComparator, // comparator
    SortOptions,   // sort_option
);

/// A comparator that wraps given array data (columns) and can compare data
/// at given two indices. The lifetime is the same at the data wrapped.
pub(super) struct KeyRangeComparator<'a> {
    compare_items: Vec<KeyRangeCompareItem<'a>>,
}

impl KeyRangeComparator<'_> {
    /// compare values at the wrapped columns with given indices.
    pub(super) fn compare<'a, 'b>(&'a self, a_idx: &'b usize, b_idx: &'b usize) -> Ordering {
        for (data, comparator, sort_option) in &self.compare_items {
            match (data.is_valid(*a_idx), data.is_valid(*b_idx)) {
                (true, true) => {
                    match (comparator)(*a_idx, *b_idx) {
                        // equal, move on to next column
                        Ordering::Equal => continue,
                        order => {
                            if sort_option.descending {
                                return order.reverse();
                            } else {
                                return order;
                            }
                        }
                    }
                }
                (false, true) => {
                    return if sort_option.nulls_first {
                        Ordering::Less
                    } else {
                        Ordering::Greater
                    };
                }
                (true, false) => {
                    return if sort_option.nulls_first {
                        Ordering::Greater
                    } else {
                        Ordering::Less
                    };
                }
                // equal, move on to next column
                (false, false) => continue,
            }
        }

        Ordering::Equal
    }

    /// Create a new comparator that will wrap the given columns and give comparison
    /// results with two indices.
    pub(super) fn try_new(columns: &[SortColumn]) -> ArrowResult<KeyRangeComparator<'_>> {
        let compare_items = columns
            .iter()
            .map(|column| {
                // flatten and convert build comparators
                // use ArrayData for is_valid checks later to avoid dynamic call
                let values = column.values.as_ref();
                let data = values.data_ref();
                Ok((
                    data,
                    build_compare(values, values)?,
                    column.options.unwrap_or_default(),
                ))
            })
            .collect::<ArrowResult<Vec<_>>>()?;
        Ok(KeyRangeComparator { compare_items })
    }
}

#[cfg(test)]
pub fn range(start: usize, end: usize) -> Range<usize> {
    Range { start, end }
}

#[cfg(test)]
mod test {
    use std::sync::Arc;

    use arrow::array::ArrayRef;
    use arrow::array::{Int64Array, TimestampNanosecondArray};

    use super::*;

    #[tokio::test]
    async fn test_key_ranges() {
        // Input columns:
        // Invisible Index |  Highest_Cardinality | Time | Second_Highest_Cardinality | Lowest_Cardinality
        // (not a real col)
        // --------------- | -------------------- | ---- | -------------------------- | --------------------
        //         0       |          1           |  1   |             1              |            1
        //         1       |          1           |  10  |             1              |            1
        //         2       |          3           |  8   |             1              |            1
        //         3       |          4           |  9   |             1              |            1
        //         4       |          4           |  9   |             1              |            1
        //         5       |          5           |  1   |             1              |            1
        //         6       |          5           |  15  |             1              |            1
        //         7       |          5           |  15  |             2              |            1
        //         8       |          5           |  15  |             2              |            1
        //         9       |          5           |  15  |             2              |            2
        //  The columns are sorted on this sort order:
        //    (Lowest_Cardinality, Second_Highest_Cardinality, Highest_Cardinality, Time)
        //  But when the key_ranges function is invoked, the input sort order will be
        //    (Highest_Cardinality, Time, Second_Highest_Cardinality, Lowest_Cardinality)
        // Out put ranges: 8 ranges on their invisible indices
        //   [0, 1],
        //   [1, 2],
        //   [2, 3],
        //   [3, 5],  -- 2 rows with same values (4, 9, 1, 1)
        //   [5, 6],
        //   [6, 7],
        //   [7, 9],  -- 2 rows with same values (5, 15, 2, 1)
        //   [9, 10],

        let mut lowest_cardinality = vec![Some(1); 9]; // 9 first values are all Some(1)
        lowest_cardinality.push(Some(2)); // Add Some(2)

        let mut second_highest_cardinality = vec![Some(1); 7];
        second_highest_cardinality.append(&mut vec![Some(2); 3]);

        let mut time = vec![Some(1), Some(10), Some(8), Some(9), Some(9), Some(1)];
        time.append(&mut vec![Some(15); 4]);

        let mut highest_cardinality = vec![Some(1), Some(1), Some(3), Some(4), Some(4)];
        highest_cardinality.append(&mut vec![Some(5); 5]);

        let input = vec![
            SortColumn {
                values: Arc::new(Int64Array::from(highest_cardinality)) as ArrayRef,
                options: None,
            },
            SortColumn {
                values: Arc::new(TimestampNanosecondArray::from(time)) as ArrayRef,
                options: None,
            },
            SortColumn {
                values: Arc::new(Int64Array::from(second_highest_cardinality)) as ArrayRef,
                options: None,
            },
            SortColumn {
                values: Arc::new(Int64Array::from(lowest_cardinality)) as ArrayRef,
                options: None,
            },
        ];

        let key_ranges = key_ranges(&input).unwrap();

        let expected_key_range = vec![
            range(0, 1),
            range(1, 2),
            range(2, 3),
            range(3, 5),
            range(5, 6),
            range(6, 7),
            range(7, 9),
            range(9, 10),
        ];

        assert_eq!(key_ranges.collect::<Vec<_>>(), expected_key_range);
    }
}
