//! COM-level tests: the factory, the parameters, the state stream and processing, called the way
//! a host calls them.

use crate::params::{self, BYPASS, PARAM_COUNT, PHASE_SWAP};
use crate::plugin::{Factory, SHIFTER_CID, ShifterPlugin};
use std::cell::RefCell;
use std::ffi::{CStr, c_void};
use std::sync::Mutex;
use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;
use vst3::{Class, ComPtr, ComWrapper};

fn c8(s: &[std::ffi::c_char]) -> String {
    // SAFETY: test buffers written by the plugin are null-terminated.
    unsafe { CStr::from_ptr(s.as_ptr()) }.to_string_lossy().into_owned()
}

fn c16(s: &[TChar]) -> String {
    let n = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    String::from_utf16_lossy(&s[..n].iter().map(|&c| c as u16).collect::<Vec<_>>())
}

// ---- host-side test objects ----------------------------------------------------------------

#[derive(Default)]
struct MemStream {
    data: RefCell<Vec<u8>>,
    pos: RefCell<usize>,
}

impl Class for MemStream {
    type Interfaces = (IBStream,);
}

impl IBStreamTrait for MemStream {
    unsafe fn read(&self, buffer: *mut c_void, n: i32, read: *mut i32) -> tresult {
        let data = self.data.borrow();
        let mut pos = self.pos.borrow_mut();
        let k = (n.max(0) as usize).min(data.len() - *pos);
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr().add(*pos), buffer.cast::<u8>(), k) };
        *pos += k;
        if !read.is_null() {
            unsafe { *read = k as i32 };
        }
        kResultOk
    }
    unsafe fn write(&self, buffer: *mut c_void, n: i32, written: *mut i32) -> tresult {
        let bytes = unsafe { std::slice::from_raw_parts(buffer.cast::<u8>(), n.max(0) as usize) };
        self.data.borrow_mut().extend_from_slice(bytes);
        if !written.is_null() {
            unsafe { *written = n };
        }
        kResultOk
    }
    unsafe fn seek(&self, pos: i64, _mode: i32, result: *mut i64) -> tresult {
        *self.pos.borrow_mut() = pos.max(0) as usize;
        if !result.is_null() {
            unsafe { *result = pos };
        }
        kResultOk
    }
    unsafe fn tell(&self, pos: *mut i64) -> tresult {
        unsafe { *pos = *self.pos.borrow() as i64 };
        kResultOk
    }
}

struct Queue {
    id: u32,
    points: Vec<(i32, f64)>,
}

impl Class for Queue {
    type Interfaces = (IParamValueQueue,);
}

impl IParamValueQueueTrait for Queue {
    unsafe fn getParameterId(&self) -> ParamID {
        self.id
    }
    unsafe fn getPointCount(&self) -> i32 {
        self.points.len() as i32
    }
    unsafe fn getPoint(&self, index: i32, offset: *mut i32, value: *mut f64) -> tresult {
        match self.points.get(index as usize) {
            Some(&(o, v)) => {
                unsafe {
                    *offset = o;
                    *value = v;
                }
                kResultOk
            }
            None => kInvalidArgument,
        }
    }
    unsafe fn addPoint(&self, _o: i32, _v: f64, _i: *mut i32) -> tresult {
        kResultFalse
    }
}

struct Changes {
    queues: Vec<ComWrapper<Queue>>,
}

impl Class for Changes {
    type Interfaces = (IParameterChanges,);
}

impl IParameterChangesTrait for Changes {
    unsafe fn getParameterCount(&self) -> i32 {
        self.queues.len() as i32
    }
    unsafe fn getParameterData(&self, index: i32) -> *mut IParamValueQueue {
        self.queues.get(index as usize).and_then(|q| q.as_com_ref::<IParamValueQueue>()).map(|r| r.as_ptr()).unwrap_or(std::ptr::null_mut())
    }
    unsafe fn addParameterData(&self, _id: *const ParamID, _index: *mut i32) -> *mut IParamValueQueue {
        std::ptr::null_mut()
    }
}

#[derive(Default)]
struct Handler {
    log: Mutex<Vec<(char, u32, f64)>>,
}

impl Class for Handler {
    type Interfaces = (IComponentHandler,);
}

