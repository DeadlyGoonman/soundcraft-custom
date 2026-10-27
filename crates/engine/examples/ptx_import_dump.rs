//! Dev tool: imports a Pro Tools session as the app does and prints the clips that land on the
//! tracks whose name contains a text (start, end, source offset, muted), in order.
//! `cargo run --release -p soundcraft-engine --example ptx_import_dump -- <file.ptx> <track text>`

use soundcraft_engine::{Engine, ptx};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(file), Some(want)) = (args.first(), args.get(1)) else { return eprintln!("usage: <file.ptx> <track text>") };
    let mut e = Engine::new(Default::default());
    let missing = ptx::open_ptx(&mut e, file).expect("import");
    println!("missing files: {}", missing.len());
    let s = e.session();
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
