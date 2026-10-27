use super::*;

/// Mark the calling thread as the realtime audio thread (`soundcraft_playback`'s cpal callbacks
/// do, every call: one thread-local store). Strips [`MixEngine::render`] hands to worker threads
/// count as the audio thread while they render for it. Code that might run there asks
/// [`on_audio_thread`] before doing anything that blocks; the desktop app's logger does, so a
/// `log::` record from the audio thread never takes a lock or touches a file.
pub fn mark_audio_thread() {
    AUDIO_THREAD.set(true);
}

/// True on the thread [`mark_audio_thread`] marked, and on a worker while it renders strips for
/// it. Lock-free and allocation-free.
pub fn on_audio_thread() -> bool {
    AUDIO_THREAD.get()
}

/// Run `f` on this thread as (or as not) the audio thread, then restore the thread's own mark
/// (also when `f` unwinds): a pool worker renders for realtime playback and offline bounces alike.
#[cfg(not(target_arch = "wasm32"))]
fn as_audio_thread(audio: bool, f: impl FnOnce()) {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            AUDIO_THREAD.set(self.0);
        }
    }
    let _restore = Restore(AUDIO_THREAD.replace(audio));
    f();
}
/// Whether strips are rendered on one thread even when there are enough of them to share out
/// (the Playback Engine's and Preferences' "Mix tracks on several cores" turned off).
static SERIAL_STRIPS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Renders strips on one thread (`true`) or shares them out to the worker pool (`false`, the
/// default). Set from the audio settings; takes effect on the next block.
pub fn set_serial_strips(serial: bool) {
    SERIAL_STRIPS.store(serial, std::sync::atomic::Ordering::Relaxed);
}

/// What each mix worker runs as it starts (see [`set_worker_start`]).
#[cfg(not(target_arch = "wasm32"))]
static WORKER_START: std::sync::OnceLock<fn()> = std::sync::OnceLock::new();

/// The mix's own worker pool: the audio thread waits on these workers every block, so they are
/// not shared with the rest of the app (waveforms, scans and the like on rayon's global pool
/// would hold up a block) and run as the host's realtime threads.
#[cfg(not(target_arch = "wasm32"))]
static POOL: std::sync::OnceLock<Option<rayon::ThreadPool>> = std::sync::OnceLock::new();

/// Sets what each mix worker thread runs as it starts: the host's realtime scheduling (MMCSS
/// "Pro Audio" on Windows). Call before [`warm_up_workers`]; later calls are ignored.
#[cfg(not(target_arch = "wasm32"))]
pub fn set_worker_start(f: fn()) {
    let _ = WORKER_START.set(f);
}

/// No worker pool on this target.
#[cfg(target_arch = "wasm32")]
pub fn set_worker_start(_f: fn()) {}

#[cfg(not(target_arch = "wasm32"))]
fn pool() -> Option<&'static rayon::ThreadPool> {
    POOL.get_or_init(|| {
        let start = WORKER_START.get().copied();
        rayon::ThreadPoolBuilder::new()
            .thread_name(|i| format!("soundcraft-mix-{i}"))
            .start_handler(move |_| {
                if let Some(f) = start {
                    f();
                }
            })
            .build()
            .map_err(|e| log::warn!("mix worker pool: {e}; tracks are mixed on one core"))
            .ok()
    })
    .as_ref()
}

/// Process strips, in parallel on native targets when there are enough of them.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn run_strips<T: Send>(work: &mut [T], f: impl Fn(&mut T) + Sync + Send) {
    use rayon::prelude::*;
    match pool() {
        Some(pool) if work.len() >= 4 && !SERIAL_STRIPS.load(std::sync::atomic::Ordering::Relaxed) => {
            // The workers render on behalf of this thread: on the audio thread, they are it too.
            let audio = on_audio_thread();
            pool.install(|| work.par_iter_mut().for_each(|w| as_audio_thread(audio, || f(w))));
        }
        _ => work.iter_mut().for_each(f),
    }
}
/// Starts the worker pool [`MixEngine::render`] shares strips out to. It would otherwise start on
/// first use, the first block of the first playback, on the audio thread: a thread per core
/// spawned inside one buffer period (1 ms at 48 frames), a dropout. Call it from a non-realtime
/// thread before audio starts; later calls are free.
#[cfg(not(target_arch = "wasm32"))]
pub fn warm_up_workers() {
    let _ = pool();
}

/// No worker pool on this target.
#[cfg(target_arch = "wasm32")]
pub fn warm_up_workers() {}

#[cfg(target_arch = "wasm32")]
pub(crate) fn run_strips<T: Send>(work: &mut [T], f: impl Fn(&mut T) + Sync + Send) {
    work.iter_mut().for_each(f);
}

#[cfg(test)]
mod audio_thread_tests {
    use super::*;

    #[test]
    fn only_a_marked_thread_is_the_audio_thread() {
        assert!(!on_audio_thread());
        let marked = std::thread::spawn(|| {
            let before = on_audio_thread();
            mark_audio_thread();
            (before, on_audio_thread())
        })
        .join()
        .unwrap();
        assert_eq!(marked, (false, true));
        assert!(!on_audio_thread(), "marking another thread leaves this one alone");
    }

    /// What `on_audio_thread` says inside each strip job, for strips run from a thread that is
    /// (or is not) the audio thread.
    fn seen_by_strips(audio: bool) -> Vec<bool> {
        std::thread::spawn(move || {
            if audio {
                mark_audio_thread();
            }
            let mut work = vec![None; 64];
            run_strips(&mut work, |w| {
                // Long enough that the pool's workers take part.
                std::thread::sleep(std::time::Duration::from_millis(1));
                *w = Some(on_audio_thread());
            });
            work.into_iter().map(|w| w.unwrap()).collect()
        })
        .join()
        .unwrap()
    }

    #[test]
    fn strips_rendered_for_the_audio_thread_count_as_the_audio_thread_and_only_then() {
        assert!(seen_by_strips(true).iter().all(|&rt| rt), "a worker rendering for the audio thread is on it");
        assert!(seen_by_strips(false).iter().all(|&rt| !rt), "the workers don't stay marked afterwards");
    }
}
