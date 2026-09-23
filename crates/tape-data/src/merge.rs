//! Merging several time-ordered event streams into one.
//!
//! A backtest over BTC and ETH needs a single stream in which every event
//! appears in the order it happened, across both. This is a k-way merge on
//! a min-heap keyed by `(timestamp, source index)`. Ties are broken by the
//! order the sources were added, never by anything that could vary between
//! runs, so the merged order is identical every time.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use tape_core::{MarketEvent, Timestamp};

use crate::error::{DataError, Result};

pub type EventStream = Box<dyn Iterator<Item = Result<MarketEvent>>>;

struct Source {
    name: String,
    events: EventStream,
    head: Option<MarketEvent>,
    last: Option<Timestamp>,
}

pub struct Merge {
    sources: Vec<Source>,
    heap: BinaryHeap<Reverse<(Timestamp, usize)>>,
    started: bool,
    failed: bool,
    /// An error hit while reading ahead, held back until the event read
    /// before it has been delivered.
    pending_error: Option<DataError>,
}

impl Merge {
    pub fn new() -> Self {
        Self {
            sources: Vec::new(),
            heap: BinaryHeap::new(),
            started: false,
            failed: false,
            pending_error: None,
        }
    }

    /// Adds a stream. Each stream must already be in time order; the merge
    /// checks this and reports an error rather than reordering. Earlier
    /// streams win ties.
    pub fn add(
        &mut self,
        name: impl Into<String>,
        events: impl Iterator<Item = Result<MarketEvent>> + 'static,
    ) {
        assert!(!self.started, "add all sources before reading");
        self.sources.push(Source {
            name: name.into(),
            events: Box::new(events),
            head: None,
            last: None,
        });
    }

    /// Pulls the next event from a source into its head slot and onto the heap.
    fn advance(&mut self, index: usize) -> Result<()> {
        let source = &mut self.sources[index];
        match source.events.next() {
            None => source.head = None,
            Some(Err(err)) => return Err(err),
            Some(Ok(event)) => {
                let ts = event.ts();
                if let Some(last) = source.last
                    && ts < last
                {
                    return Err(DataError::OutOfOrder {
                        source_name: source.name.clone(),
                        previous: last,
                        next: ts,
                    });
                }
                source.last = Some(ts);
                source.head = Some(event);
                self.heap.push(Reverse((ts, index)));
            }
        }
        Ok(())
    }
}

impl Default for Merge {
    fn default() -> Self {
        Self::new()
    }
}

impl Iterator for Merge {
    type Item = Result<MarketEvent>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        if let Some(err) = self.pending_error.take() {
            self.failed = true;
            return Some(Err(err));
        }
        if !self.started {
            self.started = true;
            for index in 0..self.sources.len() {
                if let Err(err) = self.advance(index) {
                    self.failed = true;
                    return Some(Err(err));
                }
            }
        }
        let Reverse((_, index)) = self.heap.pop()?;
        let event = self.sources[index]
            .head
            .take()
            .expect("heap entry without a head event");
        if let Err(err) = self.advance(index) {
            self.pending_error = Some(err);
        }
        Some(Ok(event))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tape_core::{InstrumentId, Side, Trade};

    fn trade(instrument: u32, millis: i64) -> MarketEvent {
        MarketEvent::Trade(Trade {
            ts: Timestamp::from_millis(millis),
            instrument: InstrumentId(instrument),
            price: "1".parse().unwrap(),
            qty: "1".parse().unwrap(),
            aggressor: Side::Buy,
        })
    }

    fn stream(events: Vec<MarketEvent>) -> impl Iterator<Item = Result<MarketEvent>> {
        events.into_iter().map(Ok)
    }

    fn order(merge: Merge) -> Vec<(u32, i64)> {
        merge
            .map(|e| {
                let e = e.unwrap();
                (e.instrument().0, e.ts().as_millis())
            })
            .collect()
    }

    #[test]
    fn interleaves_by_time_and_breaks_ties_by_source_order() {
        let mut merge = Merge::new();
        merge.add(
            "a",
            stream(vec![trade(1, 10), trade(1, 20), trade(1, 20), trade(1, 40)]),
        );
        merge.add("b", stream(vec![trade(2, 5), trade(2, 20), trade(2, 30)]));
        merge.add("c", stream(vec![]));
        assert_eq!(
            order(merge),
            vec![(2, 5), (1, 10), (1, 20), (1, 20), (2, 20), (2, 30), (1, 40)]
        );
    }

    #[test]
    fn is_identical_across_runs() {
        let build = || {
            let mut merge = Merge::new();
            for id in 0..5 {
                // Overlapping, tied timestamps across sources.
                let events = (0..200)
                    .map(|i| trade(id, i * (i64::from(id) % 3 + 1)))
                    .collect();
                merge.add(format!("s{id}"), stream(events));
            }
            order(merge)
        };
        assert_eq!(build(), build());
    }

    #[test]
    fn reports_a_source_that_goes_backwards() {
        let mut merge = Merge::new();
        merge.add("good", stream(vec![trade(1, 1), trade(1, 2)]));
        merge.add("bad", stream(vec![trade(2, 5), trade(2, 3)]));
        let results: Vec<_> = merge.collect();
        let delivered: Vec<i64> = results
            .iter()
            .filter_map(|r| r.as_ref().ok())
            .map(|e| e.ts().as_millis())
            .collect();
        // Everything before the bad event is delivered, in order.
        assert_eq!(delivered, vec![1, 2, 5]);
        let err = results.last().unwrap().as_ref().unwrap_err();
        assert!(
            err.to_string().contains("bad: timestamps go backwards"),
            "{err}"
        );
    }

    #[test]
    fn passes_through_source_errors() {
        let mut merge = Merge::new();
        let failing = vec![
            Ok(trade(1, 1)),
            Err(DataError::Parse {
                source_name: "x".into(),
                line: 2,
                message: "boom".into(),
            }),
        ];
        merge.add("x", failing.into_iter());
        let results: Vec<_> = merge.collect();
        assert!(results[0].is_ok());
        assert!(
            results[1]
                .as_ref()
                .unwrap_err()
                .to_string()
                .contains("boom")
        );
        assert_eq!(results.len(), 2);
    }
}
