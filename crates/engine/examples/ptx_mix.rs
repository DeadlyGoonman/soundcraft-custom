//! Dev tool: per track plugins, routing and the raw bytes of each mixer path block (pan hunt).
//! `cargo run -p soundcraft-engine --example ptx_mix -- <file.ptx> [raw]`

use soundcraft_engine::ptx;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let raw = std::fs::read(&args[0]).expect("read");
    let b = ptx::decode(&raw);
    let p = ptx::parse(&b);
    println!("tracks {} inserts {} version {:?}", p.active.len(), p.inserts.len(), p.version);
    for (i, n) in p.active.iter().enumerate() {
        let ins: Vec<String> = p
            .inserts
            .iter()
            .filter(|x| x.playlist == *n)
            .map(|x| format!("{:?}:{}({})", x.slot, x.plugin, x.state.as_ref().map_or(0, Vec::len)))
            .collect();
        println!("{i:3} {n:<28} kind {} route {:?} | {}", p.kinds.get(i).copied().unwrap_or(0), p.routes.get(i), ins.join(", "));
    }
    if args.get(1).map(String::as_str) == Some("pan") {
        let mut i = 0;
        let mut k = 0;
        while i + 9 < b.len() {
            if b[i] == 0x5a && b[i + 7] == 0x1b && b[i + 8] == 0x26 {
                let len = u32::from_le_bytes(b[i + 3..i + 7].try_into().unwrap()) as usize;
                let end = (i + 7 + len).min(b.len());
                let mut j = i + 9;
                let mut first = true;
                let has_ins = (i..end).any(|q| b[q] == 0x5a && b[q + 7] == 0x27 && b[q + 8] == 0x26);
                while j + 9 < end && has_ins {
                    if b[j] == 0x5a && b[j + 7] == 0x0d && b[j + 8] == 0x26 {
                        let l = u32::from_le_bytes(b[j + 3..j + 7].try_into().unwrap()) as usize;
                        if first {
                            let pl = &b[j + 9 + 9..(j + 7 + l).min(b.len())];
                            let w: Vec<i32> = pl
                                .get(..1 + 4 * 37)
                                .map(|x| x[1..].chunks(4).map(|c| i32::from_le_bytes(c.try_into().unwrap())).collect())
                                .unwrap_or_default();
                            println!("{k:3} {:<20} {:?}", p.active.get(k).map_or("", String::as_str), w);
                            first = false;
                        }
                        j += 7 + l;
                    } else {
                        j += 1;
                    }
                }
                if has_ins {
                    k += 1;
                }
                i = end;
                continue;
            }
            i += 1;
        }
        return;
    }
    if args.get(1).is_some() {
        let mut i = 0;
        while i + 9 < b.len() {
            if b[i] == 0x5a && b[i + 7] == 0x0d && b[i + 8] == 0x26 {
                let len = u32::from_le_bytes(b[i + 3..i + 7].try_into().unwrap()) as usize;
                let end = (i + 7 + len).min(b.len());
                println!("260d @{i} len {len}: {}", b[i + 9..end].iter().map(|x| format!("{x:02x}")).collect::<String>());
            }
            i += 1;
        }
    }
}
