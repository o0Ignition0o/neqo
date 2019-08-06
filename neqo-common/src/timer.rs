// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::convert::TryFrom;
use std::mem;
use std::time::{Duration, Instant};

/// Internal structure for a timer item.
struct TimerItem<T> {
    time: Instant,
    item: T,
}

impl<T> TimerItem<T> {
    fn time(ti: &TimerItem<T>) -> Instant {
        ti.time
    }
}

// impl<T> PartialOrd for TimerItem<T> {
//     fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
//         Some(self.cmp(&other))
//     }
// }

// impl<T> Ord for TimerItem<T> {
//     fn cmp(&self, other: &Self) -> Ordering {
//         self.time.cmp(&other.time)
//     }
// }

// impl<T> PartialEq for TimerItem<T> {
//     fn eq(&self, other: &Self) -> bool {
//         self.time == other.time
//     }
// }

// impl<T> Eq for TimerItem<T> {}

/// A timer queue.
pub struct Timer<T> {
    items: Vec<Vec<TimerItem<T>>>,
    now: Instant,
    granularity: Duration,
    cursor: usize,
}

impl<T> Timer<T> {
    /// Construct a new wheel at the given granularity, starting at the given time.
    pub fn new(now: Instant, granularity: Duration, capacity: usize) -> Timer<T> {
        assert!(u32::try_from(capacity).is_ok());
        assert!(granularity.as_nanos() > 0);
        let mut items = Vec::with_capacity(capacity);
        items.resize_with(capacity, Default::default);
        Timer {
            items,
            now,
            granularity,
            cursor: 0,
        }
    }

    /// Return a reference to the time of the next entry.
    pub fn next_time(&self) -> Option<Instant> {
        for i in 0..self.items.len() {
            let idx = (self.cursor + i) % self.items.len();
            if let Some(t) = self.items[idx].first() {
                return Some(t.time);
            }
        }
        None
    }

    /// Slide forward in time by `self.granularity`.
    fn tick(&mut self) {
        assert!(self.items[self.cursor].is_empty());
        self.now += self.granularity;
        self.cursor = (self.cursor + 1) % self.items.len();
    }

    /// Get the full span of time that this can cover.
    /// Two timers cannot be more than this far apart.
    /// In practice, this value is less by one amount of the timer granularity.
    #[inline]
    pub fn span(&self) -> Duration {
        self.granularity * (self.items.len() as u32)
    }

    /// For the given `time`, get the number of whole buckets in the future that is.
    #[inline]
    fn delta(&self, time: Instant) -> usize {
        // This really should use Instant::div_duration(), but it can't yet.
        let delta = ((time - self.now).as_nanos() / self.granularity.as_nanos()) as usize;
        debug_assert!(delta < self.items.len());
        delta
    }

    /// Asserts if the time given is in the past or too far in the future.
    pub fn add(&mut self, time: Instant, item: T) {
        assert!(time >= self.now);
        // Skip forward quickly if there is too large a gap.
        let short_span = self.span() - self.granularity;
        if time >= (self.now + self.span() + short_span) {
            // Assert that there aren't any items.
            for i in &self.items {
                assert!(i.is_empty());
            }
            self.now = time - short_span;
            self.cursor = 0;
        }

        // Adjust time forward as much as is necessary.
        // This will assert if it is forced to discard a value.
        while time >= self.now + self.span() {
            self.tick();
        }
        let bucket = (self.cursor + self.delta(time)) % self.items.len();
        let ins = match self.items[bucket].binary_search_by_key(&time, TimerItem::time) {
            Ok(j) => j,
            Err(j) => j,
        };
        self.items[bucket].insert(ins, TimerItem { time, item });
    }

    /// Given knowledge of the time an item was added, remove it.
    /// This requires use of a predicate that identifies matching items.
    pub fn remove<F>(&mut self, time: Instant, mut selector: F) -> Option<T>
    where
        F: FnMut(&T) -> bool,
    {
        let bucket = (self.cursor + self.delta(time)) % self.items.len();
        let start_index = match self.items[bucket].binary_search_by_key(&time, TimerItem::time) {
            Ok(idx) => idx,
            _ => return None,
        };
        // start_index is just one of potentially many items with the same time.
        // Search backwards for a match, ...
        for i in 0..=start_index {
            let idx = start_index - i;
            if self.items[bucket][idx].time != time {
                break;
            }
            if selector(&self.items[bucket][idx].item) {
                return Some(self.items[bucket].remove(idx).item);
            }
        }
        // ... then forwards.
        for i in 1..(self.items[bucket].len() - start_index) {
            let idx = start_index + i;
            if self.items[bucket][idx].time != time {
                break;
            }
            if selector(&self.items[bucket][idx].item) {
                return Some(self.items[bucket].remove(idx).item);
            }
        }
        None
    }

    /// Take the next item, unless there are no items with
    /// a timeout in the past relative to `until`.
    pub fn take_next(&mut self, until: Instant) -> Option<T> {
        loop {
            if !self.items[self.cursor].is_empty() && self.items[self.cursor][0].time <= until {
                return Some(self.items[self.cursor].remove(0).item);
            }
            if until > self.now + self.granularity {
                self.tick();
            } else {
                return None;
            }
        }
    }

