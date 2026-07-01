use std::sync::atomic::AtomicU32;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use serde::Serialize;

/// A lock-free, single-producer single-consumer ring buffer for audio samples.
///
/// The producer (audio callback thread) writes f32 samples via [`write`], [`write_i16`],
/// or [`write_u16`]. These methods are zero-allocation and non-blocking.
///
/// The consumer (application thread) reads snapshots via [`snapshot`], [`snapshot_samples`],
/// or [`snapshot_all`]. These methods allocate a `Vec<f32>` on the calling thread.
///
/// Samples are stored in atomics (`AtomicU32` bit patterns) so concurrent reads/writes
/// are memory-safe even during wraparound while preserving lock-free callback behavior.
pub struct AudioRingBuffer {
    buffer: Box<[AtomicU32]>,
    write_pos: AtomicUsize,
    capacity: usize,
    sample_rate: u32,
    channels: u16,
}

/// Manual `Debug` impl that prints buffer metadata only — never the sample
/// contents. A derived impl would walk the `Box<[AtomicU32]>` and dump up to
/// `capture_history_seconds * sample_rate * channels` atomic values (millions),
/// which would dominate any log line `RestartReport` lands in.
impl std::fmt::Debug for AudioRingBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioRingBuffer")
            .field("capacity", &self.capacity)
            .field("sample_rate", &self.sample_rate)
            .field("channels", &self.channels)
            .field("write_pos", &self.write_pos.load(Ordering::Relaxed))
            .finish()
    }
}

pub type SharedAudioRingBuffer = Arc<AudioRingBuffer>;

/// Result of reading a bounded cursor range from the ring buffer.
#[derive(Debug, Clone)]
pub struct AudioRangeSnapshot {
    pub samples: Vec<f32>,
    /// First monotonic cursor position represented by `samples`.
    pub start_pos: usize,
    /// One-past-the-end monotonic cursor position represented by `samples`.
    pub end_pos: usize,
    /// True when `start_pos` had to be advanced because older samples were
    /// already overwritten by the ring buffer.
    pub overrun: bool,
}

/// Diagnostic information about a ring buffer's state.
#[derive(Debug, Clone, Serialize)]
pub struct RingBufferInfo {
    pub capacity_samples: usize,
    pub samples_written: usize,
    pub available_samples: usize,
    pub capacity_seconds: f32,
    pub available_seconds: f32,
    pub sample_rate: u32,
    pub channels: u16,
}

impl AudioRingBuffer {
    /// Creates a ring buffer with the given fixed sample capacity.
    pub fn new(capacity: usize, sample_rate: u32, channels: u16) -> Self {
        assert!(capacity > 0, "ring buffer capacity must be > 0");
        assert!(sample_rate > 0, "sample rate must be > 0");
        assert!(channels > 0, "channels must be > 0");

        Self {
            buffer: std::iter::repeat_with(|| AtomicU32::new(0.0f32.to_bits()))
                .take(capacity)
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            write_pos: AtomicUsize::new(0),
            capacity,
            sample_rate,
            channels,
        }
    }

    /// Creates a ring buffer sized to hold `duration_seconds` of audio.
    pub fn with_duration(duration_seconds: f32, sample_rate: u32, channels: u16) -> Self {
        assert!(duration_seconds > 0.0, "duration must be > 0.0");
        let capacity = (duration_seconds * sample_rate as f32 * channels as f32) as usize;
        Self::new(capacity, sample_rate, channels)
    }

    /// Returns the sample capacity of the buffer.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Returns the sample rate.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Returns the channel count.
    pub fn channels(&self) -> u16 {
        self.channels
    }

    // --- Producer API (audio callback thread, zero-alloc, non-blocking) ---

    /// Writes f32 samples into the ring buffer.
    ///
    /// This is safe to call from a real-time audio callback: it performs no
    /// allocation, no locking, and no system calls.
    pub fn write(&self, data: &[f32]) {
        if data.is_empty() {
            return;
        }

        let pos = self.write_pos.load(Ordering::Relaxed);
        let original_len = data.len();

        // If data is larger than capacity, only write the last `capacity` samples.
        // We still advance write_pos by the full amount so the monotonic counter
        // stays consistent.
        let data = if data.len() > self.capacity {
            &data[data.len() - self.capacity..]
        } else {
            data
        };

        // Compute the effective write offset after skipping samples
        let effective_pos = pos + (original_len - data.len());

        let offset = effective_pos % self.capacity;

        if offset + data.len() <= self.capacity {
            for (i, &sample) in data.iter().enumerate() {
                self.buffer[offset + i].store(sample.to_bits(), Ordering::Relaxed);
            }
        } else {
            // Two-part copy: end of buffer, then start
            let first = self.capacity - offset;
            for (i, &sample) in data[..first].iter().enumerate() {
                self.buffer[offset + i].store(sample.to_bits(), Ordering::Relaxed);
            }
            for (i, &sample) in data[first..].iter().enumerate() {
                self.buffer[i].store(sample.to_bits(), Ordering::Relaxed);
            }
        }

        // Commit the write position with Release ordering so the consumer
        // sees all the data we just wrote.
        self.write_pos.store(pos + original_len, Ordering::Release);
    }

