//! `BarAnalyzer::process` must not allocate: it runs on the audio thread.
//!
//! A counting global allocator needs `unsafe`; this test file is the only exception to the
//! workspace-wide `unsafe_code = "deny"`.

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use live_mcp_analysis::{BarAnalyzer, BeatAnalyzer, Transport};

struct CountingAllocator;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.with(Cell::get) {
            ALLOCATIONS.with(|a| a.set(a.get() + 1));
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

#[test]
fn processing_does_not_allocate() {
    let sample_rate = 44_100.0;
    let mut analyzer = BarAnalyzer::new(sample_rate);
    let block: Vec<f32> = (0..512).map(|i| (i as f32 * 0.05).sin() * 0.3).collect();
    let mut reports = 0usize;
    let mut stats_sum = 0.0f32;

    COUNTING.with(|c| c.set(true));
    // About 20 s at 128 BPM, with a loop jump and a stop in the middle.
    for k in 0..1700u32 {
        let beat = if k < 900 { k as f64 } else { (k - 900) as f64 } * 512.0 * 128.0 / 60.0 / sample_rate as f64;
        let playing = !(1000..1010).contains(&k);
        let transport = Transport { playing, beat: Some(beat), tempo: Some(128.0), time_signature: Some((4, 4)) };
        analyzer.process(&block, &block, &transport, &mut |r| {
            reports += 1;
            stats_sum += r.stats.loudness_lufs;
        });
    }
    analyzer.flush(&mut |_| reports += 1);
    COUNTING.with(|c| c.set(false));

    assert_eq!(ALLOCATIONS.with(Cell::get), 0, "BarAnalyzer allocated on the audio path");
    assert!(reports >= 9, "{reports} reports");
    assert!(stats_sum.is_finite());
}

#[test]
fn beat_analysis_does_not_allocate() {
    let sample_rate = 48_000.0;
    let mut analyzer = BeatAnalyzer::new(sample_rate);
    let block: Vec<f32> = (0..256).map(|i| (i as f32 * 0.057).sin() * 0.3).collect();
    let mut reports = 0usize;
    let mut tonal = 0.0f32;

    COUNTING.with(|c| c.set(true));
    // About 10 s at 140 BPM, with a loop jump and a stop in the middle.
    for k in 0..1900u32 {
        let beat = if k < 1000 { k as f64 } else { (k - 1000) as f64 } * 256.0 * 140.0 / 60.0 / sample_rate as f64;
        let playing = !(1200..1210).contains(&k);
        let transport = Transport { playing, beat: Some(beat), tempo: Some(140.0), time_signature: Some((4, 4)) };
        analyzer.process(&block, &block, &transport, &mut |r| {
            reports += 1;
            tonal += r.tonal;
        });
    }
    analyzer.flush(&mut |_| reports += 1);
    COUNTING.with(|c| c.set(false));

    assert_eq!(ALLOCATIONS.with(Cell::get), 0, "BeatAnalyzer allocated on the audio path");
    assert!(reports >= 20, "{reports} reports");
    assert!(tonal.is_finite());
}
