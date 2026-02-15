use crate::ProgressEntry;

pub trait Merge {
    fn merge_progress(&mut self, new: ProgressEntry);
}

impl Merge for Vec<ProgressEntry> {
    fn merge_progress(&mut self, new: ProgressEntry) {
        let i = self.partition_point(|x| x.end < new.start);
        if i == self.len() {
            self.push(new);
            return;
        }
        if self[i].start <= new.start && self[i].end >= new.end {
            return;
        }
        let mut current_merge = new;
        let mut j = i;
        while j < self.len() {
            let entry = &self[j];
            if entry.start > current_merge.end {
                break;
            }
            current_merge.start = current_merge.start.min(entry.start);
            current_merge.end = current_merge.end.max(entry.end);
            j += 1;
        }
        if j > i {
            self.drain(i..j);
        }
        self.insert(i, current_merge);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_merge() {
        #[allow(clippy::single_range_in_vec_init)]
        let mut v = vec![1..5, 8..10];
        v.merge_progress(5..10);
        assert_eq!(v, vec![1..10]);
        v.merge_progress(10..20);
        assert_eq!(v, vec![1..20]);
        v.merge_progress(30..40);
        assert_eq!(v, vec![1..20, 30..40]);
        v.merge_progress(21..40);
        assert_eq!(v, vec![1..20, 21..40]);
        v.merge_progress(19..21);
        assert_eq!(v, vec![1..40]);
        v.merge_progress(50..60);
        assert_eq!(v, vec![1..40, 50..60]);
        v.merge_progress(50..60);
        assert_eq!(v, vec![1..40, 50..60]);
        v.merge_progress(52..60);
        assert_eq!(v, vec![1..40, 50..60]);
        v.merge_progress(52..53);
        assert_eq!(v, vec![1..40, 50..60]);
        v.merge_progress(52..61);
        assert_eq!(v, vec![1..40, 50..61]);
        v.merge_progress(62..70);
        assert_eq!(v, vec![1..40, 50..61, 62..70]);
        v.merge_progress(40..62);
        assert_eq!(v, vec![1..70]);
        v.merge_progress(72..82);
        assert_eq!(v, vec![1..70, 72..82]);
        v.merge_progress(0..90);
        assert_eq!(v, vec![0..90]);
    }
}
