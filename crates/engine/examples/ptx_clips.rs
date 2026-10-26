//! Dev tool: a session's placements on the playlists whose name contains a text, in timeline
//! order, with each clip's file offset and length, and overlaps with the clip before.
//! `cargo run --release -p soundcraft-engine --example ptx_clips -- <file.ptx> [playlist text]`

use soundcraft_engine::ptx;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(file) = args.first() else { return eprintln!("usage: <file.ptx> [playlist text]") };
    let want = args.get(1).map(|s| s.to_lowercase()).unwrap_or_default();
    let p = ptx::parse(&ptx::decode(&std::fs::read(file).expect("read")));
    println!("active playlists: {:?}", p.active);
    let mut rows: Vec<&ptx::PtxPlacement> =
        p.placements.iter().filter(|x| p.active.contains(&x.playlist) && x.playlist.to_lowercase().contains(&want)).collect();
    rows.sort_by_key(|x| (x.playlist.clone(), x.position));
    let mut last: Option<(&str, u64)> = None;
    for x in rows {
        let Some(c) = p.clips.get(x.clip) else { continue };
        let end = x.position + c.length;
        let overlap = match last {
            Some((pl, prev_end)) if pl == x.playlist && prev_end > x.position => format!("  OVERLAPS previous by {}", prev_end - x.position),
            _ => String::new(),
        };
        let file = c.file.and_then(|f| p.files.get(f)).map_or("?", String::as_str);
        println!(
            "{:<24} {:>10}..{:>10} len {:>8} off {:>9} {}{} [{}] {}",
            x.playlist,
            x.position,
            end,
            c.length,
            c.offset,
            c.name,
            if x.muted { " (muted)" } else { "" },
            file,
            overlap
        );
        last = Some((&x.playlist, end));
    }
}