    /// Writes i16 samples, converting to f32 via a stack scratch buffer.
    ///
    /// Uses a 512-sample scratch buffer (2KB on stack) to avoid heap allocation.
    pub fn write_i16(&self, data: &[i16]) {
        const SCRATCH_SIZE: usize = 512;
        let mut scratch = [0.0f32; SCRATCH_SIZE];

        for chunk in data.chunks(SCRATCH_SIZE) {
            for (i, &sample) in chunk.iter().enumerate() {
                scratch[i] = sample as f32 / i16::MAX as f32;
            }
            self.write(&scratch[..chunk.len()]);
        }
    }

    /// Writes u16 samples, converting to f32 via a stack scratch buffer.
    ///
    /// Uses a 512-sample scratch buffer (2KB on stack) to avoid heap allocation.
    pub fn write_u16(&self, data: &[u16]) {
        const SCRATCH_SIZE: usize = 512;
        let mut scratch = [0.0f32; SCRATCH_SIZE];

        for chunk in data.chunks(SCRATCH_SIZE) {
            for (i, &sample) in chunk.iter().enumerate() {
                scratch[i] = (sample as f32 / u16::MAX as f32) * 2.0 - 1.0;
            }
            self.write(&scratch[..chunk.len()]);
        }
    }

    // --- Consumer API (app thread, may allocate) ---

    /// Returns the last `duration_seconds` of audio in chronological order.
    pub fn snapshot(&self, duration_seconds: f32) -> Vec<f32> {
        let num_samples =
            (duration_seconds * self.sample_rate as f32 * self.channels as f32) as usize;
        self.snapshot_samples(num_samples)
    }

    /// Returns the last `num_samples` samples in chronological order.
    pub fn snapshot_samples(&self, num_samples: usize) -> Vec<f32> {
        let write_pos = self.write_pos.load(Ordering::Acquire);

        if write_pos == 0 {
            return Vec::new();
        }

        let available = write_pos.min(self.capacity);
        let to_read = num_samples.min(available);

        if to_read == 0 {
            return Vec::new();
        }

        let mut result = vec![0.0f32; to_read];

        let start_pos = write_pos - to_read;
        let start_offset = start_pos % self.capacity;

        if start_offset + to_read <= self.capacity {
            // Contiguous read
            for (i, sample) in result.iter_mut().enumerate() {
                *sample = f32::from_bits(self.buffer[start_offset + i].load(Ordering::Relaxed));
            }
        } else {
            // Two-part read: end of buffer, then start
            let first = self.capacity - start_offset;
            for (i, sample) in result[..first].iter_mut().enumerate() {
                *sample = f32::from_bits(self.buffer[start_offset + i].load(Ordering::Relaxed));
            }
            for (i, sample) in result[first..].iter_mut().enumerate() {
                *sample = f32::from_bits(self.buffer[i].load(Ordering::Relaxed));
            }
        }

        result
    }

    /// Returns all valid data in the buffer in chronological order.
    pub fn snapshot_all(&self) -> Vec<f32> {
        let write_pos = self.write_pos.load(Ordering::Acquire);
        let available = write_pos.min(self.capacity);
        self.snapshot_samples(available)
    }

    /// Returns diagnostic information about the buffer.
    pub fn info(&self) -> RingBufferInfo {
        let write_pos = self.write_pos.load(Ordering::Acquire);
        let available = write_pos.min(self.capacity);
        let samples_per_second = self.sample_rate as f32 * self.channels as f32;

        RingBufferInfo {
            capacity_samples: self.capacity,
            samples_written: write_pos,
            available_samples: available,
            capacity_seconds: self.capacity as f32 / samples_per_second,
            available_seconds: available as f32 / samples_per_second,
            sample_rate: self.sample_rate,
            channels: self.channels,
        }
    }

    /// Returns the total number of samples written (monotonic counter).
    pub fn samples_written(&self) -> usize {
        self.write_pos.load(Ordering::Acquire)
    }

    /// Returns samples written since `since_pos`, clamped to capacity.
    ///
    /// If more samples have been written since `since_pos` than the buffer can hold,
    /// only the most recent `capacity` samples are returned.
    pub fn snapshot_since(&self, since_pos: usize) -> Vec<f32> {
        self.snapshot_since_with_pos(since_pos).0
    }

    /// Like [`snapshot_since`], but also returns the exact `write_pos` used for the
    /// snapshot. This eliminates the race window between snapshotting data and
    /// querying the current write position — callers can use the returned position
    /// as the next `since_pos` without losing samples.
    pub fn snapshot_since_with_pos(&self, since_pos: usize) -> (Vec<f32>, usize) {
        let write_pos = self.write_pos.load(Ordering::Acquire);

        if write_pos <= since_pos {
            return (Vec::new(), since_pos);
        }

        let total_new = write_pos - since_pos;
        let to_read = total_new.min(self.capacity);

        let mut result = vec![0.0f32; to_read];

        let start_pos = write_pos - to_read;
        let start_offset = start_pos % self.capacity;

        if start_offset + to_read <= self.capacity {
            for (i, sample) in result.iter_mut().enumerate() {
                *sample = f32::from_bits(self.buffer[start_offset + i].load(Ordering::Relaxed));
            }
        } else {
            let first = self.capacity - start_offset;
            for (i, sample) in result[..first].iter_mut().enumerate() {
                *sample = f32::from_bits(self.buffer[start_offset + i].load(Ordering::Relaxed));
            }
            for (i, sample) in result[first..].iter_mut().enumerate() {
                *sample = f32::from_bits(self.buffer[i].load(Ordering::Relaxed));
            }
        }

        (result, write_pos)
    }

