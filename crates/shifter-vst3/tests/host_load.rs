//! Loads the built Shifter plugin as a real `.vst3` bundle through SoundCraft's own VST3 host
//! (in-process, offline: no audio device) and runs it: scan, instantiate, process, parameters,
//! state. Uses the bundle in `SHIFTER_VST3_BUNDLE` when set (e.g. the packaged one), else bundles
//! the cdylib cargo built next to this test.

use soundcraft_dsp::{Category, Unit};
use std::path::{Path, PathBuf};

/// A folder holding `Shifter.vst3`.
fn bundle_dir() -> PathBuf {
    if let Some(b) = std::env::var_os("SHIFTER_VST3_BUNDLE") {
        let b = PathBuf::from(b);
        return b.parent().map(Path::to_path_buf).unwrap_or(b);
    }
    let exe = std::env::current_exe().unwrap();
    let deps = exe.parent().unwrap();
    let lib = if cfg!(windows) {
        "Shifter.dll"
    } else if cfg!(target_os = "macos") {
        "libShifter.dylib"
    } else {
        "libShifter.so"
    };
    let built = [deps.join(lib), deps.parent().unwrap().join(lib)].into_iter().find(|p| p.exists()).expect("the Shifter cdylib next to the test");
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("shifter-vst3-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (folder, exe_name) = if cfg!(windows) {
        ("x86_64-win", "Shifter.vst3")
    } else if cfg!(target_os = "macos") {
        ("MacOS", "Shifter")
    } else {
        ("x86_64-linux", "Shifter.so")
    };
    let bin = dir.join("Shifter.vst3/Contents").join(folder);
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::copy(&built, bin.join(exe_name)).unwrap();
    dir
}

#[test]
fn the_bundle_loads_and_processes_in_the_soundcraft_host() {
    let dir = bundle_dir();
    assert_eq!(soundcraft_vst3_host::add_search_dir(&dir), 1);
    let found = soundcraft_vst3_host::scan_paths(std::slice::from_ref(&dir));
    assert_eq!(found.len(), 1, "{found:?}");
    let d = &found[0];
    assert_eq!(d.name, "Shifter");
    assert_eq!(d.vendor, "SoundCraft");
    assert_eq!(d.sub_categories, vec!["Fx", "Pitch Shift"]);
    assert_eq!(d.category, Category::PitchShift);
    assert!(!d.is_instrument);

    if let Err(e) = soundcraft_vst3_host::instantiate(&d.id) {
        panic!("instantiate: {e}");
    }
    let info = soundcraft_vst3_host::plugin_info(&d.id).unwrap();
    let ids: Vec<&str> = info.params.iter().map(|p| p.id).collect();
    assert_eq!(ids, ["0", "1", "2", "3", "4", "5", "6", "7"], "bypass (8) is the host's");
    let mode = info.param("0").unwrap();
    assert_eq!((mode.unit, mode.choices), (Unit::Choice, &["Twin", "Drift", "Spread"][..]));
    let anchor = info.param("3").unwrap();
    assert!((anchor.min - 20.0).abs() < 1e-2 && (anchor.max - 1000.0).abs() < 1e-1, "{anchor:?}");

    let mut p = soundcraft_vst3_host::create(&d.id).unwrap();
    p.prepare(48_000.0, 512, 2);
    assert_eq!(p.latency(), 0);
    assert!(p.set_param("5", 100.0));
    assert!(p.set_param("0", 2.0));
    assert_eq!(p.param("0"), Some(2.0));
    let mut all = Vec::new();
    for b in 0..40 {
        let x: Vec<f32> = (0..512).map(|i| if b == 0 && i == 0 { 1.0 } else { 0.3 * ((b * 512 + i) as f32 * 0.03).sin() }).collect();
        let mut io = vec![x.clone(), x];
        p.process(&mut io, 512);
        all.extend(io.into_iter().flatten());
    }
    assert!(all.iter().all(|v| v.is_finite() && v.abs() < 4.0));
    assert!(all.iter().any(|v| v.abs() > 0.01));

    // Silence in, silence out once the voices have died away.
    assert!(p.set_param("1", 0.0));
    for _ in 0..40 {
        p.process(&mut vec![vec![0.0; 512]; 2], 512);
    }
    let mut z = vec![vec![0.0f32; 512]; 2];
    p.process(&mut z, 512);
    assert!(z.iter().flatten().all(|v| v.abs() < 1e-6));

    // State through the host's blob.
    assert!(p.set_param("2", 33.0));
    let blob = p.save_state().unwrap();
    let mut q = soundcraft_vst3_host::create(&d.id).unwrap();
    q.prepare(44_100.0, 256, 2);
    assert!(q.load_state(&blob));
    assert_eq!(q.param("0"), Some(2.0));
    assert!((q.param("2").unwrap() - 33.0).abs() < 1e-3);
}