impl IComponentHandlerTrait for Handler {
    unsafe fn beginEdit(&self, id: ParamID) -> tresult {
        self.log.lock().unwrap().push(('b', id, 0.0));
        kResultOk
    }
    unsafe fn performEdit(&self, id: ParamID, v: f64) -> tresult {
        self.log.lock().unwrap().push(('p', id, v));
        kResultOk
    }
    unsafe fn endEdit(&self, id: ParamID) -> tresult {
        self.log.lock().unwrap().push(('e', id, 0.0));
        kResultOk
    }
    unsafe fn restartComponent(&self, _flags: i32) -> tresult {
        kResultOk
    }
}

// ---- helpers -------------------------------------------------------------------------------

fn instance() -> ComPtr<IComponent> {
    let f = ComWrapper::new(Factory).to_com_ptr::<IPluginFactory>().unwrap();
    let mut cid = SHIFTER_CID;
    let iid: TUID = <IComponent as vst3::Interface>::IID.map(|b| b as std::ffi::c_char);
    let mut obj: *mut c_void = std::ptr::null_mut();
    let r = unsafe { f.createInstance(cid.as_mut_ptr(), iid.as_ptr(), &mut obj) };
    assert_eq!(r, kResultOk);
    unsafe { ComPtr::from_raw(obj.cast::<IComponent>()) }.unwrap()
}

fn prepared(sr: f64, block: i32) -> (ComPtr<IComponent>, ComPtr<IAudioProcessor>, ComPtr<IEditController>) {
    let c = instance();
    let p = c.cast::<IAudioProcessor>().unwrap();
    let e = c.cast::<IEditController>().unwrap();
    unsafe {
        assert_eq!(c.initialize(std::ptr::null_mut()), kResultOk);
        let mut ins = [SpeakerArr::kStereo];
        let mut outs = [SpeakerArr::kStereo];
        assert_eq!(p.setBusArrangements(ins.as_mut_ptr(), 1, outs.as_mut_ptr(), 1), kResultTrue);
        let mut setup = ProcessSetup {
            processMode: ProcessModes_::kOffline as i32,
            symbolicSampleSize: SymbolicSampleSizes_::kSample32 as i32,
            maxSamplesPerBlock: block,
            sampleRate: sr,
        };
        assert_eq!(p.setupProcessing(&mut setup), kResultOk);
        assert_eq!(c.setActive(1), kResultOk);
        assert_eq!(p.setProcessing(1), kResultOk);
    }
    (c, p, e)
}

/// Runs one block in place (`io` = left, right) with optional parameter changes.
fn run(p: &ComPtr<IAudioProcessor>, io: &mut [Vec<f32>; 2], changes: Option<&ComWrapper<Changes>>) {
    let frames = io[0].len();
    let mut chans: [*mut f32; 2] = [io[0].as_mut_ptr(), io[1].as_mut_ptr()];
    let mut ins: AudioBusBuffers = unsafe { std::mem::zeroed() };
    ins.numChannels = 2;
    ins.__field0.channelBuffers32 = chans.as_mut_ptr();
    let mut outs: AudioBusBuffers = unsafe { std::mem::zeroed() };
    outs.numChannels = 2;
    outs.__field0.channelBuffers32 = chans.as_mut_ptr();
    let mut d: ProcessData = unsafe { std::mem::zeroed() };
    d.processMode = ProcessModes_::kOffline as i32;
    d.symbolicSampleSize = SymbolicSampleSizes_::kSample32 as i32;
    d.numSamples = frames as i32;
    d.numInputs = 1;
    d.numOutputs = 1;
    d.inputs = &mut ins;
    d.outputs = &mut outs;
    d.inputParameterChanges = changes.and_then(|c| c.as_com_ref::<IParameterChanges>()).map(|r| r.as_ptr()).unwrap_or(std::ptr::null_mut());
    assert_eq!(unsafe { p.process(&mut d) }, kResultOk);
}

fn changes(list: &[(u32, &[(i32, f64)])]) -> ComWrapper<Changes> {
    ComWrapper::new(Changes { queues: list.iter().map(|(id, pts)| ComWrapper::new(Queue { id: *id, points: pts.to_vec() })).collect() })
}

fn set(e: &ComPtr<IEditController>, p: &ComPtr<IAudioProcessor>, id: u32, plain: f64) {
    let n = unsafe { e.plainParamToNormalized(id, plain) };
    unsafe { e.setParamNormalized(id, n) };
    let c = changes(&[(id, &[(0, n)])]);
    run(p, &mut [vec![0.0; 1], vec![0.0; 1]], Some(&c));
}

// ---- tests ---------------------------------------------------------------------------------

