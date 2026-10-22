//! The FFI boundary: the only module in this crate allowed to use `unsafe`.
//!
//! It reads the driver registry, creates the driver's COM object on a thread of its own (the
//! driver's apartment), drives the IASIO vtable, and implements the host callbacks. What it
//! exposes to the rest of the crate is a safe API returning `Result`.
//!
//! Soundness rests on the ASIO contract (the driver honours the vtable signatures and the buffer
//! lifetimes). A driver that violates it can still crash the process; in-process hosting cannot
//! prevent that. What this module guarantees is that SoundCraft never calls a missing function,
//! never hands the driver a mis-sized buffer, never lets a panic cross `extern "C"`, and never
//! frees state a callback can still reach.

use crate::Error;
use crate::dsp;
use crate::stream::{Callback, DriverCaps, RATES, StreamConfig};
use std::ffi::{c_char, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::{NonNull, null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, CLSIDFromString, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows_sys::Win32::System::Registry::{HKEY, HKEY_LOCAL_MACHINE, KEY_READ, RegCloseKey, RegEnumKeyExW, RegOpenKeyExW, RegQueryValueExW};
use windows_sys::Win32::UI::WindowsAndMessaging::{DispatchMessageW, GetDesktopWindow, MSG, PM_REMOVE, PeekMessageW, TranslateMessage};
use windows_sys::core::GUID;

/// ASIO's `ASE_OK`.
const ASE_OK: i32 = 0;
/// ASIO's `ASIOBool` true.
const ASIO_TRUE: i32 = 1;
/// Largest channel count a driver may report (hostile-input cap).
const MAX_DRIVER_CHANNELS: i32 = 256;
/// Most channels per direction the host opens.
const MAX_OPEN_CHANNELS: usize = 64;
/// Largest buffer size accepted from a driver.
const MAX_FRAMES: i32 = 1 << 18;
/// Registry key holding one subkey per installed driver.
const ASIO_KEY: &str = "SOFTWARE\\ASIO";

/// The IASIO object: a pointer to its vtable, as COM lays it out.
#[repr(C)]
struct Iasio {
    vtbl: *const Vtbl,
}

/// The IASIO vtable, in the ASIO 2.3 order. Every entry takes the object pointer first.
#[repr(C)]
#[allow(dead_code)]
struct Vtbl {
    query_interface: unsafe extern "system" fn(*mut Iasio, *const GUID, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut Iasio) -> u32,
    release: unsafe extern "system" fn(*mut Iasio) -> u32,
    init: unsafe extern "system" fn(*mut Iasio, *mut c_void) -> i32,
    get_driver_name: unsafe extern "system" fn(*mut Iasio, *mut c_char),
    get_driver_version: unsafe extern "system" fn(*mut Iasio) -> i32,
    get_error_message: unsafe extern "system" fn(*mut Iasio, *mut c_char),
    start: unsafe extern "system" fn(*mut Iasio) -> i32,
    stop: unsafe extern "system" fn(*mut Iasio) -> i32,
    get_channels: unsafe extern "system" fn(*mut Iasio, *mut i32, *mut i32) -> i32,
    get_latencies: unsafe extern "system" fn(*mut Iasio, *mut i32, *mut i32) -> i32,
    get_buffer_size: unsafe extern "system" fn(*mut Iasio, *mut i32, *mut i32, *mut i32, *mut i32) -> i32,
    can_sample_rate: unsafe extern "system" fn(*mut Iasio, f64) -> i32,
    get_sample_rate: unsafe extern "system" fn(*mut Iasio, *mut f64) -> i32,
    set_sample_rate: unsafe extern "system" fn(*mut Iasio, f64) -> i32,
    get_clock_sources: unsafe extern "system" fn(*mut Iasio, *mut c_void, *mut i32) -> i32,
    set_clock_source: unsafe extern "system" fn(*mut Iasio, i32) -> i32,
    get_sample_position: unsafe extern "system" fn(*mut Iasio, *mut c_void, *mut c_void) -> i32,
    get_channel_info: unsafe extern "system" fn(*mut Iasio, *mut ChannelInfo) -> i32,
    create_buffers: unsafe extern "system" fn(*mut Iasio, *mut BufferInfo, i32, i32, *const Callbacks) -> i32,
    dispose_buffers: unsafe extern "system" fn(*mut Iasio) -> i32,
    control_panel: unsafe extern "system" fn(*mut Iasio) -> i32,
    future: unsafe extern "system" fn(*mut Iasio, i32, *mut c_void) -> i32,
    output_ready: unsafe extern "system" fn(*mut Iasio) -> i32,
}

#[repr(C)]
#[allow(dead_code)]
struct ChannelInfo {
    channel: i32,
    is_input: i32,
    is_active: i32,
    group: i32,
    sample_type: i32,
    name: [c_char; 32],
}

impl ChannelInfo {
    fn label(&self) -> String {
        text(&self.name)
    }
}

#[repr(C)]
#[allow(dead_code)]
struct BufferInfo {
    is_input: i32,
    channel: i32,
    buffers: [*mut c_void; 2],
}

/// The host callbacks the driver calls. Only function pointers, so a `static` is enough.
#[repr(C)]
#[allow(dead_code)]
struct Callbacks {
    buffer_switch: extern "C" fn(i32, i32),
    sample_rate_changed: extern "C" fn(f64),
    message: extern "C" fn(i32, i32, *mut c_void, *mut f64) -> i32,
    buffer_switch_time_info: extern "C" fn(*mut c_void, i32, i32) -> *mut c_void,
}

static CALLBACKS: Callbacks = Callbacks { buffer_switch, sample_rate_changed, message: asio_message, buffer_switch_time_info };

/// Flags shared between the audio thread and the owner (`AsioStream::reset_requested`).
#[derive(Default)]
pub(crate) struct Flags {
    reset: AtomicBool,
    failed: AtomicBool,
    /// Set while the stream is being torn down: blocks are silenced and the callback is skipped,
    /// so the driver loops silence instead of the last block.
    quiet: AtomicBool,
}

/// The state the driver's callbacks work on. Heap-allocated with `Box::into_raw`, installed in
/// [`ACTIVE`] before `start`, and freed only after the driver is stopped and its buffers disposed.
struct Context {
    /// Input channels first, then outputs; `buffers[i].buffers[index]` is the driver's half-buffer.
    buffers: Vec<BufferInfo>,
    in_types: Vec<i32>,
    out_types: Vec<i32>,
    in_ch: usize,
    out_ch: usize,
    frames: usize,
    in_scratch: Vec<f32>,
    out_scratch: Vec<f32>,
    cb: Box<dyn Callback>,
    output_ready: bool,
    driver: *mut Iasio,
    flags: Arc<Flags>,
}

/// The context the driver's callbacks use. ASIO has no user-data pointer, so there is one global.
static ACTIVE: AtomicPtr<Context> = AtomicPtr::new(null_mut());

fn type_at(types: &[i32], ch: usize) -> i32 {
    types.get(ch).copied().unwrap_or(0)
}

impl Context {
    /// The driver's half-buffer `index` of channel `vec_index` (input channels come first).
    fn channel_ptr(&self, vec_index: usize, index: usize) -> Option<*mut u8> {
        let p = self.buffers.get(vec_index)?.buffers.get(index).copied()?;
        (!p.is_null()).then(|| p.cast::<u8>())
    }

    /// One block: read the inputs, run the callback, write the outputs. Called on the audio thread.
    fn block(&mut self, index: usize) {
        let frames = self.frames;
        for ch in 0..self.in_ch {
            let t = type_at(&self.in_types, ch);
            let Some(p) = self.channel_ptr(ch, index) else { continue };
            let len = frames.saturating_mul(dsp::bytes_per_sample(t).unwrap_or(0));
            // SAFETY: the driver allocated `len` bytes for this channel's half-buffer in
            // createBuffers, and they stay valid until disposeBuffers, which runs only after this
            // context is freed. The driver calls the buffer callbacks one at a time.
            let src = unsafe { std::slice::from_raw_parts(p, len) };
            dsp::read_channel(src, t, &mut self.in_scratch, ch, self.in_ch, frames);
        }
        self.out_scratch.fill(0.0);
        self.cb.process(&self.in_scratch, &mut self.out_scratch, frames);
        for ch in 0..self.out_ch {
            let t = type_at(&self.out_types, ch);
            let Some(p) = self.channel_ptr(self.in_ch + ch, index) else { continue };
            let len = frames.saturating_mul(dsp::bytes_per_sample(t).unwrap_or(0));
            // SAFETY: as for the inputs above; the driver reads this half-buffer only after the
            // callback returns, and nothing else touches it meanwhile.
            let dst = unsafe { std::slice::from_raw_parts_mut(p, len) };
            dsp::write_channel(&self.out_scratch, ch, self.out_ch, t, dst, frames);
        }
        if self.output_ready {
            // SAFETY: `driver` is the live object owned by the owner thread. It is released only
            // after this context is freed (see `Live::drop`).
            let _ = unsafe { ((*(*self.driver).vtbl).output_ready)(self.driver) };
        }
    }

    /// Writes silence to every output half-buffer.
    fn silence(&mut self, index: usize) {
        let frames = self.frames;
        for ch in 0..self.out_ch {
            let t = type_at(&self.out_types, ch);
            let Some(p) = self.channel_ptr(self.in_ch + ch, index) else { continue };
            let len = frames.saturating_mul(dsp::bytes_per_sample(t).unwrap_or(0));
            // SAFETY: as in `block`, for an output half-buffer.
            unsafe { std::slice::from_raw_parts_mut(p, len) }.fill(0);
        }
    }
}

/// Runs one block for half-buffer `index` on the audio thread, never letting a panic escape.
fn process_block(index: i32) {
    let p = ACTIVE.load(Ordering::Acquire);
    let Ok(idx) = usize::try_from(index) else { return };
    if p.is_null() || idx > 1 {
        return;
    }
    // SAFETY: a non-null ACTIVE points at the context the owner thread installed before start().
    // It is cleared before the context is freed, and the driver serialises its buffer callbacks, so
    // this is the only live mutable reference.
    let ctx = unsafe { &mut *p };
    if ctx.flags.failed.load(Ordering::Relaxed) || ctx.flags.quiet.load(Ordering::Acquire) {
        ctx.silence(idx);
        return;
    }
    let outcome = catch_unwind(AssertUnwindSafe(|| ctx.block(idx)));
    if outcome.is_err() {
        ctx.flags.failed.store(true, Ordering::Relaxed);
        ctx.silence(idx);
    }
}

/// Runs `f` on the flags of the installed context, if any.
fn with_flags(f: impl FnOnce(&Flags)) {
    let p = ACTIVE.load(Ordering::Acquire);
    // SAFETY: as in `process_block`: a non-null ACTIVE points at a live context.
    if let Some(ctx) = unsafe { p.as_ref() } {
        f(&ctx.flags);
    }
}

extern "C" fn buffer_switch(index: i32, _direct: i32) {
    let _ = catch_unwind(AssertUnwindSafe(|| process_block(index)));
}

extern "C" fn buffer_switch_time_info(params: *mut c_void, index: i32, _direct: i32) -> *mut c_void {
    let _ = catch_unwind(AssertUnwindSafe(|| process_block(index)));
    params
}

extern "C" fn sample_rate_changed(_rate: f64) {
    let _ = catch_unwind(|| with_flags(|f| f.reset.store(true, Ordering::Release)));
}

extern "C" fn asio_message(selector: i32, value: i32, _message: *mut c_void, _opt: *mut f64) -> i32 {
    catch_unwind(|| match selector {
        // SelectorSupported: which of the selectors below we answer.
        1 => i32::from(matches!(value, 1..=6)),
        // EngineVersion.
        2 => 2,
        // ResetRequest, BufferSizeChange: the owner must reopen the stream.
        3 | 4 => {
            with_flags(|f| f.reset.store(true, Ordering::Release));
            1
        }
        // ResyncRequest, LatenciesChanged.
        5 | 6 => 1,
        _ => 0,
    })
    .unwrap_or(0)
}

/// A live driver object. Holds one COM reference, released on drop.
struct Driver(NonNull<Iasio>);

impl Driver {
    fn raw(&self) -> *mut Iasio {
        self.0.as_ptr()
    }

    fn vt(&self) -> &Vtbl {
        // SAFETY: while this `Driver` exists the object is alive (we hold a COM reference), and its
        // first field points at a vtable that lives as long as the object.
        unsafe { &*(*self.0.as_ptr()).vtbl }
    }

    /// Creates the driver object. The driver's own CLSID is used as the IID (ASIO has none).
    fn create(clsid: &GUID) -> Result<Driver, Error> {
        let mut ptr: *mut c_void = null_mut();
        // SAFETY: the caller has initialised COM on this thread. `clsid` is a valid GUID used as both
        // class and interface id, as ASIO drivers expect; `ptr` is a valid out-pointer.
        let hr = unsafe { CoCreateInstance(clsid, null_mut(), CLSCTX_INPROC_SERVER, clsid, &mut ptr) };
        if hr < 0 {
            return Err(Error::Driver(format!("CoCreateInstance failed ({hr:#x})")));
        }
        NonNull::new(ptr.cast::<Iasio>()).map(Driver).ok_or_else(|| Error::Driver("CoCreateInstance returned no object".into()))
    }

    fn init(&self) -> bool {
        let f = self.vt().init;
        // SAFETY: GetDesktopWindow takes no arguments and has no preconditions.
        let hwnd = unsafe { GetDesktopWindow() };
        // SAFETY: the object is alive; `init` takes the host window handle.
        unsafe { f(self.raw(), hwnd.cast()) == ASIO_TRUE }
    }

    fn error_message(&self) -> String {
        let f = self.vt().get_error_message;
        let mut buf: [c_char; 128] = [0; 128];
        // SAFETY: the driver writes at most 124 bytes; `buf` has 128 writable bytes.
        unsafe { f(self.raw(), buf.as_mut_ptr()) };
        text(&buf).trim_end().to_owned()
    }

    fn start(&self) -> i32 {
        let f = self.vt().start;
        // SAFETY: the object is alive; `start` takes no arguments.
        unsafe { f(self.raw()) }
    }

    fn stop(&self) -> i32 {
        let f = self.vt().stop;
        // SAFETY: the object is alive; `stop` takes no arguments.
        unsafe { f(self.raw()) }
    }

    /// `(result, input channels, output channels)`.
    fn channels(&self) -> (i32, i32, i32) {
        let f = self.vt().get_channels;
        let (mut inputs, mut outputs) = (0i32, 0i32);
        // SAFETY: both out-pointers refer to live `i32`s.
        let rc = unsafe { f(self.raw(), &mut inputs, &mut outputs) };
        (rc, inputs, outputs)
    }

    /// `(result, input latency, output latency)` in frames.
    fn latencies(&self) -> (i32, i32, i32) {
        let f = self.vt().get_latencies;
        let (mut inputs, mut outputs) = (0i32, 0i32);
        // SAFETY: both out-pointers refer to live `i32`s.
        let rc = unsafe { f(self.raw(), &mut inputs, &mut outputs) };
        (rc, inputs, outputs)
    }

    /// `(result, min, max, preferred, granularity)`.
    fn buffer_size(&self) -> (i32, i32, i32, i32, i32) {
        let f = self.vt().get_buffer_size;
        let (mut min, mut max, mut preferred, mut granularity) = (0i32, 0i32, 0i32, 0i32);
        // SAFETY: all four out-pointers refer to live `i32`s.
        let rc = unsafe { f(self.raw(), &mut min, &mut max, &mut preferred, &mut granularity) };
        (rc, min, max, preferred, granularity)
    }

    fn can_sample_rate(&self, rate: f64) -> i32 {
        let f = self.vt().can_sample_rate;
        // SAFETY: the object is alive; the rate is passed by value.
        unsafe { f(self.raw(), rate) }
    }

    /// The current sample rate, if the driver reports a usable one.
    fn sample_rate(&self) -> Option<u32> {
        let f = self.vt().get_sample_rate;
        let mut rate = 0.0f64;
        // SAFETY: `rate` is a live f64 out-pointer.
        let rc = unsafe { f(self.raw(), &mut rate) };
        (rc == ASE_OK && (1.0..=4.0e9).contains(&rate)).then(|| rate.round() as u32)
    }

    fn set_sample_rate(&self, rate: f64) -> i32 {
        let f = self.vt().set_sample_rate;
        // SAFETY: the object is alive; the rate is passed by value.
        unsafe { f(self.raw(), rate) }
    }

    fn channel_info(&self, input: bool, ch: usize) -> Result<ChannelInfo, Error> {
        let mut info = ChannelInfo {
            channel: i32::try_from(ch).unwrap_or(0),
            is_input: i32::from(input),
            is_active: 0,
            group: 0,
            sample_type: 0,
            name: [0; 32],
        };
        let f = self.vt().get_channel_info;
        // SAFETY: `info` is a live, correctly laid-out ChannelInfo; the driver fills it in.
        let rc = unsafe { f(self.raw(), &mut info) };
        check(rc, "getChannelInfo")?;
        Ok(info)
    }

    fn create_buffers(&self, infos: *mut BufferInfo, count: i32, frames: i32) -> i32 {
        let f = self.vt().create_buffers;
        // SAFETY: `infos` points at `count` BufferInfo entries that outlive the call; `CALLBACKS` is
        // a static, so the driver may keep the pointer for the life of the buffers.
        unsafe { f(self.raw(), infos, count, frames, &CALLBACKS) }
    }

    fn dispose_buffers(&self) -> i32 {
        let f = self.vt().dispose_buffers;
        // SAFETY: the object is alive; buffers were created by `create_buffers`.
        unsafe { f(self.raw()) }
    }

    fn control_panel(&self) -> i32 {
        let f = self.vt().control_panel;
        // SAFETY: the object is alive; `controlPanel` takes no arguments.
        unsafe { f(self.raw()) }
    }

    fn output_ready(&self) -> i32 {
        let f = self.vt().output_ready;
        // SAFETY: the object is alive; `outputReady` takes no arguments.
        unsafe { f(self.raw()) }
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        let f = self.vt().release;
        // SAFETY: this is the reference we hold; it is released exactly once, here.
        let _ = unsafe { f(self.raw()) };
    }
}

/// Everything one driver load owns on its thread: COM apartment, object, and (for an open stream)
/// the buffers and context. Dropping it tears the stream down in the required order.
struct Live {
    co: bool,
    drv: Option<Driver>,
    ctx: *mut Context,
    created: bool,
    started: bool,
    /// Two buffer periods, in ms: how long to keep silence flowing before `stop()`.
    quiet_ms: u64,
}

/// Two buffer periods in milliseconds, clamped to 5..=100 ms.
fn quiet_period_ms(rate: u32, frames: u32) -> u64 {
    let ms = u64::from(frames).saturating_mul(2).saturating_mul(1000) / u64::from(rate.max(1));
    ms.clamp(5, 100)
}

impl Live {
    fn empty() -> Live {
        Live { co: false, drv: None, ctx: null_mut(), created: false, started: false, quiet_ms: 5 }
    }

    /// Initialises COM on this thread, creates the driver and calls its `init`.
    fn load(clsid: &GUID) -> Result<Live, Error> {
        let mut live = Live::empty();
        // SAFETY: the reserved argument is null as COM requires. This thread is ours, and the
        // apartment is undone in `Drop`.
        let hr = unsafe { CoInitializeEx(null(), COINIT_APARTMENTTHREADED as u32) };
        if hr < 0 {
            return Err(Error::Driver(format!("CoInitializeEx failed ({hr:#x})")));
        }
        live.co = true;
        let drv = Driver::create(clsid)?;
        if !drv.init() {
            return Err(Error::Driver(drv.error_message()));
        }
        live.drv = Some(drv);
        Ok(live)
    }

    fn driver(&self) -> Result<&Driver, Error> {
        self.drv.as_ref().ok_or_else(|| Error::Driver("driver not loaded".into()))
    }

    fn caps(&self, name: &str) -> Result<DriverCaps, Error> {
        read_layout(self.driver()?, name).map(|layout| layout.caps)
    }

    fn control_panel(&self) -> Result<(), Error> {
        check(self.driver()?.control_panel(), "controlPanel")
    }

    /// The open sequence: layout, sample rate, buffers, types, latency, context, start.
    fn open_stream(&mut self, job: Job) -> Result<Info, Error> {
        let Some(drv) = self.drv.as_ref() else {
            return Err(Error::Driver("driver not loaded".into()));
        };
        let layout = read_layout(drv, &job.name)?;
        if let Some(rate) = job.rate {
            let r = f64::from(rate);
            if drv.can_sample_rate(r) == ASE_OK {
                check(drv.set_sample_rate(r), "setSampleRate")?;
            }
        }
        let sample_rate = drv.sample_rate().ok_or_else(|| Error::Driver("getSampleRate gave no usable rate".into()))?;
        let chosen = dsp::choose_buffer(job.buffer, layout.min, layout.max, layout.preferred, layout.granularity);
        let frames = i32::try_from(chosen)
            .ok()
            .filter(|f| (1..=MAX_FRAMES).contains(f))
            .ok_or_else(|| Error::Driver(format!("unusable buffer size {chosen}")))?;

        let open_in = layout.in_ch.min(job.max_inputs).min(MAX_OPEN_CHANNELS);
        let open_out = layout.out_ch.min(job.max_outputs).min(MAX_OPEN_CHANNELS);
        let mut buffers: Vec<BufferInfo> = Vec::with_capacity(open_in + open_out);
        for ch in 0..open_in {
            buffers.push(BufferInfo { is_input: ASIO_TRUE, channel: i32::try_from(ch).unwrap_or(0), buffers: [null_mut(); 2] });
        }
        for ch in 0..open_out {
            buffers.push(BufferInfo { is_input: 0, channel: i32::try_from(ch).unwrap_or(0), buffers: [null_mut(); 2] });
        }
        let count = i32::try_from(buffers.len()).unwrap_or(i32::MAX);
        check(drv.create_buffers(buffers.as_mut_ptr(), count, frames), "createBuffers")?;
        self.created = true;

        // The sample types are only meaningful once the buffers exist.
        let mut in_types = Vec::with_capacity(open_in);
        for ch in 0..open_in {
            in_types.push(drv.channel_info(true, ch)?.sample_type);
        }
        let mut out_types = Vec::with_capacity(open_out);
        for ch in 0..open_out {
            out_types.push(drv.channel_info(false, ch)?.sample_type);
        }
        let (rc, lat_in, lat_out) = drv.latencies();
        check(rc, "getLatencies")?;
        let output_ready = drv.output_ready() == ASE_OK;

        let flags = Arc::new(Flags::default());
        let frames_len = usize::try_from(frames).unwrap_or(0);
        let ctx = Box::new(Context {
            buffers,
            in_types,
            out_types,
            in_ch: open_in,
            out_ch: open_out,
            frames: frames_len,
            in_scratch: vec![0.0; frames_len.saturating_mul(open_in)],
            out_scratch: vec![0.0; frames_len.saturating_mul(open_out)],
            cb: job.cb,
            output_ready,
            driver: drv.raw(),
            flags: Arc::clone(&flags),
        });
        let raw = Box::into_raw(ctx);
        self.ctx = raw;
        ACTIVE.store(raw, Ordering::Release);
        check(drv.start(), "start")?;
        self.started = true;
        self.quiet_ms = quiet_period_ms(sample_rate, u32::try_from(frames).unwrap_or(0));

        log::info!("asio: {} open at {sample_rate} Hz, {frames} frames, {open_in} in / {open_out} out, latency {lat_in}/{lat_out}", job.name);
        Ok(Info {
            sample_rate,
            buffer_frames: u32::try_from(frames).unwrap_or(0),
            input_channels: open_in,
            output_channels: open_out,
            latency: (unsigned(lat_in), unsigned(lat_out)),
            caps: layout.caps,
            flags,
        })
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        if self.started
            && let Some(d) = self.drv.as_ref()
        {
            // Drivers loop the last half-buffer they were given, so keep silence flowing for two
            // buffer periods before stopping. The callback is skipped while quiet.
            with_flags(|f| f.quiet.store(true, Ordering::Release));
            thread::sleep(Duration::from_millis(self.quiet_ms));
            let _ = d.stop();
        }
        self.started = false;
        ACTIVE.store(null_mut(), Ordering::Release);
        if self.created
            && let Some(d) = self.drv.as_ref()
        {
            let _ = d.dispose_buffers();
        }
        self.created = false;
        if !self.ctx.is_null() {
            // SAFETY: `ctx` came from `Box::into_raw` in `open_stream` and is freed exactly once,
            // here. The driver is stopped, ACTIVE is cleared and the buffers are disposed, so no
            // callback can still reach it.
            drop(unsafe { Box::from_raw(self.ctx) });
            self.ctx = null_mut();
        }
        // Release the driver object before the apartment goes away.
        self.drv = None;
        if self.co {
            // SAFETY: balances the successful CoInitializeEx on this thread, after the object was
            // released above.
            unsafe { CoUninitialize() };
            self.co = false;
        }
    }
}

/// What the driver reports about its channels, buffer sizes and rates.
struct Layout {
    caps: DriverCaps,
    in_ch: usize,
    out_ch: usize,
    min: u32,
    max: u32,
    preferred: u32,
    granularity: i32,
}

fn read_layout(drv: &Driver, name: &str) -> Result<Layout, Error> {
    let (rc, in_n, out_n) = drv.channels();
    check(rc, "getChannels")?;
    let in_ch = channel_count(in_n);
    let out_ch = channel_count(out_n);
    let (rc, min, max, preferred, granularity) = drv.buffer_size();
    check(rc, "getBufferSize")?;
    let mut inputs = Vec::with_capacity(in_ch);
    for ch in 0..in_ch {
        inputs.push(drv.channel_info(true, ch)?.label());
    }
    let mut outputs = Vec::with_capacity(out_ch);
    for ch in 0..out_ch {
        outputs.push(drv.channel_info(false, ch)?.label());
    }
    let rates: Vec<u32> = RATES.iter().copied().filter(|&r| drv.can_sample_rate(f64::from(r)) == ASE_OK).collect();
    let current_rate = drv.sample_rate().unwrap_or(0);
    let (min, max, preferred) = (unsigned(min), unsigned(max), unsigned(preferred));
    Ok(Layout {
        caps: DriverCaps {
            name: name.to_owned(),
            inputs,
            outputs,
            rates,
            current_rate,
            buffer_min: min,
            buffer_max: max,
            buffer_preferred: preferred,
            granularity,
        },
        in_ch,
        out_ch,
        min,
        max,
        preferred,
        granularity,
    })
}

fn check(code: i32, what: &str) -> Result<(), Error> {
    if code == ASE_OK { Ok(()) } else { Err(Error::Driver(format!("{what} failed ({code})"))) }
}

fn channel_count(n: i32) -> usize {
    usize::try_from(n.clamp(0, MAX_DRIVER_CHANNELS)).unwrap_or(0)
}

fn unsigned(v: i32) -> u32 {
    u32::try_from(v).unwrap_or(0)
}

/// NUL-terminated C text up to the first NUL, lossily decoded.
fn text(bytes: &[c_char]) -> String {
    let v: Vec<u8> = bytes.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    String::from_utf8_lossy(&v).into_owned()
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// An open registry key, closed on drop.
struct Key(HKEY);

impl Key {
    fn open(root: HKEY, path: &str) -> Option<Key> {
        let path = wide(path);
        let mut key: HKEY = null_mut();
        // SAFETY: `path` is NUL-terminated and `key` is a valid out-pointer.
        let rc = unsafe { RegOpenKeyExW(root, path.as_ptr(), 0, KEY_READ, &mut key) };
        (rc == ERROR_SUCCESS).then_some(Key(key))
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: the key was opened by RegOpenKeyExW and is closed exactly once, here.
        unsafe { RegCloseKey(self.0) };
    }
}

/// Subkey names of `HKLM\SOFTWARE\ASIO`, in registry order.
fn registry_names() -> Vec<String> {
    let Some(root) = Key::open(HKEY_LOCAL_MACHINE, ASIO_KEY) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for index in 0u32.. {
        let mut buf = [0u16; 256];
        let mut len = buf.len() as u32;
        // SAFETY: `root` is an open key; `buf`/`len` describe writable wide-char storage; the
        // optional out-parameters may be null.
        let rc = unsafe { RegEnumKeyExW(root.0, index, buf.as_mut_ptr(), &mut len, null(), null_mut(), null_mut(), null_mut()) };
        if rc != ERROR_SUCCESS {
            break;
        }
        names.push(String::from_utf16_lossy(buf.get(..len as usize).unwrap_or(&[])));
    }
    names
}

/// Installed drivers, sorted by name.
pub(crate) fn drivers() -> Vec<String> {
    let mut names = registry_names();
    names.sort();
    names
}

/// The driver's CLSID from its registry subkey.
fn registry_clsid(name: &str) -> Result<GUID, Error> {
    let not_found = || Error::NotFound(name.to_owned());
    if name.is_empty() || name.contains(['\\', '/', '\0']) {
        return Err(not_found());
    }
    let key = Key::open(HKEY_LOCAL_MACHINE, &format!("{ASIO_KEY}\\{name}")).ok_or_else(not_found)?;
    let value = wide("CLSID");
    let mut buf = [0u16; 128];
    let mut size = (buf.len() * 2) as u32;
    let mut kind = 0u32;
    // SAFETY: `key` is open; `value` is NUL-terminated; `buf` has `size` writable bytes; `kind` is
    // a valid out-pointer.
    let rc = unsafe { RegQueryValueExW(key.0, value.as_ptr(), null(), &mut kind, buf.as_mut_ptr().cast(), &mut size) };
    if rc != ERROR_SUCCESS {
        return Err(not_found());
    }
    let text = buf.split(|&c| c == 0).next().map(String::from_utf16_lossy).unwrap_or_default();
    let wide_text = wide(&text);
    let mut clsid = GUID { data1: 0, data2: 0, data3: 0, data4: [0; 8] };
    // SAFETY: `wide_text` is NUL-terminated and `clsid` is a valid out-pointer.
    let hr = unsafe { CLSIDFromString(wide_text.as_ptr(), &mut clsid) };
    if hr < 0 {
        return Err(Error::Driver(format!("bad CLSID for \"{name}\"")));
    }
    Ok(clsid)
}

/// Pumps this thread's window messages (STA drivers need it).
fn pump_messages() {
    // SAFETY: MSG is plain old data, and all-zero is a valid value.
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    // SAFETY: `msg` is a valid, writable MSG, and a null window reads this thread's queue.
    while unsafe { PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) } != 0 {
        // SAFETY: `msg` was just filled in by PeekMessageW.
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Runs `f` on a fresh worker thread (its own COM apartment) and returns its result.
fn on_worker<T: Send + 'static>(f: impl FnOnce() -> Result<T, Error> + Send + 'static) -> Result<T, Error> {
    let handle = thread::Builder::new()
        .name("soundcraft-asio-probe".into())
        .spawn(f)
        .map_err(|e| Error::Driver(format!("cannot start a driver thread: {e}")))?;
    handle.join().unwrap_or_else(|_| Err(Error::Driver("driver thread panicked".into())))
}

/// Loads the driver just long enough to read its caps.
pub(crate) fn probe(name: &str) -> Result<DriverCaps, Error> {
    {
        let open = lock(&OPEN);
        if let Some(o) = open.as_ref().filter(|o| o.name == name) {
            return o.caps.clone().ok_or_else(|| Error::Busy(name.to_owned()));
        }
    }
    let clsid = registry_clsid(name)?;
    let owned = name.to_owned();
    on_worker(move || {
        let live = Live::load(&clsid)?;
        live.caps(&owned)
    })
}

/// Opens the driver's control panel: through the open stream when it is that driver, else on a
/// worker thread that loads the driver briefly.
pub(crate) fn control_panel(name: &str) -> Result<(), Error> {
    let tx = lock(&OPEN).as_ref().filter(|o| o.name == name).map(|o| o.tx.clone());
    if let Some(tx) = tx {
        return send_control(&tx);
    }
    let clsid = registry_clsid(name)?;
    on_worker(move || {
        let live = Live::load(&clsid)?;
        live.control_panel()
    })
}

fn send_control(tx: &Sender<Cmd>) -> Result<(), Error> {
    let (answer, reply) = channel();
    tx.send(Cmd::ControlPanel(answer)).map_err(|_| Error::Driver("the stream is closed".into()))?;
    match reply.recv() {
        Ok(result) => result,
        Err(_) => Err(Error::Driver("the stream is closed".into())),
    }
}

/// Commands the owner thread takes between audio-driver messages.
enum Cmd {
    ControlPanel(Sender<Result<(), Error>>),
    Close,
}

/// What `open` needs the owner thread to do.
struct Job {
    name: String,
    clsid: GUID,
    rate: Option<u32>,
    buffer: Option<u32>,
    max_outputs: usize,
    max_inputs: usize,
    cb: Box<dyn Callback>,
}

/// What the owner reports back after a successful open.
struct Info {
    sample_rate: u32,
    buffer_frames: u32,
    input_channels: usize,
    output_channels: usize,
    latency: (u32, u32),
    caps: DriverCaps,
    flags: Arc<Flags>,
}

/// The open stream, as seen by the free functions (control panel, busy check, probe).
struct Open {
    name: String,
    caps: Option<DriverCaps>,
    tx: Sender<Cmd>,
}

/// The one open stream in the process. Set while opening, cleared when it closes.
static OPEN: Mutex<Option<Open>> = Mutex::new(None);

/// The owner thread: loads and opens the driver, then serves commands until told to close.
fn owner_main(job: Job, reply: Sender<Result<Info, Error>>, cmds: Receiver<Cmd>) {
    let mut live = match Live::load(&job.clsid) {
        Ok(live) => live,
        Err(e) => {
            let _ = reply.send(Err(e));
            return;
        }
    };
    let info = match live.open_stream(job) {
        Ok(info) => info,
        Err(e) => {
            let _ = reply.send(Err(e));
            return;
        }
    };
    if reply.send(Ok(info)).is_err() {
        return;
    }
    loop {
        match cmds.recv_timeout(Duration::from_millis(20)) {
            Ok(Cmd::ControlPanel(answer)) => {
                let _ = answer.send(live.control_panel());
            }
            Ok(Cmd::Close) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
        pump_messages();
    }
    drop(live);
}

/// An open, running stream. Owned by `AsioStream`.
pub(crate) struct Stream {
    name: String,
    info: Info,
    tx: Sender<Cmd>,
    join: Option<JoinHandle<()>>,
}

impl Stream {
    pub(crate) fn driver(&self) -> &str {
        &self.name
    }

    pub(crate) fn sample_rate(&self) -> u32 {
        self.info.sample_rate
    }

    pub(crate) fn buffer_frames(&self) -> u32 {
        self.info.buffer_frames
    }

    pub(crate) fn input_channels(&self) -> usize {
        self.info.input_channels
    }

    pub(crate) fn output_channels(&self) -> usize {
        self.info.output_channels
    }

    pub(crate) fn latency(&self) -> (u32, u32) {
        self.info.latency
    }

    pub(crate) fn caps(&self) -> &DriverCaps {
        &self.info.caps
    }

    pub(crate) fn reset_requested(&self) -> bool {
        self.info.flags.reset.load(Ordering::Acquire)
    }

    /// True once a panic escaped the audio callback; the stream then plays silence.
    pub(crate) fn failed(&self) -> bool {
        self.info.flags.failed.load(Ordering::Acquire)
    }

    pub(crate) fn control_panel(&self) -> Result<(), Error> {
        send_control(&self.tx)
    }

    /// Stops the driver and releases it, on the owner thread, and waits for that to finish.
    pub(crate) fn close(&mut self) {
        if let Some(join) = self.join.take() {
            let _ = self.tx.send(Cmd::Close);
            let _ = join.join();
        }
        let mut open = lock(&OPEN);
        if open.as_ref().is_some_and(|o| o.name == self.name) {
            *open = None;
        }
    }
}

/// Opens and starts `cfg.driver`. Fails with `Busy` while another stream is open.
pub(crate) fn open(cfg: &StreamConfig, cb: Box<dyn Callback>) -> Result<Stream, Error> {
    let clsid = registry_clsid(&cfg.driver)?;
    let (tx, rx) = channel();
    {
        let mut open = lock(&OPEN);
        if let Some(busy) = open.as_ref() {
            return Err(Error::Busy(busy.name.clone()));
        }
        *open = Some(Open { name: cfg.driver.clone(), caps: None, tx: tx.clone() });
    }
    let (reply_tx, reply_rx) = channel();
    let job = Job {
        name: cfg.driver.clone(),
        clsid,
        rate: cfg.sample_rate,
        buffer: cfg.buffer_frames,
        max_outputs: cfg.max_outputs,
        max_inputs: cfg.max_inputs.min(MAX_OPEN_CHANNELS),
        cb,
    };
    let join = match thread::Builder::new().name("soundcraft-asio".into()).spawn(move || owner_main(job, reply_tx, rx)) {
        Ok(join) => join,
        Err(e) => {
            lock(&OPEN).take();
            return Err(Error::Driver(format!("cannot start the driver thread: {e}")));
        }
    };
    let result = match reply_rx.recv() {
        Ok(result) => result,
        Err(_) => Err(Error::Driver("the driver thread stopped".into())),
    };
    match result {
        Ok(info) => {
            if let Some(o) = lock(&OPEN).as_mut() {
                o.caps = Some(info.caps.clone());
            }
            Ok(Stream { name: cfg.driver.clone(), info, tx, join: Some(join) })
        }
        Err(e) => {
            let _ = join.join();
            lock(&OPEN).take();
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::quiet_period_ms;

    #[test]
    fn the_quiet_period_is_two_buffers_clamped_to_5_to_100_ms() {
        assert_eq!(quiet_period_ms(48_000, 512), 21, "two 512-frame buffers at 48 kHz");
        assert_eq!(quiet_period_ms(44_100, 64), 5, "a tiny buffer is still given 5 ms");
        assert_eq!(quiet_period_ms(48_000, 1 << 18), 100, "a huge buffer is capped at 100 ms");
        assert_eq!(quiet_period_ms(0, 512), 100, "a zero rate cannot divide by zero");
    }
}
