//! Hardware smoke test: needs a real ASIO driver, so it is ignored by default.
//!
//! Run with: `cargo test -p soundcraft-asio --test hardware -- --ignored --nocapture`

use soundcraft_asio::{AsioStream, Callback, StreamConfig, drivers, probe};
use std::f64::consts::PI;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::time::Duration;

/// A quiet 440 Hz sine on every opened output channel.
struct Sine {
    phase: f64,
    rate: Arc<AtomicU32>,
    blocks: Arc<AtomicUsize>,
}

impl Callback for Sine {
    fn process(&mut self, _input: &[f32], output: &mut [f32], frames: usize) {
        let channels = output.len().checked_div(frames).unwrap_or(0);
        let rate = f64::from(self.rate.load(Ordering::Relaxed).max(1));
        let step = 2.0 * PI * 440.0 / rate;
        for f in 0..frames {
            let v = (self.phase.sin() * 0.05) as f32;
            for c in 0..channels {
                if let Some(s) = output.get_mut(f * channels + c) {
                    *s = v;
                }
            }
            self.phase = (self.phase + step) % (2.0 * PI);
        }
        self.blocks.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
#[ignore]
fn hardware_smoke() {
    let names = drivers();
    println!("drivers: {names:?}");
    for name in &names {
        match probe(name) {
            Ok(caps) => println!(
                "{name}: inputs {:?} outputs {:?} rates {:?} current {} Hz buffer {}..={} preferred {} granularity {}",
                caps.inputs, caps.outputs, caps.rates, caps.current_rate, caps.buffer_min, caps.buffer_max, caps.buffer_preferred, caps.granularity
            ),
            Err(e) => println!("{name}: probe failed: {e}"),
        }
    }

    // Try each Focusrite driver in order; a driver whose hardware is unplugged refuses to open.
    let rate = Arc::new(AtomicU32::new(48_000));
    let blocks = Arc::new(AtomicUsize::new(0));
    let mut opened = None;
    for name in names.iter().filter(|n| n.contains("Focusrite")) {
        println!("opening {name}");
        let cfg = StreamConfig { driver: name.clone(), sample_rate: None, buffer_frames: None, max_outputs: 2, max_inputs: 0 };
        let sine = Sine { phase: 0.0, rate: Arc::clone(&rate), blocks: Arc::clone(&blocks) };
        match AsioStream::open(&cfg, Box::new(sine)) {
            Ok(stream) => {
                opened = Some(stream);
                break;
            }
            Err(e) => println!("{name}: open failed: {e}"),
        }
    }
    let Some(stream) = opened else {
        println!("no Focusrite driver opened, nothing more to test");
        return;
    };
    rate.store(stream.sample_rate(), Ordering::Relaxed);
    println!(
        "open: driver {} rate {} Hz buffer {} frames in {} out {} latency {:?} reset {}",
        stream.driver(),
        stream.sample_rate(),
        stream.buffer_frames(),
        stream.input_channels(),
        stream.output_channels(),
        stream.latency(),
        stream.reset_requested()
    );
    std::thread::sleep(Duration::from_secs(1));
    let processed = blocks.load(Ordering::Relaxed);
    println!("callback blocks in 1 s: {processed}");
    drop(stream);
    println!("stream dropped");
    assert!(processed > 0, "the driver never called the callback");
}
