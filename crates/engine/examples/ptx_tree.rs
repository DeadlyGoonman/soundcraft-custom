//! Dev tool: for the placements of a clip (by name), the chain of blocks around each (outermost
//! first: offset, type, length, and the name of 0x1052 name blocks) and the placement's bytes.
//! `cargo run --release -p soundcraft-engine --example ptx_tree -- <file.ptx> <clip name>...`

use soundcraft_engine::ptx;

fn uint(b: &[u8], at: usize, w: usize) -> Option<u64> {
    Some(b.get(at..at + w)?.iter().rev().fold(0u64, |v, &x| (v << 8) | u64::from(x)))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let b = ptx::decode(&std::fs::read(&args[0]).expect("read"));
    let p = ptx::parse(&b);
    // Every plausible block: (start, end, type).
    let all: Vec<(usize, usize, u16)> = (0..b.len().saturating_sub(9))
        .filter(|&i| b[i] == 0x5a)
        .filter_map(|i| {
            let n = usize::try_from(uint(&b, i + 3, 4)?).ok()?;
            let end = i.checked_add(7)?.checked_add(n)?;
            let t = u16::try_from(uint(&b, i + 7, 2)?).ok()?;
            (n >= 2 && end <= b.len()).then_some((i, end, t))
        })
        .collect();
    let defs = all.iter().filter(|(s, _, t)| *t == 0x2628 && b.get(s + 7..s + 9) == Some(&0x2628u16.to_le_bytes()[..])).count();
    println!("clip defs {defs}, parsed clips {}", p.clips.len());
    // `PTX_DEFS=<from>-<to>`: those clip definitions.
    if let Some((a, z)) =
        std::env::var("PTX_DEFS").ok().and_then(|s| s.split_once('-').map(|(a, z)| (a.parse::<usize>().ok(), z.parse::<usize>().ok())))
    {
        for (i, c) in p.clips.iter().enumerate().skip(a.unwrap_or(0)).take(z.unwrap_or(0).saturating_sub(a.unwrap_or(0)) + 1) {
            println!("{i:>5} {:<36} off {:>9} len {:>8} file {:?}", c.name, c.offset, c.length, c.file.and_then(|f| p.files.get(f)));
        }
        return;
    }
    // `PTX_TYPES=1`: how many blocks of each type, with their typical length.
    if std::env::var("PTX_TYPES").is_ok() {
        let mut m: std::collections::BTreeMap<u16, (usize, usize, usize)> = Default::default();
        for &(s, e, t) in &all {
            let r = m.entry(t).or_insert((0, usize::MAX, 0));
            r.0 += 1;
            r.1 = r.1.min(e - s);
            r.2 = r.2.max(e - s);
        }
        for (t, (n, lo, hi)) in m.iter().filter(|(_, (n, _, _))| *n >= 3 && *n < 20_000) {
            println!("{t:04x} x{n:<6} len {lo}..{hi}");
        }
        return;
    }
    // `PTX_OF=<type hex>`: each block of that type, the chain around it and its first bytes.
    if let Some(want) = std::env::var("PTX_OF").ok().and_then(|s| u16::from_str_radix(&s, 16).ok()) {
        let tag = want.to_le_bytes();
        for &(s, e, _) in all.iter().filter(|(s, _, t)| *t == want && b.get(s + 7..s + 9) == Some(&tag[..])).take(8) {
            let chain: Vec<String> = all
                .iter()
                .filter(|(cs, ce, _)| *cs < s && s < *ce && ce - cs < 50_000_000)
                .map(|&(cs, _, t)| {
                    let name = if t == 0x1052 || t == 0x261b {
                        uint(&b, cs + 9, 4)
                            .and_then(|n| b.get(cs + 13..cs + 13 + n.min(64) as usize))
                            .map(|x| String::from_utf8_lossy(x).into_owned())
                    } else {
                        None
                    };
                    format!("{t:04x}{}", name.map(|n| format!("'{n}'")).unwrap_or_default())
                })
                .collect();
            println!("{want:04x} at {s} len {}: {}", e - s, chain.join(" > "));
            for row in b[s..e.min(s + 160)].chunks(32) {
                println!("    {}", row.iter().map(|x| format!("{x:02x}")).collect::<String>());
            }
        }
        return;
    }
    // `PTX_DEF=<clip index>`: that clip definition's block, its children and its bytes.
    if let Some(ci) = std::env::var("PTX_DEF").ok().and_then(|s| s.parse::<usize>().ok()) {
        let tag = 0x2628u16.to_le_bytes();
        let defs: Vec<&(usize, usize, u16)> = all.iter().filter(|(s, _, t)| *t == 0x2628 && b.get(s + 7..s + 9) == Some(&tag[..])).collect();
        let &&(s, e, _) = defs.get(ci).expect("no such clip");
        println!("def {ci} at {s} len {} name {:?}", e - s, p.clips.get(ci).map(|c| &c.name));
        for &(ps, pe, t) in all.iter().filter(|(ps, pe, _)| *ps < s && e <= *pe && pe - ps < 2000) {
            println!("  parent {t:04x} at {ps} len {}", pe - ps);
            for row in b[ps..pe].chunks(32) {
                println!("    {}", row.iter().map(|x| format!("{x:02x}")).collect::<String>());
            }
        }
        for &(cs, ce, t) in all.iter().filter(|(cs, ce, _)| *cs > s && *ce <= e) {
            println!("  child {t:04x} at +{} len {}", cs - s, ce - cs);
        }
        for row in b[s..e.min(s + 512)].chunks(32) {
            println!("  {}", row.iter().map(|x| format!("{x:02x}")).collect::<String>());
        }
        return;
    }
    // `PTX_KIDS=<offset>`: the blocks directly inside the block at that offset, in order.
    if let Some(at) = std::env::var("PTX_KIDS").ok().and_then(|s| s.parse::<usize>().ok()) {
        let end = all.iter().find(|(s, _, _)| *s == at).map(|x| x.1).expect("no block there");
        let mut i = at + 9;
        while i + 9 <= end {
            match all.iter().find(|(s, e, _)| *s == i && *e <= end) {
                Some(&(s, e, t)) => {
                    let extra = match t {
                        0x1050 => {
                            let q = s + 9;
                            let clip = uint(&b, q + 11, 4).unwrap_or(0) as usize;
                            let name = p.clips.get(clip).map_or("?", |c| c.name.as_str());
                            format!(" clip {name} pos {} flag {:02x}", uint(&b, q + 16, 8).unwrap_or(0), b[q + 24])
                        }
                        _ => format!(" {:02x?}", &b[s + 9..(s + 9 + 16).min(e)]),
                    };
                    println!("{s:>8} {t:04x} len {:>6}{extra}", e - s);
                    i = e;
                }
                None => i += 1,
            }
        }
        return;
    }
    for want in &args[1..] {
        let Some(ci) = p.clips.iter().position(|c| c.name == *want) else { continue };
        println!("== {want} (clip {ci})");
        let tag = 0x104fu16.to_le_bytes();
        for &(s, e, _) in
            all.iter().filter(|(s, _, t)| *t == 0x104f && b.get(s + 7..s + 9) == Some(&tag[..]) && uint(&b, s + 11, 4) == Some(ci as u64))
        {
            let chain: Vec<String> = all
                .iter()
                .filter(|(cs, ce, _)| *cs < s && s < *ce && ce - cs < 50_000_000)
                .map(|&(cs, ce, t)| {
                    let name = if t == 0x1052 {
                        uint(&b, cs + 9, 4).and_then(|n| b.get(cs + 13..cs + 13 + n as usize)).map(|x| String::from_utf8_lossy(x).into_owned())
                    } else {
                        None
                    };
                    format!("{t:04x}@{cs}+{}{}", ce - cs, name.map(|n| format!("'{n}'")).unwrap_or_default())
                })
                .collect();
            println!("  at {s}: {}", chain.join(" > "));
            println!("     bytes {:02x?}", &b[s..e.min(s + 48)]);
        }
    }
}
