//! Order statistics shared by the group verdicts.

/// The median of `v`: the middle value, or the mean of the middle two for an
/// even count. `None` for an empty list. NaN sorts last.
pub(crate) fn median(mut v: Vec<f32>) -> Option<f32> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(f32::total_cmp);
    let mid = v.len() / 2;
    Some(if v.len() % 2 == 1 {
        v[mid]
    } else {
        f32::midpoint(v[mid - 1], v[mid])
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_of_odd_even_and_empty() {
        assert_eq!(median(vec![]), None);
        assert_eq!(median(vec![4.0]), Some(4.0));
        assert_eq!(median(vec![3.0, 1.0, 2.0]), Some(2.0));
        // Even: the mean of the middle two, not the upper one.
        assert_eq!(median(vec![1.0, 3.0]), Some(2.0));
        assert_eq!(median(vec![4.0, 1.0, 3.0, 2.0]), Some(2.5));
        assert_eq!(median(vec![-1.0, -3.0]), Some(-2.0));
        // NaN sorts last instead of panicking the sort.
        assert_eq!(median(vec![f32::NAN, 1.0, 2.0]), Some(2.0));
    }
}