#[test]
fn factory_exports_one_audio_module() {
    let f = unsafe { ComPtr::from_raw(crate::GetPluginFactory()) }.unwrap();
    unsafe {
        let mut fi: PFactoryInfo = std::mem::zeroed();
        assert_eq!(f.getFactoryInfo(&mut fi), kResultOk);
        assert_eq!(c8(&fi.vendor), "SoundCraft");
        assert_eq!(f.countClasses(), 1);
        let mut ci: PClassInfo = std::mem::zeroed();
        assert_eq!(f.getClassInfo(0, &mut ci), kResultOk);
        assert_eq!((c8(&ci.category), c8(&ci.name)), ("Audio Module Class".into(), "Shifter".into()));
        assert_eq!(ci.cid, SHIFTER_CID);
        assert_ne!(f.getClassInfo(1, &mut ci), kResultOk);
        let f2 = f.cast::<IPluginFactory2>().unwrap();
        let mut c2: PClassInfo2 = std::mem::zeroed();
        assert_eq!(f2.getClassInfo2(0, &mut c2), kResultOk);
        assert_eq!(c8(&c2.subCategories), "Fx|Pitch Shift");
        // An unknown class id is refused.
        let mut bad: TUID = [0; 16];
        let iid: TUID = <IComponent as vst3::Interface>::IID.map(|b| b as std::ffi::c_char);
        let mut obj: *mut c_void = std::ptr::null_mut();
        assert_ne!(f.createInstance(bad.as_mut_ptr(), iid.as_ptr(), &mut obj), kResultOk);
        assert!(obj.is_null());
    }
}

#[test]
fn parameters_have_titles_units_steps_and_ranges() {
    let (_c, _p, e) = prepared(48_000.0, 256);
    unsafe {
        assert_eq!(e.getParameterCount(), PARAM_COUNT as i32);
        let mut seen = Vec::new();
        for i in 0..PARAM_COUNT as i32 {
            let mut info: ParameterInfo = std::mem::zeroed();
            assert_eq!(e.getParameterInfo(i, &mut info), kResultOk);
            seen.push((info.id, c16(&info.title), c16(&info.units), info.stepCount));
            assert!((info.defaultNormalizedValue - params::default_normalized(info.id)).abs() < 1e-9);
            if info.id == BYPASS {
                assert_ne!(info.flags & ParameterInfo_::ParameterFlags_::kIsBypass, 0);
            }
            if info.id == 0 {
                assert_ne!(info.flags & ParameterInfo_::ParameterFlags_::kIsList, 0);
            }
        }
        let want: Vec<(u32, String, String, i32)> = [
            (0, "Mode", "", 2),
            (1, "Delay", "ms", 0),
            (2, "Detune", "ct", 0),
            (3, "Anchor", "Hz", 0),
            (4, "Width", "%", 0),
            (5, "Mix", "%", 0),
            (6, "Output", "dB", 0),
            (7, "Phase Swap", "", 1),
            (8, "Bypass", "", 1),
        ]
        .iter()
        .map(|&(i, t, u, s)| (i, t.to_string(), u.to_string(), s))
        .collect();
        assert_eq!(seen, want);
        let mut info: ParameterInfo = std::mem::zeroed();
        assert_ne!(e.getParameterInfo(9, &mut info), kResultOk);
        // Plain ranges and the normalized round trip through the controller.
        assert!((e.normalizedParamToPlain(3, 0.0) - 20.0).abs() < 1e-3 && (e.normalizedParamToPlain(3, 1.0) - 1000.0).abs() < 1e-2);
        assert!((e.normalizedParamToPlain(6, 0.0) + 24.0).abs() < 1e-4 && (e.normalizedParamToPlain(6, 1.0) - 12.0).abs() < 1e-4);
        for id in 1..7 {
            for n in [0.0, 0.25, 0.5, 1.0] {
                let back = e.plainParamToNormalized(id, e.normalizedParamToPlain(id, n));
                assert!((back - n).abs() < 1e-5, "id {id} {n} {back}");
            }
        }
        // Text both ways.
        let mut s: String128 = [0; 128];
        assert_eq!(e.getParamStringByValue(0, 1.0, &mut s), kResultOk);
        assert_eq!(c16(&s), "Spread");
        assert_eq!(e.getParamStringByValue(2, e.plainParamToNormalized(2, 25.0), &mut s), kResultOk);
        assert_eq!(c16(&s), "25 ct");
        let mut text: Vec<TChar> = "40 ms".encode_utf16().map(|c| c as TChar).chain(Some(0)).collect();
        let mut v = 0.0;
        assert_eq!(e.getParamValueByString(1, text.as_mut_ptr(), &mut v), kResultOk);
        assert!((e.normalizedParamToPlain(1, v) - 40.0).abs() < 1e-3);
        assert_eq!(e.setParamNormalized(99, 0.5), kInvalidArgument);
    }
}