    /// Returns samples in the half-open monotonic cursor range
    /// `[from_pos, until_pos)`, capped to committed data. Unlike
    /// [`snapshot_since_with_pos`], this never reads past `until_pos`, which
    /// lets callers enforce a hard stop boundary even while capture continues.
    pub fn snapshot_range(&self, from_pos: usize, until_pos: usize) -> AudioRangeSnapshot {
        let write_pos = self.write_pos.load(Ordering::Acquire);
        let end_pos = until_pos.min(write_pos);

        if end_pos <= from_pos {
            return AudioRangeSnapshot {
                samples: Vec::new(),
                start_pos: from_pos,
                end_pos: from_pos,
                overrun: false,
            };
        }

        let earliest_pos = write_pos.saturating_sub(self.capacity);
        let start_pos = from_pos.max(earliest_pos);
        let overrun = start_pos > from_pos;

        if end_pos <= start_pos {
            return AudioRangeSnapshot {
                samples: Vec::new(),
                start_pos,
                end_pos: start_pos,
                overrun,
            };
        }

        let to_read = end_pos - start_pos;
        let mut result = vec![0.0f32; to_read];
        let start_offset = start_pos % self.capacity;

        if start_offset + to_read <= self.capacity {
            for (i, sample) in result.iter_mut().enumerate() {
                *sample = f32::from_bits(self.buffer[start_offset + i].load(Ordering::Relaxed));
            }
        } else {
            let first = self.capacity - start_offset;
            for (i, sample) in result[..first].iter_mut().enumerate() {
                *sample = f32::from_bits(self.buffer[start_offset + i].load(Ordering::Relaxed));
            }
            for (i, sample) in result[first..].iter_mut().enumerate() {
                *sample = f32::from_bits(self.buffer[i].load(Ordering::Relaxed));
            }
        }

        AudioRangeSnapshot {
            samples: result,
            start_pos,
            end_pos,
            overrun,
        }
    }

    /// Computes the RMS energy of committed samples since `since_pos` without allocating.
    ///
    /// Reads at most `max_samples` of the most recent data (clamped to capacity).
    /// Uses an `f64` accumulator for precision over large windows.
    /// Returns `None` if no new samples exist since `since_pos`.
    pub fn rms_energy_since(&self, since_pos: usize, max_samples: usize) -> Option<f32> {
        let write_pos = self.write_pos.load(Ordering::Acquire);

        if write_pos <= since_pos {
            return None;
        }

        let total_new = write_pos - since_pos;
        let to_read = total_new.min(self.capacity).min(max_samples);

        if to_read == 0 {
            return None;
        }

        let start_pos = write_pos - to_read;
        let start_offset = start_pos % self.capacity;

        let mut sum_sq: f64 = 0.0;
        if start_offset + to_read <= self.capacity {
            for i in 0..to_read {
                let s = f32::from_bits(self.buffer[start_offset + i].load(Ordering::Relaxed));
                sum_sq += (s as f64) * (s as f64);
            }
        } else {
            let first = self.capacity - start_offset;
            for i in 0..first {
                let s = f32::from_bits(self.buffer[start_offset + i].load(Ordering::Relaxed));
                sum_sq += (s as f64) * (s as f64);
            }
            for i in 0..(to_read - first) {
                let s = f32::from_bits(self.buffer[i].load(Ordering::Relaxed));
                sum_sq += (s as f64) * (s as f64);
            }
        }

        Some((sum_sq / to_read as f64).sqrt() as f32)
    }

    /// Resets the buffer, zeroing all data and resetting the write position.
    ///
    /// Only call this when capture is stopped (no concurrent writer).
    #[cfg(test)]
    pub fn reset(&self) {
        for sample in self.buffer.iter() {
            sample.store(0.0f32.to_bits(), Ordering::Relaxed);
        }
        self.write_pos.store(0, Ordering::Release);
    }
}

