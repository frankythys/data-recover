//! Surface test: reads a whole disk once, only reading, and maps which
//! parts read well, slowly or not at all. Slow areas are often the first
//! sign of a failing hard disk; unreadable ones mean data is already lost.

use crate::imaging::BlockSource;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const CHUNK: usize = 1 << 20;
const RETRY_BLOCK: usize = 4096;
/// A 1 MiB read slower than this (under ~3 MB/s) marks its area slow.
const SLOW_READ: Duration = Duration::from_millis(300);
/// Offsets of unreadable blocks kept for the report.
const MAX_LISTED_BAD: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Cell {
    Pending,
    Good,
    Slow,
    Bad,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SurfaceProgress {
    pub done: u64,
    pub total: u64,
    /// The disk divided into equal areas; each shows the worst result in it.
    pub cells: Vec<Cell>,
    /// Bytes that could not be read.
    pub bad_bytes: u64,
    /// Start of the first unreadable blocks.
    pub bad_offsets: Vec<u64>,
    pub slow_chunks: u64,
    pub finished: bool,
    pub cancelled: bool,
}

impl SurfaceProgress {
    /// Marks the areas that `offset..offset + len` falls in as `cell`,
    /// unless they already have a worse result.
    fn mark(&mut self, offset: u64, len: u64, cell: Cell) {
        if self.total == 0 || self.cells.is_empty() {
            return;
        }
        let n = self.cells.len() as u128;
        let index = |at: u64| ((u128::from(at) * n / u128::from(self.total)) as usize).min(self.cells.len() - 1);
        let (first, last) = (index(offset), index(offset + len.max(1) - 1));
        for c in &mut self.cells[first..=last] {
            if cell > *c {
                *c = cell;
            }
        }
    }
}

/// Reads all of `source`, dividing it into `cells` areas for the map.
pub fn test(
    source: &mut impl BlockSource,
    cells: usize,
    cancel: &AtomicBool,
    paused: &AtomicBool,
    mut report: impl FnMut(&SurfaceProgress),
) -> io::Result<SurfaceProgress> {
    let total = source.len()?;
    let mut p = SurfaceProgress { total, cells: vec![Cell::Pending; cells], ..Default::default() };
    let mut last_report = Instant::now();
    report(&p);
    while p.done < total {
        while paused.load(Ordering::Relaxed) && !cancel.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(60));
        }
        if cancel.load(Ordering::Relaxed) {
            p.cancelled = true;
            break;
        }
        let want = (total - p.done).min(CHUNK as u64) as usize;
        let started = Instant::now();
        let read = source.read_block(p.done, want);
        let took = started.elapsed();
        match read {
            Ok(b) if b.len() == want => {
                let slow = took > SLOW_READ.mul_f64(want as f64 / CHUNK as f64);
                p.slow_chunks += u64::from(slow);
                p.mark(p.done, want as u64, if slow { Cell::Slow } else { Cell::Good });
            }
            _ => test_blocks(source, p.done, want, &mut p),
        }
        p.done += want as u64;
        if last_report.elapsed().as_millis() > 150 {
            report(&p);
            last_report = Instant::now();
        }
    }
    p.finished = true;
    report(&p);
    Ok(p)
}

/// Reads a chunk that failed block by block to find the unreadable blocks.
fn test_blocks(source: &mut impl BlockSource, offset: u64, len: usize, p: &mut SurfaceProgress) {
    let mut pos = 0;
    while pos < len {
        let n = (len - pos).min(RETRY_BLOCK);
        let at = offset + pos as u64;
        match source.read_block(at, n) {
            Ok(b) if b.len() == n => p.mark(at, n as u64, Cell::Slow),
            _ => {
                p.bad_bytes += n as u64;
                if p.bad_offsets.len() < MAX_LISTED_BAD {
                    p.bad_offsets.push(at);
                }
                p.mark(at, n as u64, Cell::Bad);
            }
        }
        pos += n;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeDisk {
        len: u64,
        bad: Vec<u64>,
    }

    impl BlockSource for FakeDisk {
        fn len(&mut self) -> io::Result<u64> {
            Ok(self.len)
        }

        fn read_block(&mut self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
            let range = offset..offset + len as u64;
            if self.bad.iter().any(|b| range.contains(b)) {
                return Err(io::Error::other("CRC-fout"));
            }
            Ok(vec![0; len])
        }
    }

    fn run(disk: &mut FakeDisk, cells: usize) -> SurfaceProgress {
        test(disk, cells, &AtomicBool::new(false), &AtomicBool::new(false), |_| {}).unwrap()
    }

    #[test]
    fn healthy_disk_is_good_everywhere() {
        let p = run(&mut FakeDisk { len: 4 * CHUNK as u64 + 512, bad: vec![] }, 10);

        assert!(p.cells.iter().all(|&c| c == Cell::Good));
        assert_eq!((p.done, p.bad_bytes, p.finished), (4 * CHUNK as u64 + 512, 0, true));
    }

    #[test]
    fn unreadable_blocks_are_found_and_mapped() {
        let bad_at = 2 * CHUNK as u64 + 5 * 4096 + 100;
        let p = run(&mut FakeDisk { len: 4 * CHUNK as u64, bad: vec![bad_at] }, 4);

        assert_eq!(p.bad_bytes, 4096);
        assert_eq!(p.bad_offsets, [2 * CHUNK as u64 + 5 * 4096]);
        assert_eq!(p.cells, [Cell::Good, Cell::Good, Cell::Bad, Cell::Good]);
    }

    #[test]
    fn a_cell_keeps_its_worst_result() {
        let mut p = SurfaceProgress { total: 100, cells: vec![Cell::Pending; 2], ..Default::default() };

        p.mark(0, 10, Cell::Bad);
        p.mark(10, 10, Cell::Good);
        p.mark(40, 20, Cell::Slow);

        assert_eq!(p.cells, [Cell::Bad, Cell::Slow]);
    }

    #[test]
    fn stops_when_cancelled() {
        let p = test(&mut FakeDisk { len: CHUNK as u64, bad: vec![] }, 4, &AtomicBool::new(true), &AtomicBool::new(false), |_| {})
            .unwrap();

        assert!(p.cancelled);
        assert!(p.cells.iter().all(|&c| c == Cell::Pending));
    }
}
