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
    // `PTX_DUMP=<name>|<name>...`: also writes the first two states of each of those plugins per
    // format (first 4 bytes) to `<name>-<hex>-N.bin`.
    let dump: Vec<String> = std::env::var("PTX_DUMP").map(|d| d.split('|').map(str::to_string).collect()).unwrap_or_default();
    // `PTX_DUMP_MIN=<bytes>`: only states at least that long.
    let min: usize = std::env::var("PTX_DUMP_MIN").ok().and_then(|s| s.parse().ok()).unwrap_or(0);
    let mut dumped: BTreeMap<String, usize> = BTreeMap::new();
    // `PTX_DUMP_N=<n>`: how many per format (default 2).
    let dump_n: usize = std::env::var("PTX_DUMP_N").ok().and_then(|s| s.parse().ok()).unwrap_or(2);
    // `PTX_FIND=1`: prints each session holding plugins whose settings import carries
    // (score = how many of them), so a test session can be picked.
    let find = std::env::var("PTX_FIND").is_ok();
    let mut take = |raw: &[u8], seen: &mut BTreeMap<String, BTreeMap<String, (usize, usize)>>, at: &str| {
        let p = ptx::parse(&ptx::decode(raw));
        if find {
            let mut hit: Vec<String> = Vec::new();
            for ins in &p.inserts {
                let Some(s) = &ins.state else { continue };
                let carried = match ins.plugin.as_str() {
                    "FabFilter Pro-Q 2" | "FabFilter Pro-Q 3" | "FabFilter Pro-MB" | "FabFilter Pro-L 2" => true,
                    "Auto-Tune Pro" => s.len() >= 10_000,
                    _ => s.starts_with(b"<PresetChunkXMLTree"),
                };
                let tag = if s.starts_with(b"<PresetChunkXMLTree") { "Waves".to_string() } else { ins.plugin.clone() };
                if carried && !hit.contains(&tag) {
                    hit.push(tag);
                }
            }
            if hit.len() >= 3 {
                println!("FIND {} | {at} | {}", hit.len(), hit.join(", "));
            }
        }
        for ins in p.inserts {
            if let Some(s) = &ins.state
                && dump.contains(&ins.plugin)
                && s.len() >= min
            {
                let hex: String = s.iter().take(4).map(|b| format!("{b:02x}")).collect();
                let file = format!("{}-{hex}", ins.plugin.replace([' ', '/'], "_"));
                let n = dumped.entry(file.clone()).or_default();
                if *n < dump_n {
                    let _ = std::fs::write(format!("{file}-{n}.bin"), s);
                    *n += 1;
                }
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
                take(&raw, &mut seen, &f.display().to_string());
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
            let real = entry.name().map(|n| n.to_string()).unwrap_or_default();
            let name = real.to_ascii_lowercase();
            if !name.ends_with(".ptx") || name.contains("__macosx") {
                continue;
            }
            let mut raw = Vec::new();
            if entry.read_to_end(&mut raw).is_ok() {
                take(&raw, &mut seen, &format!("{} :: {real}", f.display()));
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
