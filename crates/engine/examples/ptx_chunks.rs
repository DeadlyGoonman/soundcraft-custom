//! Dev survey: which settings formats plugins in Pro Tools sessions use. Reads every `.ptx`
//! inside the zips under a folder (in memory, never extracting) and prints, per plugin name,
//! how often each settings signature (first bytes of its data) shows up.
//!
//! `cargo run --release -p soundcraft-engine --example ptx_chunks -- <folder> [max sessions]`

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::Path;

use soundcraft_engine::ptx;

fn signature(d: &[u8]) -> String {
    let head = d.get(..8).unwrap_or(d);
    if head.starts_with(b"<") {
        return String::from_utf8_lossy(d.get(..20).unwrap_or(d)).into_owned();
    }
    head.iter().map(|&c| if c.is_ascii_graphic() { char::from(c).to_string() } else { format!("\\x{c:02x}") }).collect()
}

fn zips(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            zips(&p, out);
        } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("zip") || x.eq_ignore_ascii_case("ptx")) {
            out.push(p);
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dir = args.first().map_or("D:\\sessions", String::as_str);
    let max: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(usize::MAX);
    let mut files = Vec::new();
    zips(Path::new(dir), &mut files);
    // plugin -> signature -> (count, total length)
    let mut seen: BTreeMap<String, BTreeMap<String, (usize, usize)>> = BTreeMap::new();
    let mut sessions = 0;
    // `PTX_DUMP=<plugin name>`: also writes the first few states of that plugin to `<name>-N.bin`.
    let dump = std::env::var("PTX_DUMP").ok();
    let mut dumped = 0;
    let mut take = |raw: &[u8], seen: &mut BTreeMap<String, BTreeMap<String, (usize, usize)>>| {
        let p = ptx::parse(&ptx::decode(raw));
        for ins in p.inserts {
            if let (Some(d), Some(s)) = (&dump, &ins.state)
                && ins.plugin == *d
                && dumped < 3
            {
                let _ = std::fs::write(format!("{}-{dumped}.bin", d.replace(' ', "_")), s);
                dumped += 1;
            }
            let (sig, len) = ins.state.as_deref().map_or(("<none>".into(), 0), |s| (signature(s), s.len()));
            let e = seen.entry(ins.plugin).or_default().entry(sig).or_default();
            e.0 += 1;
            e.1 += len;
        }
    };
    'all: for f in &files {
        if f.extension().is_some_and(|x| x.eq_ignore_ascii_case("ptx")) {
            if let Ok(raw) = std::fs::read(f) {
                take(&raw, &mut seen);
                sessions += 1;
            }
            if sessions >= max {
                break;
            }
            continue;
        }
        let Ok(file) = std::fs::File::open(f) else { continue };
        let Ok(mut z) = zip::ZipArchive::new(file) else { continue };
        for i in 0..z.len() {
            let Ok(mut entry) = z.by_index(i) else { continue };
            let name = entry.name().map(|n| n.to_ascii_lowercase()).unwrap_or_default();
            if !name.ends_with(".ptx") || name.contains("__macosx") {
                continue;
            }
            let mut raw = Vec::new();
            if entry.read_to_end(&mut raw).is_ok() {
                take(&raw, &mut seen);
                sessions += 1;
            }
            if sessions >= max {
                break 'all;
            }
        }
    }
    println!("sessions: {sessions}");
    let mut rows: Vec<(usize, String, &BTreeMap<String, (usize, usize)>)> = seen.iter().map(|(n, s)| (s.values().map(|v| v.0).sum(), n.clone(), s)).collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.0));
    for (total, name, sigs) in rows {
        println!("{total:5}  {name}");
        for (sig, (n, len)) in sigs {
            println!("        {n:5} x {sig}  (avg {} B)", len / n.max(&1));
        }
    }
}