#[test]
fn state_save_load_round_trip() {
    let (c, _p, e) = prepared(48_000.0, 256);
    let values = [(0u32, 1.0), (1, 0.3), (3, 0.9), (5, 1.0), (PHASE_SWAP, 1.0), (BYPASS, 1.0)];
    unsafe {
        for (id, n) in values {
            e.setParamNormalized(id, n);
        }
        let s = ComWrapper::new(MemStream::default());
        let sp = s.as_com_ref::<IBStream>().unwrap().as_ptr();
        assert_eq!(c.getState(sp), kResultOk);
        let blob = s.data.borrow().clone();
        assert_eq!(&blob[..4], b"SHFT");
        assert_eq!(u32::from_le_bytes(blob[4..8].try_into().unwrap()), params::STATE_VERSION);

        let (c2, _p2, e2) = prepared(44_100.0, 128);
        *s.pos.borrow_mut() = 0;
        assert_eq!(c2.setState(sp), kResultOk);
        for id in 0..PARAM_COUNT as u32 {
            assert!((e2.getParamNormalized(id) - e.getParamNormalized(id)).abs() < 1e-6, "id {id}");
        }
        // Garbage is refused and changes nothing.
        let junk = ComWrapper::new(MemStream { data: RefCell::new(b"not a state at all".to_vec()), pos: RefCell::new(0) });
        let before: Vec<f64> = (0..PARAM_COUNT as u32).map(|id| e2.getParamNormalized(id)).collect();
        assert_eq!(c2.setState(junk.as_com_ref::<IBStream>().unwrap().as_ptr()), kResultFalse);
        let after: Vec<f64> = (0..PARAM_COUNT as u32).map(|id| e2.getParamNormalized(id)).collect();
        assert_eq!(before, after);
    }
}

#[test]
fn silence_in_silence_out() {
    let (_c, p, e) = prepared(48_000.0, 512);
    set(&e, &p, 5, 100.0);
    for _ in 0..20 {
        let mut io = [vec![0.0f32; 512], vec![0.0f32; 512]];
        run(&p, &mut io, None);
        assert!(io.iter().flatten().all(|&x| x == 0.0));
    }
}

#[test]
fn click_with_full_mix_gives_finite_wet_output() {
    for mode in [0.0, 1.0, 2.0] {
        let (_c, p, e) = prepared(48_000.0, 512);
        set(&e, &p, 0, mode);
        set(&e, &p, 5, 100.0);
        let mut out = [Vec::new(), Vec::new()];
        for b in 0..20 {
            let mut io = [vec![0.0f32; 512], vec![0.0f32; 512]];
            if b == 0 {
                io[0][0] = 1.0;
                io[1][0] = 1.0;
            }
            run(&p, &mut io, None);
            out[0].extend_from_slice(&io[0]);
            out[1].extend_from_slice(&io[1]);
        }
        assert!(out.iter().flatten().all(|x| x.is_finite() && x.abs() < 4.0), "mode {mode}");
        // The delayed voices answer after the click (Delay 12 ms + grain).
        let late: f32 = out[0][400..].iter().map(|x| x.abs()).sum();
        assert!(late > 0.01, "mode {mode}: no wet signal ({late})");
    }
}

#[test]
fn bypass_passes_the_input_through() {
    let (_c, p, e) = prepared(48_000.0, 480);
    set(&e, &p, 5, 100.0);
    set(&e, &p, BYPASS, 1.0);
    // Let the 10 ms crossfade finish.
    run(&p, &mut [vec![0.0; 480], vec![0.0; 480]], None);
    let x: Vec<f32> = (0..480).map(|i| (i as f32 * 0.05).sin() * 0.5).collect();
    let mut io = [x.clone(), x.clone()];
    run(&p, &mut io, None);
    assert!(io[0].iter().zip(&x).all(|(a, b)| (a - b).abs() < 1e-6));
}

