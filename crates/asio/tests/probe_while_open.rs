//! Regression check: probing other ASIO drivers must not disturb an open stream. Opens a Focusrite
//! driver with a callback that writes silence, probes every other driver, and asserts that the
//! stream kept calling back at (at least) 60% of its rate and that probes of other drivers return
//! quickly. Run by hand (`--ignored`): needs the hardware. Prints nothing when it passes.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use soundcraft_asio::{AsioStream, Callback, Error, StreamConfig};

struct Blocks(Arc<AtomicU64>);

impl Callback for Blocks {
    fn process(&mut self, _input: &[f32], output: &mut [f32], _frames: usize) {
        output.fill(0.0);
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

/// Blocks a stream at `rate` Hz with `frames`-frame buffers should deliver in `elapsed`.
fn expected_blocks(elapsed: Duration, rate: u32, frames: u32) -> f64 {
    elapsed.as_secs_f64() * f64::from(rate) / f64::from(frames.max(1))
}

#[test]
#[ignore]
fn probing_other_drivers_while_a_stream_runs() {
    let drivers = soundcraft_asio::drivers();
    let Some(open) = drivers.iter().find(|d| d.contains("Focusrite USB")).cloned() else {
        return;
    };
    // Probe the others once while nothing is open, so their caps are cached for the probes below.
    for other in drivers.iter().filter(|d| **d != open) {
        let _ = soundcraft_asio::probe(other);
    }
    let blocks = Arc::new(AtomicU64::new(0));
    let cfg = StreamConfig { driver: open.clone(), sample_rate: Some(48_000), buffer_frames: Some(64), max_outputs: 2, max_inputs: 64 };
    let stream = AsioStream::open(&cfg, Box::new(Blocks(Arc::clone(&blocks)))).unwrap_or_else(|e| panic!("open {open}: {e}"));
    let (rate, frames) = (stream.sample_rate(), stream.buffer_frames());

    let start = Instant::now();
    std::thread::sleep(Duration::from_secs(1));
    let baseline = blocks.load(Ordering::Relaxed);
    let want = expected_blocks(start.elapsed(), rate, frames);
    assert!(baseline as f64 >= 0.6 * want, "baseline: {baseline} blocks in 1 s, expected about {want:.0}");

    for other in drivers.iter().filter(|d| **d != open) {
        let before = blocks.load(Ordering::Relaxed);
        let t = Instant::now();
        let result = soundcraft_asio::probe(other);
        let took = t.elapsed();
        assert!(took < Duration::from_millis(50), "probe {other} took {took:?} while {open} runs");
        assert!(matches!(result, Ok(_) | Err(Error::Busy(_))), "probe {other} while {open} runs: {result:?}");
        std::thread::sleep(Duration::from_millis(500));
        let window = t.elapsed();
        let got = blocks.load(Ordering::Relaxed) - before;
        let want = expected_blocks(window, rate, frames);
        assert!(got as f64 >= 0.6 * want, "after probing {other}: {got} blocks in {window:?}, expected about {want:.0}");
    }
    drop(stream);
}
