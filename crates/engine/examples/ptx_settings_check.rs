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
    if name == "--session" {
        return session(file);
    }
    let id = ptx::installed_plugins().into_iter().find(|(_, n)| n == name).map(|(i, _)| i).expect("not installed");
    let data = std::fs::read(file).expect("read");
    let Some(state) = ptx_settings::convert(&id, &data, &mut Default::default()) else { return println!("no conversion") };
    let n = soundcraft_mix::create_instance(&id, Some(&state), 48_000.0, 512, 2).expect("cannot create");
    println!("refused: {}", n.refused_state);
    for p in n.plugin.info().params.iter().take(shown) {
        println!("{:<32} {:?}", p.name, n.plugin.param(p.id));
    }
}

/// `--session <file.ptx>`: per insert, whether its settings convert and the plugin takes them.
fn session(file: &str) {
    let p = ptx::parse(&ptx::decode(&std::fs::read(file).expect("read")));
    let installed = ptx::installed_plugins();
    let mut templates = Default::default();
    let mut tally: std::collections::BTreeMap<String, [usize; 4]> = Default::default();
    for ins in &p.inserts {
        // [carried, defaults (not converted), refused, not installed]
        let row = tally.entry(ins.plugin.clone()).or_default();
        if ptx_settings::is_autotune_variant(&ins.plugin) {
            // As the import does: Auto-Tune Pro with the values that can be read.
            let Some(id) = ptx::installed_match("Auto-Tune Pro", &installed, true) else {
                row[3] += 1;
                continue;
            };
            let params = ins.state.as_deref().map(ptx_settings::autotune_params).unwrap_or_default();
            let Some(mut n) = soundcraft_mix::create_instance(&id, None, 48_000.0, 512, 2) else {
                row[2] += 1;
                continue;
            };
            for (k, v) in &params {
                n.plugin.set_param(k, *v);
            }
            let back: Vec<String> = params.keys().map(|k| format!("{k}={:?}", n.plugin.param(k))).collect();
            if params.is_empty() {
                row[1] += 1;
            } else {
                row[0] += 1;
                println!("  {} -> Pro {params:?} read back {}", ins.plugin, back.join(" "));
            }
            continue;
        }
        let Some(id) = ptx::installed_match(&ins.plugin, &installed, true) else {
            row[3] += 1;
            continue;
        };
        let Some(state) = ins.state.as_deref().and_then(|s| ptx_settings::convert(&id, s, &mut templates)) else {
            row[1] += 1;
            continue;
        };
        match soundcraft_mix::create_instance(&id, Some(&state), 48_000.0, 512, 2) {
            Some(n) if !n.refused_state => row[0] += 1,
            _ => row[2] += 1,
        }
    }
    println!("{:<34} carried defaults refused missing", "plugin");
    for (name, [c, d, r, m]) in tally {
        println!("{name:<34} {c:>7} {d:>8} {r:>7} {m:>7}");
    }
}
