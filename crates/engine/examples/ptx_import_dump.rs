//! Dev tool: imports a Pro Tools session as the app does and prints the clips that land on the
//! tracks whose name contains a text (start, end, source offset, muted), in order.
//! `cargo run --release -p soundcraft-engine --example ptx_import_dump -- <file.ptx> <track text>`

use soundcraft_engine::plugin_scan::{CACHE_FILE, PluginCache};
use soundcraft_engine::{Engine, ptx};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(file), Some(want)) = (args.first(), args.get(1)) else { return eprintln!("usage: <file.ptx> <track text>") };
    let dir = std::path::PathBuf::from(std::env::var("APPDATA").unwrap_or_default()).join("SoundCraft");
    if let Some(c) = PluginCache::load(&dir.join(CACHE_FILE)) {
        c.apply_to_hosts();
    }
    let mut e = Engine::new(Default::default());
    let missing = ptx::open_ptx(&mut e, file).expect("import");
    println!("missing files: {}", missing.len());
    let s = e.session();
    // `--mix`: each track's pan and inserts, then the plugins that were missing.
    if want == "--mix" {
        for t in &s.tracks {
            let ins: Vec<String> = t.mixer.inserts.iter().flatten().map(|i| i.plugin.clone()).collect();
            println!("{:<28} pan {:?} inserts {ins:?}", t.name, t.mixer.pan);
        }
        for m in &e.missing_plugins {
            println!("missing plugin {} x{}", m.name, m.targets.len());
        }
        return;
    }
    // `--tails`: unmuted clips that play their source to its very end (a whole take's tail).
    if want == "--tails" {
        for t in &s.tracks {
            let Some(pl) = t.playlist() else { continue };
            for c in pl.clips.iter().filter(|c| !c.muted) {
                let frames = c.source().and_then(|id| s.source(id)).map_or(0, |x| x.frames);
                let end = c.source_offset() + c.length;
                if frames > 0 && end as u64 >= frames {
                    println!(
                        "{:<28} {:>9}..{:>9} plays {:>7} of {:>7} from {:>7} {}",
                        t.name,
                        c.start,
                        c.start + c.length,
                        c.length,
                        frames,
                        c.source_offset(),
                        c.name
                    );
                }
            }
        }
        return;
    }
    for t in s.tracks.iter().filter(|t| t.name.to_lowercase().contains(&want.to_lowercase())) {
        println!("== {}", t.name);
        let Some(pl) = t.playlist() else { continue };
        for c in &pl.clips {
            println!(
                "  {:>9}..{:>9} len {:>8} src-off {:>8} {}{}",
                c.start,
                c.start + c.length,
                c.length,
                c.source_offset(),
                c.name,
                if c.muted { " (muted)" } else { "" }
            );
        }
    }
}
