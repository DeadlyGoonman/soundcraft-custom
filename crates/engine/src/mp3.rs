//! MP3 export through a system `ffmpeg` binary (no encoder is bundled).

use soundcraft_audio_io::Metadata;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

#[cfg(windows)]
fn quiet(cmd: &mut Command) -> &mut Command {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(0x0800_0000)
}

#[cfg(not(windows))]
fn quiet(cmd: &mut Command) -> &mut Command {
    cmd
}

fn answers(candidate: &Path) -> bool {
    let mut cmd = Command::new(candidate);
    cmd.arg("-version").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    quiet(&mut cmd).status().is_ok_and(|s| s.success())
}

/// A working `ffmpeg`: on `PATH`, else in the WinGet link folders.
pub fn ffmpeg_path() -> Option<PathBuf> {
    if answers(Path::new("ffmpeg")) {
        return Some(PathBuf::from("ffmpeg"));
    }
    ["LOCALAPPDATA", "ProgramFiles"]
        .iter()
        .filter_map(std::env::var_os)
        .map(|base| PathBuf::from(base).join("Microsoft").join("WinGet").join("Links").join("ffmpeg.exe"))
        .find(|p| answers(p))
}

fn arg(a: &str) -> OsString {
    OsString::from(a)
}

/// MP3 holds 48 kHz at most, and only the standard rates.
fn mp3_rate(rate: u32) -> Option<u32> {
    const OK: [u32; 9] = [8000, 11025, 12000, 16000, 22050, 24000, 32000, 44100, 48000];
    if OK.contains(&rate) {
        None
    } else if rate.is_multiple_of(44100) {
        Some(44100)
    } else {
        Some(48000)
    }
}

pub(crate) fn args(wav: &Path, out_part: &Path, meta: Option<&Metadata>, rate: u32, kbps: u32) -> Vec<OsString> {
    let mut a: Vec<OsString> = ["-nostdin", "-y", "-hide_banner", "-loglevel", "error", "-i"].into_iter().map(arg).collect();
    a.push(wav.as_os_str().to_owned());
    for x in ["-map_metadata", "-1", "-codec:a", "libmp3lame", "-b:a"] {
        a.push(arg(x));
    }
    a.push(OsString::from(format!("{}k", kbps.clamp(32, 320))));
    a.push(arg("-id3v2_version"));
    a.push(arg("3"));
    if let Some(r) = mp3_rate(rate) {
        a.push(arg("-ar"));
        a.push(OsString::from(r.to_string()));
    }
    if let Some(m) = meta {
        for (key, value) in
            [("title", &m.title), ("artist", &m.artist), ("album", &m.album), ("genre", &m.genre), ("date", &m.year), ("comment", &m.comment)]
        {
            if !value.is_empty() {
                a.push(arg("-metadata"));
                a.push(OsString::from(format!("{key}={value}")));
            }
        }
    }
    a.push(arg("-f"));
    a.push(arg("mp3"));
    a.push(out_part.as_os_str().to_owned());
    a
}

pub(crate) fn spawn(ffmpeg: &Path, args: &[OsString]) -> std::io::Result<Child> {
    let mut cmd = Command::new(ffmpeg);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
    quiet(&mut cmd).spawn()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strs(a: &[OsString]) -> Vec<String> {
        a.iter().map(|x| x.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn mp3_args_carry_tags_and_rate() {
        let meta = Metadata { title: "T".into(), artist: "A B".into(), year: "2026".into(), ..Metadata::default() };
        let a = strs(&args(Path::new("in.wav"), Path::new("out.mp3.part"), Some(&meta), 96000, 320));
        let joined = a.join(" ");
        assert!(joined.contains("-codec:a libmp3lame -b:a 320k -id3v2_version 3"), "{joined}");
        assert!(joined.contains("-ar 48000"), "{joined}");
        assert!(a.contains(&"title=T".to_string()) && a.contains(&"artist=A B".to_string()) && a.contains(&"date=2026".to_string()));
        assert!(!joined.contains("album="), "{joined}");
        assert_eq!(a.last().map(String::as_str), Some("out.mp3.part"));
        assert!(strs(&args(Path::new("in.wav"), Path::new("o"), None, 44100, 320)).iter().all(|x| x != "-ar"));
        assert!(strs(&args(Path::new("in.wav"), Path::new("o"), None, 88200, 320)).join(" ").contains("-ar 44100"));
        assert!(strs(&args(Path::new("in.wav"), Path::new("o"), None, 44100, 128)).join(" ").contains("-b:a 128k"));
        assert!(strs(&args(Path::new("in.wav"), Path::new("o"), None, 44100, 9999)).join(" ").contains("-b:a 320k"));
    }
}