#[test]
fn parameter_changes_are_sample_accurate() {
    // Output -24 dB from sample 0, then 0 dB (wet off) at sample 100 of the same block.
    let (_c, p, e) = prepared(48_000.0, 256);
    set(&e, &p, 5, 0.0);
    let lo = unsafe { e.plainParamToNormalized(6, -24.0) };
    let hi = unsafe { e.plainParamToNormalized(6, 0.0) };
    let ch = changes(&[(6, &[(0, lo)])]);
    run(&p, &mut [vec![0.0; 256], vec![0.0; 256]], Some(&ch));
    // Settle the gain smoother at -24 dB.
    for _ in 0..200 {
        run(&p, &mut [vec![0.0; 256], vec![0.0; 256]], None);
    }
    let ch = changes(&[(6, &[(100, hi)])]);
    let mut io = [vec![1.0f32; 256], vec![1.0f32; 256]];
    run(&p, &mut io, Some(&ch));
    let g = 10f32.powf(-24.0 / 20.0);
    assert!(io[0][..100].iter().all(|&y| (y - g).abs() < 1e-4), "{:?}", &io[0][95..100]);
    // From sample 100 the gain starts moving up (smoothed), not before.
    assert!(io[0][101] > g + 1e-4 && io[0][255] > io[0][101]);
}

#[test]
fn phase_swap_negates_the_wet_right_voice() {
    let render = |swap: f64| {
        let (_c, p, e) = prepared(48_000.0, 480);
        set(&e, &p, 5, 100.0);
        set(&e, &p, 3, 20.0);
        set(&e, &p, PHASE_SWAP, swap);
        let mut out = [Vec::new(), Vec::new()];
        for b in 0..60 {
            let x: Vec<f32> = (0..480).map(|i| 0.5 * (std::f32::consts::TAU * 2000.0 * (b * 480 + i) as f32 / 48_000.0).sin()).collect();
            let mut io = [x.clone(), x];
            run(&p, &mut io, None);
            out[0].extend_from_slice(&io[0]);
            out[1].extend_from_slice(&io[1]);
        }
        out
    };
    let (off, on) = (render(0.0), render(1.0));
    let max = |v: &mut dyn Iterator<Item = f32>| v.fold(0.0f32, |a, x| a.max(x.abs()));
    // Skip the smoothers' glide (the swap was set after preparing).
    assert!(max(&mut off[0][19200..].iter().zip(&on[0][19200..]).map(|(a, b)| a - b)) < 1e-6, "left unchanged");
    assert!(max(&mut off[1][19200..].iter().copied()) > 0.2);
    assert!(max(&mut off[1][19200..].iter().zip(&on[1][19200..]).map(|(a, b)| a + b)) < 0.03, "right negated");
}

#[test]
fn editor_edits_reach_the_host_handler() {
    let plugin = ComWrapper::new(ShifterPlugin::new());
    let e = plugin.to_com_ptr::<IEditController>().unwrap();
    let h = ComWrapper::new(Handler::default());
    unsafe {
        assert_eq!(e.setComponentHandler(h.as_com_ref::<IComponentHandler>().unwrap().as_ptr()), kResultOk);
    }
    crate::plugin::edit(&plugin.shared, 2, 0.75);
    assert_eq!(*h.log.lock().unwrap(), vec![('b', 2, 0.0), ('p', 2, 0.75), ('e', 2, 0.0)]);
    assert_eq!(unsafe { e.getParamNormalized(2) }, 0.75, "the editor's edit shows in the controller at once");
    unsafe {
        assert_eq!(e.terminate(), kResultOk);
    }
    crate::plugin::edit(&plugin.shared, 2, 0.1);
    assert_eq!(h.log.lock().unwrap().len(), 3, "no edits reach a released handler");
}
#[test]
fn tail_and_latency() {
    let (_c, p, _e) = prepared(48_000.0, 256);
    unsafe {
        assert_eq!(p.getLatencySamples(), 0);
        // Default Delay 12 ms: (12 × 1.8 + 20) ms at 48 kHz + 8.
        assert_eq!(p.getTailSamples(), ((12.0 * 1.8 + 20.0) * 48.0 + 8.0) as u32);
        assert_eq!(p.canProcessSampleSize(SymbolicSampleSizes_::kSample64 as i32), kResultFalse);
        let mut mono = [SpeakerArr::kMono];
        let mut st = [SpeakerArr::kStereo];
        assert_eq!(p.setBusArrangements(mono.as_mut_ptr(), 1, st.as_mut_ptr(), 1), kResultFalse);
    }
}