/// Small fixed time guard (in seconds) subtracted from `available_seconds`
/// when a backfill rewind would otherwise reach the very oldest committed
/// samples.
///
/// Why a *fixed constant* and not a percentage: `available_seconds` is sampled
/// once (under a lock), but the actual rewind + extraction happens slightly
/// later under a separate lock acquisition. In that gap the producer thread
/// keeps writing and overwrites the oldest samples. Rewinding by MORE than
/// `available@T0` (an explicit over-request) can therefore land the read cursor
/// behind `write_pos@T1 - capacity` — i.e. where committed data has already
/// been overwritten — producing a silently clamped snapshot. The exact-equal
/// boundary is intentionally NOT guarded (`clamp_backfill_to_available` honours
/// `requested == available`): real callers reach it only with a strictly-
/// smaller stale `available`, and `snapshot_since` clamps any residual to
/// committed data, so honouring a full "Full buffer" request there is benign.
///
/// The size of that race window is bounded by *elapsed wall-clock time* between
/// sampling and extraction (lock contention + setup), NOT by the buffer's
/// capacity. A fixed sub-second guard covers it exactly. A percentage margin
/// (the old `available * 0.95`) scaled the guard with capacity, so a 300s
/// buffer lost ~15s of tail audio even though the race window is only tens of
/// milliseconds. This constant is generous (50ms) relative to that window.
pub const BACKFILL_WRITE_HEAD_GUARD_SECONDS: f32 = 0.05;

