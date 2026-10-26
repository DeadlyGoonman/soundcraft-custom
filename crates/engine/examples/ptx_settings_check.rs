//! Dev tool: converts one Pro Tools plugin chunk (plugin data, past Pro Tools' header) for an
//! installed plugin, loads it and prints the first parameters. `SC_EXE` names the app for the
//! settings check of unverified formats.
//! `cargo run --release -p soundcraft-engine --example ptx_settings_check -- "<name>" <data file> [n]`

use soundcraft_engine::plugin_scan::{CACHE_FILE, PluginCache};
use soundcraft_engine::{ptx, ptx_settings};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(name), Some(file)) = (args.first(), args.get(1)) else { return eprintln!("usage: <plugin name> <data file> [n]") };
    let shown: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(12);
    if let Ok(exe) = std::env::var("SC_EXE") {
        ptx_settings::set_probe_exe(exe.into());
    }
    let dir = std::path::PathBuf::from(std::env::var("APPDATA").unwrap_or_default()).join("SoundCraft");
    PluginCache::load(&dir.join(CACHE_FILE)).expect("no plugin cache").apply_to_hosts();
    let id = ptx::installed_plugins().into_iter().find(|(_, n)| n == name).map(|(i, _)| i).expect("not installed");
    let data = std::fs::read(file).expect("read");
    let Some(state) = ptx_settings::convert(&id, &data, &mut Default::default()) else { return println!("no conversion") };
    let n = soundcraft_mix::create_instance(&id, Some(&state), 48_000.0, 512, 2).expect("cannot create");
    println!("refused: {}", n.refused_state);
    for p in n.plugin.info().params.iter().take(shown) {
        println!("{:<32} {:?}", p.name, n.plugin.param(p.id));
    }
}
