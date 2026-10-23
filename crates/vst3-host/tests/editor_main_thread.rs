//! VST3 editors on the process main thread (`harness = false`: the standard harness never runs
//! tests there). Opening a real window would flash on screen, so this only checks what the
//! main thread changes without one: the view probe and the parameter round trip through the
//! controller. The window itself is exercised manually (see `installed_plugins_smoke`).

mod common;

fn main() {
    let dir = common::fixture_dir();
    let gain = soundcraft_vst3_host::scan_paths(&[dir.to_path_buf()]).into_iter().find(|d| d.name == "Fixture Gain").expect("fixture").id;
    let mut p = soundcraft_vst3_host::instantiate_plugin(&gain).expect("instantiate");
    // Editor windows are hosted on macOS and Windows; elsewhere the probe reports "unsupported".
    if cfg!(any(target_os = "macos", windows)) {
        assert_eq!(p.editor_size(), Ok((320, 240)));
    } else {
        assert!(p.editor_size().is_err());
    }
    let mut ed = soundcraft_dsp::Plugin::editor(&mut p).expect("editor handle");
    assert!(!ed.is_open());
    assert_eq!(ed.idle().len(), 1, "the fixture's setup edit");
    // Session → editor: the controller follows (seen through the component handler round trip
    // below and through the editor's own value).
    ed.set_param("7", 1.5);
    ed.set_param("nope", 1.0);
    assert!(ed.idle().is_empty(), "set_param is not reported back as an edit");
    // #599: an identity-mapped (JUCE-style) parameter shows the plugin's own text, verbatim, with
    // its units appended once; correctly mapped parameters and choices keep SoundCraft's format.
    assert_eq!(ed.value_text("13", 0.375).as_deref(), Some("\u{2212}6.0 dB"));
    assert_eq!(ed.value_text("13", 1.0).as_deref(), Some("24.0 dB"));
    assert_eq!(ed.value_text("14", 0.25).as_deref(), Some("3.0 dB"), "units already in the text");
    assert_eq!(ed.value_text("7", 1.5), None);
    assert_eq!(ed.value_text("3", 1.0), None);
    assert_eq!(ed.value_text("nope", 0.5), None);
    drop(p);
    assert_eq!(ed.value_text("13", 0.375), None, "the plugin is gone");
    assert!(!ed.is_open());
    assert!(ed.open().is_err(), "the plugin is gone");
    session_and_gui_sync(dir);
    println!("editor_main_thread: ok");
}

/// The processor and the controller agree on values through the editor's idle, on the main
/// thread: session changes reach the controller, a parameter the GUI edited keeps the GUI's
/// value, and a plugin restart re-reads its values.
fn session_and_gui_sync(dir: &std::path::Path) {
    use soundcraft_dsp::Plugin;
    let gain = soundcraft_vst3_host::scan_paths(&[dir.to_path_buf()]).into_iter().find(|d| d.name == "Fixture Gain").expect("fixture").id;
    let freq_n = |plain: f64| (plain / 20.0).ln() / 1000f64.ln();

    // The processor's "ping" (sent from its second activation) reaches the controller as a GUI
    // edit of Gain to 0.25 (plain 0.5); the session then sets Freq and Gain, and the GUI edit wins.
    let mut p = soundcraft_vst3_host::instantiate_plugin(&gain).expect("instantiate");
    let mut ed = Plugin::editor(&mut p).expect("editor handle");
    assert_eq!(ed.idle().len(), 1, "the fixture's setup edit");
    p.prepare(48_000.0, 256, 2);
    p.set_param("12", 1000.0);
    p.set_param("7", 1.5);
    let gain_before = p.controller_normalized("7").expect("controller gain");
    assert_eq!(ed.idle(), vec![("7".to_string(), 0.5)], "the controller's message arrives as a GUI edit");
    let freq = p.controller_normalized("12").expect("controller freq");
    assert!((freq - freq_n(1000.0)).abs() < 1e-9, "the session's Freq reached the controller: {freq}");
    assert_eq!(p.controller_normalized("7"), Some(gain_before), "the GUI's edit is not overwritten");
    // The GUI's edit comes back through the session (the echo) and is not pushed again.
    p.set_param("7", 0.5);
    assert!(ed.idle().is_empty());
    assert_eq!(p.controller_normalized("7"), Some(gain_before), "the echo is not written back");

    // A plugin restart (`kParamValuesChanged`, from its third activation) re-reads the values:
    // the plugin moved Freq to 0.9 itself, and the processor side adopts it on its next block.
    let mut q = soundcraft_vst3_host::instantiate_plugin(&gain).expect("instantiate");
    let mut qe = Plugin::editor(&mut q).expect("editor handle");
    let _ = qe.idle();
    q.prepare(48_000.0, 256, 2);
    q.prepare(48_000.0, 256, 2);
    let _ = qe.idle();
    let mut io = vec![vec![0.0f32; 256]; 2];
    q.process(&mut io, 256);
    let want = 20.0 * 1000f64.powf(0.9);
    let got = f64::from(q.param("12").expect("freq"));
    assert!((got - want).abs() / want < 1e-3, "adopted the plugin's own Freq: {got} vs {want}");
    assert!((q.controller_normalized("12").expect("controller freq") - 0.9).abs() < 1e-9);
}
