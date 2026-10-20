//! Encoding (track writers), preview decoding and export, built on FFmpeg.

mod writer;

pub use writer::VideoWriter;

use visor_core::EditList;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    /// H.264 + AAC.
    Mp4,
    /// VP9 or AV1 + Opus.
    WebM { av1: bool },
    Gif,
}

/// Builds the FFmpeg `trim`/`concat` filter chain for an edit list. This is the
/// pure-string part of export, kept separate so it can be unit-tested.
pub fn trim_concat_filter(edit: &EditList, has_audio: bool) -> String {
    let secs = |us: u64| format!("{}.{:06}", us / 1_000_000, us % 1_000_000);
    let n = edit.ranges().len();
    let mut graph = String::new();
    let mut inputs = String::new();
    for (i, r) in edit.ranges().iter().enumerate() {
        let (s, e) = (secs(r.0), secs(r.1));
        graph.push_str(&format!("[0:v]trim=start={s}:end={e},setpts=PTS-STARTPTS[v{i}];"));
        inputs.push_str(&format!("[v{i}]"));
        if has_audio {
            graph.push_str(&format!("[1:a]atrim=start={s}:end={e},asetpts=PTS-STARTPTS[a{i}];"));
            inputs.push_str(&format!("[a{i}]"));
        }
    }
    let a = u8::from(has_audio);
    graph.push_str(&format!("{inputs}concat=n={n}:v=1:a={a}[vout]"));
    if has_audio {
        graph.push_str("[aout]");
    }
    graph
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_filter_for_two_ranges() {
        let mut e = EditList::full(10_000_000);
        e.cut(2_000_000, 3_500_000);
        assert_eq!(
            trim_concat_filter(&e, true),
            "[0:v]trim=start=0.000000:end=2.000000,setpts=PTS-STARTPTS[v0];\
             [1:a]atrim=start=0.000000:end=2.000000,asetpts=PTS-STARTPTS[a0];\
             [0:v]trim=start=3.500000:end=10.000000,setpts=PTS-STARTPTS[v1];\
             [1:a]atrim=start=3.500000:end=10.000000,asetpts=PTS-STARTPTS[a1];\
             [v0][a0][v1][a1]concat=n=2:v=1:a=1[vout][aout]"
        );
    }
}
