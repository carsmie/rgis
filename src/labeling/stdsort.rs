//! libstdc++'s std::sort (introsort + final insertion sort), so candidates with equal costs
//! end up in the same order as in QGIS (PAL sorts with an unstable std::sort on cost alone).

const THRESHOLD: usize = 16;

/// std::sort(v.begin(), v.end(), less).
pub fn std_sort<T>(v: &mut [T], less: &impl Fn(&T, &T) -> bool) {
    let n = v.len();
    if n > 1 {
        introsort_loop(v, 0, n, 2 * n.ilog2() as usize, less);
        final_insertion_sort(v, 0, n, less);
    }
}

fn introsort_loop<T>(v: &mut [T], first: usize, mut last: usize, mut depth: usize, less: &impl Fn(&T, &T) -> bool) {
    while last - first > THRESHOLD {
        if depth == 0 {
            heap_sort(&mut v[first..last], less);
            return;
        }
        depth -= 1;
        let cut = partition_pivot(v, first, last, less);
        introsort_loop(v, cut, last, depth, less);
        last = cut;
    }
}

fn partition_pivot<T>(v: &mut [T], first: usize, last: usize, less: &impl Fn(&T, &T) -> bool) -> usize {
    let mid = first + (last - first) / 2;
    move_median_to_first(v, first, first + 1, mid, last - 1, less);
    unguarded_partition(v, first + 1, last, first, less)
}

fn move_median_to_first<T>(v: &mut [T], result: usize, a: usize, b: usize, c: usize, less: &impl Fn(&T, &T) -> bool) {
    let pick = if less(&v[a], &v[b]) {
        if less(&v[b], &v[c]) {
            b
        } else if less(&v[a], &v[c]) {
            c
        } else {
            a
        }
    } else if less(&v[a], &v[c]) {
        a
    } else if less(&v[b], &v[c]) {
        c
    } else {
        b
    };
    v.swap(result, pick);
}

fn unguarded_partition<T>(v: &mut [T], mut first: usize, mut last: usize, pivot: usize, less: &impl Fn(&T, &T) -> bool) -> usize {
    loop {
        while less(&v[first], &v[pivot]) {
            first += 1;
        }
        last -= 1;
        while less(&v[pivot], &v[last]) {
            last -= 1;
        }
        if first >= last {
            return first;
        }
        v.swap(first, last);
        first += 1;
    }
}

fn final_insertion_sort<T>(v: &mut [T], first: usize, last: usize, less: &impl Fn(&T, &T) -> bool) {
    if last - first > THRESHOLD {
        insertion_sort(v, first, first + THRESHOLD, less);
        for i in first + THRESHOLD..last {
            unguarded_linear_insert(v, i, less);
        }
    } else {
        insertion_sort(v, first, last, less);
    }
}

fn insertion_sort<T>(v: &mut [T], first: usize, last: usize, less: &impl Fn(&T, &T) -> bool) {
    for i in first + 1..last {
        if less(&v[i], &v[first]) {
            v[first..=i].rotate_right(1);
        } else {
            unguarded_linear_insert(v, i, less);
        }
    }
}

fn unguarded_linear_insert<T>(v: &mut [T], mut last: usize, less: &impl Fn(&T, &T) -> bool) {
    while less(&v[last], &v[last - 1]) {
        v.swap(last, last - 1);
        last -= 1;
    }
}

/// std::__partial_sort(first, last, last): make_heap + sort_heap.
fn heap_sort<T>(v: &mut [T], less: &impl Fn(&T, &T) -> bool) {
    let len = v.len();
    if len < 2 {
        return;
    }
    let mut parent = (len - 2) / 2;
    loop {
        adjust_heap(v, parent, len, less);
        if parent == 0 {
            break;
        }
        parent -= 1;
    }
    for end in (1..len).rev() {
        v.swap(0, end);
        adjust_heap(v, 0, end, less);
    }
}

/// std::__adjust_heap with the hole at `hole` and the value that was there.
fn adjust_heap<T>(v: &mut [T], mut hole: usize, len: usize, less: &impl Fn(&T, &T) -> bool) {
    let top = hole;
    let mut child = hole;
    while child < (len - 1) / 2 {
        child = 2 * (child + 1);
        if less(&v[child], &v[child - 1]) {
            child -= 1;
        }
        v.swap(hole, child);
        hole = child;
    }
    if len % 2 == 0 && child == (len - 2) / 2 {
        child = 2 * (child + 1);
        v.swap(hole, child - 1);
        hole = child - 1;
    }
    // std::__push_heap: sift the moved value back up towards `top`.
    while hole > top {
        let parent = (hole - 1) / 2;
        if parent < top || !less(&v[parent], &v[hole]) {
            break;
        }
        v.swap(parent, hole);
        hole = parent;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sorts_like_a_sort() {
        let mut v: Vec<(i32, usize)> = (0..200).map(|i| ((i * 37 % 11) as i32, i)).collect();
        std_sort(&mut v, &|a, b| a.0 < b.0);
        assert!(v.windows(2).all(|w| w[0].0 <= w[1].0));
        // Small inputs are insertion sorted: stable.
        let mut s: Vec<(i32, usize)> = (0..10).map(|i| ((i % 3) as i32, i)).collect();
        std_sort(&mut s, &|a, b| a.0 < b.0);
        assert_eq!(s.iter().map(|x| x.1).collect::<Vec<_>>(), [0, 3, 6, 9, 1, 4, 7, 2, 5, 8]);
    }
}