/// Clamps a requested backfill rewind to what the ring buffer can actually and
/// safely yield, returning the effective rewind in seconds.
///
/// This is a pure helper (no I/O, no buffer access) so the clamp policy is unit
/// testable in isolation. Behaviour:
///
/// - `requested <= available` -> returns `requested` unchanged. The full
///   requested rewind is honoured when the buffer holds it; there is NO
///   proportional shave (this is the regression guard for the Full/Max tail
///   loss bug). The exact-equal boundary is intentionally in this branch: real
///   callers reach it only with a slightly-stale, strictly-smaller `available`
///   (so the rewind still lands inside committed data), and `snapshot_since`
///   clamps any residual over-read to committed samples rather than returning
///   garbage — so honouring a full "Full buffer" request here is safe.
/// - `requested > available` -> returns `available` minus the exact fixed
///   [`BACKFILL_WRITE_HEAD_GUARD_SECONDS`] guard (never less than 0). This is
///   the explicit over-request path (e.g. a ceil-rounded "all backfill"): the
///   caller asked for more than exists, so we step back by the small fixed
///   guard to stay clear of the live write head.
///
/// `available` here is the live snapshot of committed audio. The guard is a
/// fixed time constant, not a fraction of `available`, so it does not grow with
/// buffer size — see [`BACKFILL_WRITE_HEAD_GUARD_SECONDS`].
pub fn clamp_backfill_to_available(requested_seconds: f32, available_seconds: f32) -> f32 {
    if requested_seconds <= 0.0 || available_seconds <= 0.0 {
        return 0.0;
    }

    if requested_seconds <= available_seconds {
        // Buffer holds the full request: honour it exactly, no shave.
        requested_seconds
    } else {
        // Requesting at/over everything available: back off by the exact fixed
        // write-head guard so a concurrent writer can't overwrite the oldest
        // samples out from under the read cursor before extraction runs.
        (available_seconds - BACKFILL_WRITE_HEAD_GUARD_SECONDS).max(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new() {
        let buf = AudioRingBuffer::new(1024, 16000, 1);
        assert_eq!(buf.capacity(), 1024);
        assert_eq!(buf.sample_rate(), 16000);
        assert_eq!(buf.channels(), 1);
    }

    #[test]
    fn test_with_duration() {
        let buf = AudioRingBuffer::with_duration(1.0, 16000, 1);
        assert_eq!(buf.capacity(), 16000);

        let buf_stereo = AudioRingBuffer::with_duration(1.0, 44100, 2);
        assert_eq!(buf_stereo.capacity(), 88200);
    }

    #[test]
    fn test_with_duration_180s() {
        let buf = AudioRingBuffer::with_duration(180.0, 16000, 1);
        assert_eq!(buf.capacity(), 2_880_000);
    }

    #[test]
    #[should_panic(expected = "capacity must be > 0")]
    fn test_zero_capacity_panics() {
        AudioRingBuffer::new(0, 16000, 1);
    }

    #[test]
    fn test_write_and_snapshot_simple() {
        let buf = AudioRingBuffer::new(100, 16000, 1);
        let data: Vec<f32> = (0..10).map(|i| i as f32).collect();
        buf.write(&data);

        let snap = buf.snapshot_all();
        assert_eq!(snap.len(), 10);
        assert_eq!(snap, data);
    }

    #[test]
    fn test_write_wraparound() {
        let buf = AudioRingBuffer::new(10, 16000, 1);

        // Write 8 samples
        let data1: Vec<f32> = (0..8).map(|i| i as f32).collect();
        buf.write(&data1);

        // Write 5 more — wraps around
        let data2: Vec<f32> = (10..15).map(|i| i as f32).collect();
        buf.write(&data2);

        // Buffer should contain the last 10 samples
        let snap = buf.snapshot_all();
        assert_eq!(snap.len(), 10);
        // Should be: [3,4,5,6,7, 10,11,12,13,14]
        assert_eq!(
            snap,
            vec![3.0, 4.0, 5.0, 6.0, 7.0, 10.0, 11.0, 12.0, 13.0, 14.0]
        );
    }

    #[test]
    fn test_full_overwrite() {
        let buf = AudioRingBuffer::new(5, 16000, 1);

        // Write more than capacity
        let data: Vec<f32> = (0..20).map(|i| i as f32).collect();
        buf.write(&data);

        let snap = buf.snapshot_all();
        assert_eq!(snap.len(), 5);
        // Should contain the last 5 samples
        assert_eq!(snap, vec![15.0, 16.0, 17.0, 18.0, 19.0]);
    }

    #[test]
    fn test_snapshot_by_duration() {
        let buf = AudioRingBuffer::new(16000, 16000, 1);
        let data: Vec<f32> = (0..16000).map(|i| i as f32).collect();
        buf.write(&data);

        // Request 0.5 seconds = 8000 samples
        let snap = buf.snapshot(0.5);
        assert_eq!(snap.len(), 8000);
        // Should be the last 8000 samples
        assert_eq!(snap[0], 8000.0);
        assert_eq!(snap[7999], 15999.0);
    }

    #[test]
    fn test_snapshot_by_sample_count() {
        let buf = AudioRingBuffer::new(100, 16000, 1);
        let data: Vec<f32> = (0..50).map(|i| i as f32).collect();
        buf.write(&data);

        let snap = buf.snapshot_samples(20);
        assert_eq!(snap.len(), 20);
        // Last 20 samples: 30..50
        assert_eq!(snap[0], 30.0);
        assert_eq!(snap[19], 49.0);
    }

    #[test]
    fn test_snapshot_empty_buffer() {
        let buf = AudioRingBuffer::new(100, 16000, 1);
        let snap = buf.snapshot_all();
        assert!(snap.is_empty());

        let snap = buf.snapshot(1.0);
        assert!(snap.is_empty());

        let snap = buf.snapshot_samples(10);
        assert!(snap.is_empty());
    }

    #[test]
    fn test_snapshot_more_than_available() {
        let buf = AudioRingBuffer::new(100, 16000, 1);
        let data: Vec<f32> = (0..10).map(|i| i as f32).collect();
        buf.write(&data);

        // Request more than available
        let snap = buf.snapshot_samples(50);
        assert_eq!(snap.len(), 10);
        assert_eq!(snap, data);
    }

    #[test]
    fn test_non_destructive_reads() {
        let buf = AudioRingBuffer::new(100, 16000, 1);
        let data: Vec<f32> = (0..30).map(|i| i as f32).collect();
        buf.write(&data);

        let snap1 = buf.snapshot_all();
        let snap2 = buf.snapshot_all();
        assert_eq!(snap1, snap2);
    }

    #[test]
    fn test_write_i16_conversion() {
        let buf = AudioRingBuffer::new(100, 16000, 1);
        let data = [i16::MAX, 0, i16::MIN + 1]; // +1 to avoid asymmetry
        buf.write_i16(&data);

        let snap = buf.snapshot_all();
        assert_eq!(snap.len(), 3);
        assert!((snap[0] - 1.0).abs() < 0.001);
        assert!((snap[1]).abs() < 0.001);
        assert!((snap[2] + 1.0).abs() < 0.001);
    }

    #[test]
    fn test_write_u16_conversion() {
        let buf = AudioRingBuffer::new(100, 16000, 1);
        let data = [u16::MAX, u16::MAX / 2, 0];
        buf.write_u16(&data);

        let snap = buf.snapshot_all();
        assert_eq!(snap.len(), 3);
        // u16::MAX -> 1.0
        assert!((snap[0] - 1.0).abs() < 0.001);
        // u16::MAX/2 -> ~0.0
        assert!(snap[1].abs() < 0.01);
        // 0 -> -1.0
        assert!((snap[2] + 1.0).abs() < 0.001);
    }

    #[test]
    fn test_concurrent_write_read() {
        use std::thread;

        let buf = Arc::new(AudioRingBuffer::new(10000, 16000, 1));
        let writer_buf = Arc::clone(&buf);

        let writer = thread::spawn(move || {
            for i in 0..1000 {
                let data: Vec<f32> = (0..10).map(|j| (i * 10 + j) as f32).collect();
                writer_buf.write(&data);
            }
        });

        // Reader: repeatedly take snapshots while writer is running
        let mut last_len = 0;
        for _ in 0..100 {
            let snap = buf.snapshot_all();
            // Available data should only grow (or stay at capacity)
            assert!(snap.len() >= last_len || snap.len() == buf.capacity());
            last_len = snap.len();
        }

        writer.join().unwrap();

        // After writer finishes, all 10000 samples should be available
        let final_snap = buf.snapshot_all();
        assert_eq!(final_snap.len(), 10000);
    }

    #[test]
    fn test_empty_write() {
        let buf = AudioRingBuffer::new(100, 16000, 1);
        buf.write(&[]);
        assert_eq!(buf.info().samples_written, 0);
    }

    #[test]
    fn test_reset() {
        let buf = AudioRingBuffer::new(100, 16000, 1);
        let data: Vec<f32> = (0..50).map(|i| i as f32).collect();
        buf.write(&data);

        assert_eq!(buf.info().samples_written, 50);
        buf.reset();
        assert_eq!(buf.info().samples_written, 0);
        assert!(buf.snapshot_all().is_empty());
    }

    #[test]
    fn test_info_accuracy() {
        let buf = AudioRingBuffer::with_duration(2.0, 16000, 1);
        let data = vec![0.0f32; 16000]; // 1 second
        buf.write(&data);

        let info = buf.info();
        assert_eq!(info.capacity_samples, 32000);
        assert_eq!(info.samples_written, 16000);
        assert_eq!(info.available_samples, 16000);
        assert!((info.capacity_seconds - 2.0).abs() < 0.001);
        assert!((info.available_seconds - 1.0).abs() < 0.001);
        assert_eq!(info.sample_rate, 16000);
        assert_eq!(info.channels, 1);
    }

    #[test]
    fn test_samples_written_starts_at_zero() {
        let buf = AudioRingBuffer::new(100, 16000, 1);
        assert_eq!(buf.samples_written(), 0);
    }

    #[test]
    fn test_samples_written_increments() {
        let buf = AudioRingBuffer::new(100, 16000, 1);
        let data: Vec<f32> = (0..25).map(|i| i as f32).collect();
        buf.write(&data);
        assert_eq!(buf.samples_written(), 25);

        buf.write(&data);
        assert_eq!(buf.samples_written(), 50);
    }

    #[test]
    fn test_snapshot_since_basic() {
        let buf = AudioRingBuffer::new(100, 16000, 1);
        let data1: Vec<f32> = (0..10).map(|i| i as f32).collect();
        buf.write(&data1);
        let pos = buf.samples_written();

        let data2: Vec<f32> = (10..20).map(|i| i as f32).collect();
        buf.write(&data2);

        let snap = buf.snapshot_since(pos);
        assert_eq!(snap.len(), 10);
        assert_eq!(snap, data2);
    }

    #[test]
    fn test_snapshot_since_with_pos_returns_exact_write_pos() {
        let buf = AudioRingBuffer::new(100, 16000, 1);
        let data1: Vec<f32> = (0..10).map(|i| i as f32).collect();
        buf.write(&data1);
        let pos = buf.samples_written();

        let data2: Vec<f32> = (10..20).map(|i| i as f32).collect();
        buf.write(&data2);

        let (snap, new_pos) = buf.snapshot_since_with_pos(pos);
        assert_eq!(snap.len(), 10);
        assert_eq!(snap, data2);
        assert_eq!(new_pos, 20);
    }

    #[test]
    fn test_snapshot_since_with_pos_no_new_data() {
        let buf = AudioRingBuffer::new(100, 16000, 1);
        let data: Vec<f32> = (0..10).map(|i| i as f32).collect();
        buf.write(&data);
        let pos = buf.samples_written();

        let (snap, new_pos) = buf.snapshot_since_with_pos(pos);
        assert!(snap.is_empty());
        assert_eq!(new_pos, pos);
    }

    #[test]
    fn test_snapshot_since_clamped_to_capacity() {
        let buf = AudioRingBuffer::new(10, 16000, 1);
        // Write 20 samples — more than capacity since pos 0
        let data: Vec<f32> = (0..20).map(|i| i as f32).collect();
        buf.write(&data);

        // Ask for all since 0 (20 samples) but capacity is 10
        let snap = buf.snapshot_since(0);
        assert_eq!(snap.len(), 10);
        // Should be the last 10: 10..20
        assert_eq!(snap[0], 10.0);
        assert_eq!(snap[9], 19.0);
    }

    #[test]
    fn test_snapshot_range_stops_at_bound_even_when_newer_samples_exist() {
        let buf = AudioRingBuffer::new(100, 16000, 1);
        let data: Vec<f32> = (0..30).map(|i| i as f32).collect();
        buf.write(&data);

        let snap = buf.snapshot_range(10, 20);

        assert_eq!(snap.start_pos, 10);
        assert_eq!(snap.end_pos, 20);
        assert!(!snap.overrun);
        assert_eq!(snap.samples, (10..20).map(|i| i as f32).collect::<Vec<_>>());
    }

    #[test]
    fn test_snapshot_range_reports_overrun() {
        let buf = AudioRingBuffer::new(10, 16000, 1);
        let data: Vec<f32> = (0..25).map(|i| i as f32).collect();
        buf.write(&data);

        let snap = buf.snapshot_range(0, 20);

        assert_eq!(snap.start_pos, 15);
        assert_eq!(snap.end_pos, 20);
        assert!(snap.overrun);
        assert_eq!(snap.samples, (15..20).map(|i| i as f32).collect::<Vec<_>>());
    }

    #[test]
    fn test_rms_energy_since_basic() {
        let buf = AudioRingBuffer::new(100, 16000, 1);
        let pos = buf.samples_written();
        // Constant 0.5 signal — RMS should be 0.5
        let signal: Vec<f32> = vec![0.5; 50];
        buf.write(&signal);
        let rms = buf.rms_energy_since(pos, 100);
        assert!(rms.is_some());
        assert!((rms.unwrap() - 0.5).abs() < 0.001);
    }

    #[test]
    fn test_rms_energy_since_empty() {
        let buf = AudioRingBuffer::new(100, 16000, 1);
        // No data written
        assert!(buf.rms_energy_since(0, 100).is_none());
        // Write data, then ask since current pos
        buf.write(&[0.1, 0.2]);
        let pos = buf.samples_written();
        assert!(buf.rms_energy_since(pos, 100).is_none());
    }

    #[test]
    fn test_rms_energy_since_clamped() {
        let buf = AudioRingBuffer::new(100, 16000, 1);
        let signal: Vec<f32> = vec![0.5; 50];
        buf.write(&signal);
        // max_samples = 10, should only read last 10 samples
        let rms = buf.rms_energy_since(0, 10);
        assert!(rms.is_some());
        assert!((rms.unwrap() - 0.5).abs() < 0.001);
    }

    #[test]
    fn test_rms_energy_since_wraparound() {
        let buf = AudioRingBuffer::new(10, 16000, 1);
        // Write 8 samples, then 5 more to wrap
        buf.write(&[0.0f32; 8]);
        let pos = buf.samples_written();
        let signal: Vec<f32> = vec![0.3; 5];
        buf.write(&signal);
        let rms = buf.rms_energy_since(pos, 100);
        assert!(rms.is_some());
        assert!((rms.unwrap() - 0.3).abs() < 0.001);
    }

    #[test]
    fn test_large_write_exceeding_capacity() {
        let buf = AudioRingBuffer::new(10, 16000, 1);
        // Write 25 samples in one call (2.5x capacity)
        let data: Vec<f32> = (0..25).map(|i| i as f32).collect();
        buf.write(&data);

        let snap = buf.snapshot_all();
        assert_eq!(snap.len(), 10);
        // Should contain the last 10 samples: 15..25
        assert_eq!(
            snap,
            vec![15.0, 16.0, 17.0, 18.0, 19.0, 20.0, 21.0, 22.0, 23.0, 24.0]
        );
    }

    // --- clamp_backfill_to_available -----------------------------------------

    /// Regression guard for the Full/Max backfill tail-loss bug. When the
    /// buffer holds the full requested rewind, the FULL request must come back
    /// with no proportional shave. A 300s request against 300s of available
    /// audio used to return ~285s (5% margin); it must now return exactly 300s.
    #[test]
    fn test_clamp_backfill_requested_within_available_returns_requested() {
        // 300s requested, 300s available, full buffer -> full 300s, no drop.
        assert_eq!(clamp_backfill_to_available(300.0, 300.0), 300.0);
        // Comfortably inside the buffer -> unchanged.
        assert_eq!(clamp_backfill_to_available(120.0, 300.0), 120.0);
        assert_eq!(clamp_backfill_to_available(5.0, 300.0), 5.0);
    }

    /// Boundary: requested == available must yield exactly available with NO
    /// 5% (or any percentage) shave.
    #[test]
    fn test_clamp_backfill_at_boundary_no_percentage_shave() {
        assert_eq!(clamp_backfill_to_available(300.0, 300.0), 300.0);
        assert_eq!(clamp_backfill_to_available(42.0, 42.0), 42.0);
    }

    /// When requesting MORE than available, the result is clamped to available
    /// minus the exact fixed write-head guard — and never exceeds available.
    #[test]
    fn test_clamp_backfill_over_available_uses_exact_fixed_guard() {
        let effective = clamp_backfill_to_available(500.0, 300.0);
        assert!((effective - (300.0 - BACKFILL_WRITE_HEAD_GUARD_SECONDS)).abs() < 1e-3);
        assert!(effective < 300.0);
        // The guard is a fixed constant, NOT a fraction of available: the gap
        // below `available` must be the same tiny constant regardless of how
        // large the buffer is. (Compared with an epsilon because f32
        // subtraction of large magnitudes is not bit-exact.)
        let small = clamp_backfill_to_available(50.0, 10.0);
        assert!((10.0 - small - BACKFILL_WRITE_HEAD_GUARD_SECONDS).abs() < 1e-3);
        let large = clamp_backfill_to_available(5000.0, 1000.0);
        assert!((1000.0 - large - BACKFILL_WRITE_HEAD_GUARD_SECONDS).abs() < 1e-3);
    }

    /// Zero / negative inputs collapse to 0.0 (no backfill).
    #[test]
    fn test_clamp_backfill_zero_inputs() {
        assert_eq!(clamp_backfill_to_available(0.0, 300.0), 0.0);
        assert_eq!(clamp_backfill_to_available(120.0, 0.0), 0.0);
        assert_eq!(clamp_backfill_to_available(-5.0, 300.0), 0.0);
        // Available smaller than the guard never underflows below 0.
        assert_eq!(clamp_backfill_to_available(10.0, 0.01), 0.0);
    }

    // --- backfill extraction path (clamp + rewind + snapshot) ----------------

    /// Mirrors the real caller's rewind: rewind `write_pos` by
    /// `effective_seconds` worth of samples, frame-aligned to `channels`, then
    /// pull `cursor..write_pos` via the actual snapshot/extract API
    /// (`snapshot_since_with_pos`). Returns `(extracted_samples, cursor)`.
    ///
    /// This is the same arithmetic `build_initial_sources_and_backfill` uses
    /// (`raw = seconds * sr * ch`, rounded down to a frame boundary, then
    /// `write_pos.saturating_sub(rewind)`) followed by `extract_source_audio`'s
    /// `snapshot_since_with_pos`. It exists so the extraction path — not just
    /// the clamp arithmetic — is exercised end to end.
    fn rewind_and_extract(buf: &AudioRingBuffer, effective_seconds: f32) -> (Vec<f32>, usize) {
        let write_pos = buf.samples_written();
        let raw = (effective_seconds * buf.sample_rate() as f32 * buf.channels() as f32) as usize;
        let ch = buf.channels() as usize;
        let rewind = raw - (raw % ch);
        let cursor = write_pos.saturating_sub(rewind);
        let (snap, _new_pos) = buf.snapshot_since_with_pos(cursor);
        (snap, cursor)
    }

    /// Integration regression for the Full/Max backfill tail-loss bug, walking
    /// the REAL extraction path (clamp helper -> rewind cursor -> snapshot),
    /// which the clamp-arithmetic unit tests do not exercise.
    ///
    /// The original bug clamped the rewind to `available * 0.95`, which dropped
    /// the OLDEST ~5% of samples. Here we fill a buffer that holds a full
    /// 300s-equivalent window, request the full window, compute the effective
    /// backfill via `clamp_backfill_to_available`, then extract. The extracted
    /// region must cover the ENTIRE requested window (no tail loss) and must
    /// begin at the oldest committed sample (the tail was not dropped).
    #[test]
    fn test_backfill_extraction_full_window_keeps_oldest_samples() {
        // Small but representative: a 100 Hz mono "sample rate" so 300 seconds
        // is 30_000 samples — exercises the real capacity/rewind math without
        // allocating millions of atomics. Each sample value is its monotonic
        // index, so the oldest sample is uniquely identifiable as `0.0`.
        let sample_rate = 100;
        let channels = 1;
        let window_seconds = 300.0_f32;
        let total = (window_seconds * sample_rate as f32 * channels as f32) as usize; // 30_000

        // Capacity is exactly the window: the buffer holds the full request.
        let buf = AudioRingBuffer::new(total, sample_rate, channels);
        let data: Vec<f32> = (0..total).map(|i| i as f32).collect();
        buf.write(&data);

        let available = buf.info().available_seconds;
        assert!((available - window_seconds).abs() < 1e-3);

        // Requesting exactly the full window: the clamp must honour it with no
        // proportional shave (regression guard) — effective == requested.
        let effective = clamp_backfill_to_available(window_seconds, available);
        assert_eq!(effective, window_seconds);

        // Walk the real extraction path with that effective backfill.
        let (extracted, cursor) = rewind_and_extract(&buf, effective);

        // No tail loss: the extracted region covers the entire window. The old
        // `available * 0.95` clamp would have returned ~28_500 samples here.
        assert_eq!(
            extracted.len(),
            total,
            "extraction must yield the full requested window, not a shaved subset"
        );

        // The OLDEST sample (index 0) is the one the original bug truncated.
        assert_eq!(cursor, 0, "rewind must reach the oldest committed sample");
        assert_eq!(
            extracted.first().copied(),
            Some(0.0),
            "the oldest (first) requested sample must be present — tail not dropped"
        );
        assert_eq!(
            extracted.last().copied(),
            Some((total - 1) as f32),
            "the newest requested sample must still be present"
        );
    }

    /// Companion to the full-window case: when the caller over-requests (asks
    /// for MORE than the buffer holds), the clamp steps back by exactly the
    /// fixed write-head guard, and the extraction path drops ONLY that tiny
    /// guard's worth of the oldest samples — never a percentage of the window.
    #[test]
    fn test_backfill_extraction_over_request_drops_only_fixed_guard() {
        let sample_rate = 100;
        let channels = 1;
        let window_seconds = 300.0_f32;
        let total = (window_seconds * sample_rate as f32 * channels as f32) as usize;

        let buf = AudioRingBuffer::new(total, sample_rate, channels);
        let data: Vec<f32> = (0..total).map(|i| i as f32).collect();
        buf.write(&data);

        let available = buf.info().available_seconds;

        // Over-request: ask for far more than the buffer holds.
        let effective = clamp_backfill_to_available(10_000.0, available);
        assert!((effective - (available - BACKFILL_WRITE_HEAD_GUARD_SECONDS)).abs() < 1e-3);

        let (extracted, cursor) = rewind_and_extract(&buf, effective);

        // Only the fixed guard's worth of oldest samples is skipped — 0.05s at
        // 100 Hz mono == 5 samples. A percentage shave would skip ~1_500.
        let guard_samples = (BACKFILL_WRITE_HEAD_GUARD_SECONDS * sample_rate as f32) as usize;
        assert_eq!(cursor, guard_samples, "only the fixed guard is skipped");
        assert_eq!(extracted.len(), total - guard_samples);
        assert_eq!(
            extracted.first().copied(),
            Some(guard_samples as f32),
            "extraction starts exactly one fixed guard past the oldest sample"
        );
    }
}
