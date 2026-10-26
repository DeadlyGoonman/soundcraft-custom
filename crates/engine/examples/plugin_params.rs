//! Dev tool: loads one installed plugin (no audio), prints its parameters and writes its fresh
//! state to `<out>`. `cargo run --release -p soundcraft-engine --example plugin_params -- "<name>" <out>`

use soundcraft_engine::plugin_scan::{CACHE_FILE, PluginCache};
use soundcraft_engine::ptx;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(name), Some(out)) = (args.first(), args.get(1)) else { return eprintln!("usage: <plugin name> <out file>") };
    let dir = std::path::PathBuf::from(std::env::var("APPDATA").unwrap_or_default()).join("SoundCraft");
    PluginCache::load(&dir.join(CACHE_FILE)).expect("no plugin cache").apply_to_hosts();
    let installed = ptx::installed_plugins();
    let id = installed.iter().find(|(_, n)| n == name).map(|(i, _)| i.clone()).expect("not installed");
    let mut n = soundcraft_mix::create_instance(&id, None, 48_000.0, 512, 2).expect("cannot create");
    let info = n.plugin.info();
    println!("{id} {} params", info.params.len());
    for p in info.params {
        println!("{:>6} {:<32} min {:<10} max {:<10} def {:<10} now {:?}", p.id, p.name, p.min, p.max, p.default, n.plugin.param(p.id));
    }
    // `PARAM_TEXT=<id>,<id>...`: the plugin's text at 41 normalized values of each parameter.
    if let Ok(ids) = std::env::var("PARAM_TEXT") {
        let vst = soundcraft_vst3_host::instantiate_plugin(&id).expect("vst3");
        for pid in ids.split(',') {
            let texts: Vec<String> =
                (0..=40).map(|k| f64::from(k) / 40.0).map(|v| format!("{v:.3}={}", vst.controller_text(pid, v).unwrap_or_default())).collect();
            println!("{pid}: {}", texts.join(" | "));
        }
    }
    if let Some(s) = n.plugin.save_state() {
        std::fs::write(out, s).expect("write");
    }
}