    /// Create an iterator that takes all items until the given time.
    /// Note: Items might be removed even if the iterator is either leaked
    ///   or not fully exhausted.
    pub fn take_until(&mut self, until: Instant) -> impl Iterator<Item = T> {
        let get_item = move |x: TimerItem<T>| x.item;
        if until >= self.now + self.span() {
            // Drain everything, so a clean sweep.
            let mut empty_items = Vec::with_capacity(self.items.len());
            empty_items.resize_with(self.items.len(), Default::default);
            let mut items = mem::replace(&mut self.items, empty_items);
            self.now = until;
            self.cursor = 0;

            let tail = items.split_off(self.cursor);
            return tail.into_iter().chain(items).flatten().map(get_item);
        }

        // Only returning a partial span, so do it bucket at a time.
        let delta = self.delta(until);
        let mut buckets = Vec::with_capacity(delta + 1);

        // First, the whole buckets.
        for _ in 0..delta {
            buckets.push(mem::replace(
                &mut self.items[self.cursor],
                Default::default(),
            ));
            self.tick();
        }

        // Now we need to split the last bucket, because there might be
        // some items with `item.time > until`.
        let bucket = &mut self.items[self.cursor];
        let last_idx = match bucket.binary_search_by_key(&until, TimerItem::time) {
            Ok(mut m) => {
                // If there are multiple values, the search will hit any of them.
                // Make sure to get them all.
                while m < bucket.len() && bucket[m].time == until {
                    m += 1;
                }
                m
            }
            Err(ins) => ins,
        };
        let tail = bucket.split_off(last_idx);
        buckets.push(mem::replace(bucket, tail));
        // This tomfoolery with the empty vector ensures that
        // the returned type here matches the one above precisely.
        buckets.into_iter().chain(vec![]).flatten().map(get_item)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use lazy_static::lazy_static;

    lazy_static! {
        static ref NOW: Instant = Instant::now();
    }

    const GRANULARITY: Duration = Duration::from_millis(10);
    const CAPACITY: usize = 10;
    #[test]
    fn create() {
        let t: Timer<()> = Timer::new(*NOW, GRANULARITY, CAPACITY);
        assert_eq!(t.span(), Duration::from_millis(100));
        assert_eq!(None, t.next_time());
    }

    #[test]
    fn immediate_entry() {
        let mut t = Timer::new(*NOW, GRANULARITY, CAPACITY);
        t.add(*NOW, 12);
        assert_eq!(*NOW, t.next_time().expect("should have an entry"));
        let values: Vec<_> = t.take_until(*NOW).collect();
        assert_eq!(vec![12], values);
    }

    #[test]
    fn same_time() {
        let mut t = Timer::new(*NOW, GRANULARITY, CAPACITY);
        let v1 = 12;
        let v2 = 13;
        t.add(*NOW, v1);
        t.add(*NOW, v2);
        assert_eq!(*NOW, t.next_time().expect("should have an entry"));
        let values: Vec<_> = t.take_until(*NOW).collect();
        assert!(values.contains(&v1));
        assert!(values.contains(&v2));
    }

    #[test]
    fn add() {
        let mut t = Timer::new(*NOW, GRANULARITY, CAPACITY);
        let near_future = *NOW + Duration::from_millis(17);
        let v = 9;
        t.add(near_future, v);
        assert_eq!(near_future, t.next_time().expect("should return a value"));
        let values: Vec<_> = t
            .take_until(near_future - Duration::from_millis(1))
            .collect();
        assert!(values.is_empty());
        let values: Vec<_> = t
            .take_until(near_future + Duration::from_millis(1))
            .collect();
        assert!(values.contains(&v));
    }

    #[test]
    fn add_future() {
        let mut t = Timer::new(*NOW, GRANULARITY, CAPACITY);
        let future = *NOW + Duration::from_millis(117);
        let v = 9;
        t.add(future, v);
        assert_eq!(future, t.next_time().expect("should return a value"));
        let values: Vec<_> = t.take_until(future).collect();
        assert!(values.contains(&v));
    }

    #[test]
    fn add_far_future() {
        let mut t = Timer::new(*NOW, GRANULARITY, CAPACITY);
        let far_future = *NOW + Duration::from_millis(892);
        let v = 9;
        t.add(far_future, v);
        assert_eq!(far_future, t.next_time().expect("should return a value"));
        let values: Vec<_> = t.take_until(far_future).collect();
        assert!(values.contains(&v));
    }

    const TIMES: &[Duration] = &[
        Duration::from_millis(40),
        Duration::from_millis(91),
        Duration::from_millis(6),
        Duration::from_millis(3),
        Duration::from_millis(22),
        Duration::from_millis(40),
    ];

    fn with_times() -> Timer<usize> {
        let mut t = Timer::new(*NOW, GRANULARITY, CAPACITY);
        for i in 0..TIMES.len() {
            t.add(*NOW + TIMES[i], i);
        }
        assert_eq!(
            *NOW + *TIMES.iter().min().unwrap(),
            t.next_time().expect("should have a time")
        );
        t
    }

    #[test]
    fn multiple_values() {
        let mut t = with_times();
        let values: Vec<_> = t.take_until(*NOW + *TIMES.iter().max().unwrap()).collect();
        for i in 0..TIMES.len() {
            assert!(values.contains(&i));
        }
    }

    #[test]
    fn take_far_future() {
        let mut t = with_times();
        let values: Vec<_> = t.take_until(*NOW + Duration::from_secs(100)).collect();
        for i in 0..TIMES.len() {
            assert!(values.contains(&i));
        }
    }

    #[test]
    fn remove_each() {
        let mut t = with_times();
        for i in 0..TIMES.len() {
            assert_eq!(Some(i), t.remove(*NOW + TIMES[i], |&x| x == i));
        }
        assert_eq!(None, t.next_time());
    }
}