//! What agents' edits do to a recording, apart from where they apply (the
//! open editor, or `project.json`): pure functions from a project and the
//! agent's arguments to the changed part, or why not. Times are recorded
//! (source) microseconds, as every agent time is.

use serde_json::{Value, json};
use visor_core::project::{BubbleShape, MAX_SPEED, MIN_SPEED};
use visor_core::{Blur, EditList, Marker, Micros, Project, Range};

/// How close to a time a marker counts as the one at it.
pub const MARKER_REACH: Micros = 500_000;

fn secs(us: Micros) -> f64 {
    (us as f64 / 1e3).round() / 1e3
}

fn at(us: Micros) -> String {
    format!("{} s", secs(us))
}

/// The edit with the clip at `t` cut in two there.
pub fn split(p: &Project, t: Micros) -> Result<EditList, String> {
    let mut edit = p.edit.clone();
    if p.edit.clip_at(t).is_none() {
        return Err(format!(
            "No clip plays at {}: it's removed or past the end.",
            at(t)
        ));
    }
    if !edit.split(t) {
        return Err(format!("{} is already where a clip starts.", at(t)));
    }
    Ok(edit)
}

/// The edit with the clip at `t` playing just before the clip at `before`,
/// or last.
pub fn move_clip(p: &Project, t: Micros, before: Option<Micros>) -> Result<EditList, String> {
    let from = p
        .edit
        .clip_at(t)
        .ok_or_else(|| format!("No clip plays at {}.", at(t)))?;
    let to = match before {
        None => p.edit.ranges().len(),
        Some(b) => p
            .edit
            .clip_at(b)
            .ok_or_else(|| format!("No clip plays at {} to move it before.", at(b)))?,
    };
    if to == from {
        return Err("That's the clip being moved.".into());
    }
    let mut edit = p.edit.clone();
    edit.move_clip(from, to);
    if edit == p.edit {
        return Err("It already plays there.".into());
    }
    Ok(edit)
}

/// The edit with the removed stretch at `t` brought back, next to the clip
/// it was removed from.
pub fn bring_back_at(p: &Project, t: Micros) -> Result<EditList, String> {
    let removed = p.edit.removed(p.duration_us);
    let r = removed
        .iter()
        .find(|r| (r.range.0..r.range.1).contains(&t))
        .ok_or_else(|| format!("Nothing is removed at {}.", at(t)))?;
    let mut edit = p.edit.clone();
    edit.bring_back(p.duration_us, r.range);
    Ok(edit)
}

/// The edit with every one of `ranges` removed, closing up the space;
/// what's left shorter than `sliver` between them or an end goes too.
pub fn remove_ranges(
    p: &Project,
    ranges: &[(Micros, Micros)],
    sliver: Micros,
) -> Result<EditList, String> {
    if ranges.is_empty() {
        return Err("Give at least one stretch to remove.".into());
    }
    let mut edit = p.edit.clone();
    for &(start, end) in ranges {
        if end <= start {
            return Err(format!(
                "{} to {}: the end must be after the start.",
                at(start),
                at(end)
            ));
        }
        if start >= p.duration_us {
            return Err(format!("{} is past the end of the recording.", at(start)));
        }
        edit.cut_joining(start, end, sliver);
    }
    if edit.duration() == 0 {
        return Err("That would remove the whole recording.".into());
    }
    Ok(edit)
}

/// The marker nearest `t` within [`MARKER_REACH`], by index.
pub fn find_marker(p: &Project, t: Micros) -> Result<usize, String> {
    let near: Vec<usize> = (0..p.markers.len())
        .filter(|&i| p.markers[i].at_us.abs_diff(t) <= MARKER_REACH)
        .collect();
    match near.as_slice() {
        [] => Err(format!("No marker within 0.5 s of {}.", at(t))),
        [one] => Ok(*one),
        several => {
            let exact: Vec<usize> = several
                .iter()
                .copied()
                .filter(|&i| p.markers[i].at_us.abs_diff(t) < 1_000)
                .collect();
            if let [one] = exact.as_slice() {
                return Ok(*one);
            }
            let list: Vec<String> = several
                .iter()
                .map(|&i| format!("{} “{}”", at(p.markers[i].at_us), p.markers[i].text))
                .collect();
            Err(format!(
                "Several markers are that close: {}. Give one's exact time.",
                list.join(", ")
            ))
        }
    }
}

fn marker_text(text: &str) -> String {
    text.trim().chars().take(200).collect()
}

pub fn add_marker(p: &Project, t: Micros, text: &str) -> Result<Vec<Marker>, String> {
    if t >= p.duration_us {
        return Err(format!("{} is past the end of the recording.", at(t)));
    }
    let mut markers = p.markers.clone();
    markers.push(Marker {
        at_us: t,
        text: marker_text(text),
    });
    markers.sort_by_key(|m| m.at_us);
    Ok(markers)
}

pub fn rename_marker(p: &Project, t: Micros, text: &str) -> Result<Vec<Marker>, String> {
    let i = find_marker(p, t)?;
    let mut markers = p.markers.clone();
    markers[i].text = marker_text(text);
    Ok(markers)
}

pub fn delete_marker(p: &Project, t: Micros) -> Result<Vec<Marker>, String> {
    let i = find_marker(p, t)?;
    let mut markers = p.markers.clone();
    markers.remove(i);
    Ok(markers)
}

/// An area of the frame, as fractions of its width and height.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Area {
    pub x: Option<f64>,
    pub y: Option<f64>,
    pub width: Option<f64>,
    pub height: Option<f64>,
}

/// `b` with what `area` gives, kept inside the frame; refused if nothing
/// of it is left, or a number isn't one.
fn place(mut b: Blur, area: Area) -> Result<Blur, String> {
    for v in [area.x, area.y, area.width, area.height]
        .into_iter()
        .flatten()
    {
        if !v.is_finite() {
            return Err("x, y, width and height are fractions of the frame, 0 to 1.".into());
        }
    }
    b.x = area.x.unwrap_or(b.x).clamp(0.0, 1.0);
    b.y = area.y.unwrap_or(b.y).clamp(0.0, 1.0);
    b.width = area.width.unwrap_or(b.width).clamp(0.0, 1.0 - b.x);
    b.height = area.height.unwrap_or(b.height).clamp(0.0, 1.0 - b.y);
    if b.width <= 0.0 || b.height <= 0.0 {
        return Err("That area has nothing inside the frame.".into());
    }
    Ok(b)
}

/// The blur covering `t`: the only one, or the `number`th (from 1, in the
/// order get_recording lists them) of those that do.
pub fn find_blur(p: &Project, t: Micros, number: Option<usize>) -> Result<usize, String> {
    let covering: Vec<usize> = (0..p.blurs.len())
        .filter(|&i| p.blurs[i].covers(t))
        .collect();
    match (covering.as_slice(), number) {
        ([], _) => Err(format!("No blur covers {}.", at(t))),
        ([one], None) => Ok(*one),
        (_, None) => Err(format!(
            "{} blurs cover {}; say which with number (1 to {}).",
            covering.len(),
            at(t),
            covering.len()
        )),
        (_, Some(n)) => covering
            .get(n.wrapping_sub(1))
            .copied()
            .ok_or_else(|| format!("number is 1 to {}.", covering.len())),
    }
}

fn blur_times(
    p: &Project,
    start: Option<Micros>,
    end: Option<Micros>,
    was: (Micros, Micros),
) -> Result<(Micros, Micros), String> {
    let (s, e) = (
        start.unwrap_or(was.0),
        end.unwrap_or(was.1).min(p.duration_us),
    );
    if e <= s {
        return Err("The blur must end after it starts, inside the recording.".into());
    }
    Ok((s, e))
}

/// The blurs with a new one over `area`, from `start` to `end` (default:
/// the whole recording).
pub fn add_blur(
    p: &Project,
    area: Area,
    start: Option<Micros>,
    end: Option<Micros>,
) -> Result<Vec<Blur>, String> {
    if area.width.is_none() || area.height.is_none() {
        return Err("Give the area: x, y, width and height, as fractions of the frame.".into());
    }
    let (start_us, end_us) = blur_times(p, start, end, (0, p.duration_us))?;
    let blur = place(
        Blur {
            start_us,
            end_us,
            x: 0.0,
            y: 0.0,
            width: 0.0,
            height: 0.0,
        },
        area,
    )?;
    let mut blurs = p.blurs.clone();
    blurs.push(blur);
    Ok(blurs)
}

pub fn change_blur(
    p: &Project,
    t: Micros,
    number: Option<usize>,
    area: Area,
    start: Option<Micros>,
    end: Option<Micros>,
) -> Result<Vec<Blur>, String> {
    let i = find_blur(p, t, number)?;
    let mut blurs = p.blurs.clone();
    let b = blurs[i].clone();
    let (start_us, end_us) = blur_times(p, start, end, (b.start_us, b.end_us))?;
    blurs[i] = place(
        Blur {
            start_us,
            end_us,
            ..b
        },
        area,
    )?;
    Ok(blurs)
}

pub fn delete_blur(p: &Project, t: Micros, number: Option<usize>) -> Result<Vec<Blur>, String> {
    let i = find_blur(p, t, number)?;
    let mut blurs = p.blurs.clone();
    blurs.remove(i);
    Ok(blurs)
}

/// What `set_recording` changes; `None` leaves it as it is.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Looks {
    pub name: Option<String>,
    pub speed: Option<f64>,
    /// `Some(None)`: back to the first kept frame.
    pub thumbnail: Option<Option<Micros>>,
    pub bubble_visible: Option<bool>,
    pub bubble_shape: Option<BubbleShape>,
    pub bubble_x: Option<f64>,
    pub bubble_y: Option<f64>,
    pub bubble_size: Option<f64>,
    pub bubble_mirror: Option<bool>,
}

/// `p` with `looks` applied, or why not.
pub fn set_looks(p: &Project, looks: &Looks) -> Result<Project, String> {
    let mut p = p.clone();
    if let Some(name) = &looks.name {
        let name = name.trim();
        if name.is_empty() {
            return Err("The name can't be empty.".into());
        }
        p.title = name.chars().take(200).collect();
    }
    if let Some(speed) = looks.speed {
        if !(MIN_SPEED..=MAX_SPEED).contains(&speed) {
            return Err(format!("speed is {MIN_SPEED} to {MAX_SPEED}."));
        }
        p.speed = speed;
    }
    if let Some(thumbnail) = looks.thumbnail {
        if let Some(t) = thumbnail
            && p.edit.to_output(t).is_none()
        {
            return Err(format!("{} isn't kept in the video.", at(t)));
        }
        p.thumbnail_us = thumbnail;
    }
    let o = &mut p.camera_overlay;
    let fraction = |v: Option<f64>, what: &str| -> Result<Option<f64>, String> {
        match v {
            Some(v) if !(0.0..=1.0).contains(&v) => Err(format!("{what} is 0 to 1.")),
            v => Ok(v),
        }
    };
    if let Some(v) = looks.bubble_visible {
        o.visible = v;
    }
    if let Some(s) = looks.bubble_shape {
        o.shape = s;
    }
    if let Some(size) = fraction(looks.bubble_size, "bubble size")? {
        if size < 0.05 {
            return Err("bubble size is at least 0.05.".into());
        }
        o.size = size;
    }
    if let Some(x) = fraction(looks.bubble_x, "bubble x")? {
        o.x = x;
    }
    if let Some(y) = fraction(looks.bubble_y, "bubble y")? {
        o.y = y;
    }
    if let Some(m) = looks.bubble_mirror {
        o.mirror = m;
    }
    Ok(p)
}

/// A recording's state for get_recording: what an agent needs to find its
/// way around it and change it. `exports`: file name and size.
pub fn recording_json(
    p: &Project,
    folder: &str,
    exports: &[(String, u64)],
    editor_open: bool,
) -> Value {
    let range = |r: &Range| json!([secs(r.0), secs(r.1)]);
    let clips: Vec<Value> = p.edit.ranges().iter().map(range).collect();
    let removed: Vec<Value> = p
        .edit
        .removed(p.duration_us)
        .iter()
        .map(|r| {
            json!({
                "range": range(&r.range),
                "next_to_clip": r.clip + 1,
                "comes_back_at": if r.end { "end" } else { "start" },
            })
        })
        .collect();
    let markers: Vec<Value> = p
        .markers
        .iter()
        .map(|m| json!({"at_seconds": secs(m.at_us), "text": m.text}))
        .collect();
    let blurs: Vec<Value> = p
        .blurs
        .iter()
        .map(|b| {
            json!({
                "start_seconds": secs(b.start_us),
                "end_seconds": secs(b.end_us),
                "x": b.x, "y": b.y, "width": b.width, "height": b.height,
            })
        })
        .collect();
    let o = &p.camera_overlay;
    let size = p
        .tracks
        .screen
        .as_ref()
        .and_then(|t| Some(json!({"width": t.width?, "height": t.height?})));
    json!({
        "folder": folder,
        "name": p.title,
        "recorded_seconds": secs(p.duration_us),
        "kept_seconds": secs(p.edit.duration()),
        "video_seconds": secs(p.output_duration()),
        "speed": p.speed(),
        "frame": size,
        "clips": clips,
        "removed": removed,
        "markers": markers,
        "blurs": blurs,
        "camera": {
            "recorded": p.has_track(visor_core::TrackKind::Camera),
            "visible": o.visible,
            "shape": if o.shape == BubbleShape::Circle { "circle" } else { "rounded" },
            "x": o.x, "y": o.y, "size": o.size, "mirror": o.mirror,
        },
        "thumbnail_seconds": p.thumbnail_us.map(secs),
        "exports": exports.iter().map(|(n, s)| json!({"file": n, "bytes": s})).collect::<Vec<_>>(),
        "editor_open": editor_open,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Micros = 1_000_000;

    /// 20 s: 5–8 s removed, cut in two at 12 s, the clip from 12 s first.
    fn demo() -> Project {
        let mut p = Project::new("Demo", "2026-11-28T10:00:00Z", 20 * S);
        p.edit.cut(5 * S, 8 * S);
        p.edit.split(12 * S);
        p.edit.move_clip(2, 0);
        p.markers = vec![
            Marker {
                at_us: 3 * S,
                text: "Open".into(),
            },
            Marker {
                at_us: 3 * S + 300_000,
                text: "Click".into(),
            },
            Marker {
                at_us: 15 * S,
                text: "Done".into(),
            },
        ];
        p
    }

    fn r(a: Micros, b: Micros) -> Range {
        Range(a * S, b * S)
    }

    #[test]
    fn clips_split_move_and_come_back() {
        let p = demo();
        assert_eq!(p.edit.ranges(), [r(12, 20), r(0, 5), r(8, 12)]);
        let e = split(&p, 2 * S).unwrap();
        assert_eq!(e.ranges(), [r(12, 20), r(0, 2), r(2, 5), r(8, 12)]);
        assert!(split(&p, 6 * S).unwrap_err().contains("No clip"));
        assert!(split(&p, 8 * S).unwrap_err().contains("starts"));

        // The clip at 9 s before the one at 15 s; then last.
        let e = move_clip(&p, 9 * S, Some(15 * S)).unwrap();
        assert_eq!(e.ranges(), [r(8, 12), r(12, 20), r(0, 5)]);
        let e = move_clip(&p, 15 * S, None).unwrap();
        assert_eq!(e.ranges(), [r(0, 5), r(8, 12), r(12, 20)]);
        assert!(move_clip(&p, 6 * S, None).is_err());
        assert!(move_clip(&p, 9 * S, None).unwrap_err().contains("already"));

        let e = bring_back_at(&p, 6 * S).unwrap();
        assert_eq!(e.duration(), 20 * S);
        assert!(bring_back_at(&p, 9 * S).is_err());
    }

    #[test]
    fn several_stretches_go_in_one_edit() {
        let p = Project::new("Demo", "", 20 * S);
        let e = remove_ranges(&p, &[(2 * S, 3 * S), (10 * S, 12 * S)], 0).unwrap();
        assert_eq!(e.ranges(), [r(0, 2), r(3, 10), r(12, 20)]);
        assert!(remove_ranges(&p, &[], 0).is_err());
        assert!(remove_ranges(&p, &[(3 * S, 2 * S)], 0).is_err());
        assert!(remove_ranges(&p, &[(25 * S, 30 * S)], 0).is_err());
        assert!(
            remove_ranges(&p, &[(0, 20 * S)], 0)
                .unwrap_err()
                .contains("whole")
        );
    }

    #[test]
    fn markers_are_found_by_their_time() {
        let p = demo();
        let m = add_marker(&p, 10 * S, "  Middle ").unwrap();
        assert_eq!(m[2].text, "Middle");
        assert!(add_marker(&p, 20 * S, "x").is_err());
        assert_eq!(
            rename_marker(&p, 15 * S + 400_000, "Finished").unwrap()[2].text,
            "Finished"
        );
        assert!(delete_marker(&p, 16 * S).unwrap_err().contains("No marker"));
        // Two within reach: ambiguous, unless one is exactly there.
        assert!(
            delete_marker(&p, 3 * S + 100_000)
                .unwrap_err()
                .contains("Several")
        );
        assert_eq!(delete_marker(&p, 3 * S).unwrap().len(), 2);
    }

    #[test]
    fn blurs_are_placed_inside_the_frame_and_found_by_time() {
        let p = demo();
        let area = |x, y, w, h| Area {
            x: Some(x),
            y: Some(y),
            width: Some(w),
            height: Some(h),
        };
        let b = add_blur(&p, area(0.8, 0.5, 0.5, 0.2), None, None).unwrap();
        assert_eq!((b[0].start_us, b[0].end_us), (0, 20 * S));
        assert!(
            (b[0].width - 0.2).abs() < 1e-9,
            "kept inside: {}",
            b[0].width
        );
        assert!(add_blur(&p, area(1.0, 0.0, 0.5, 0.5), None, None).is_err());
        assert!(add_blur(&p, area(0.1, 0.1, f64::NAN, 0.1), None, None).is_err());
        assert!(add_blur(&p, Area::default(), None, None).is_err());

        let mut q = p.clone();
        q.blurs = add_blur(&p, area(0.0, 0.0, 0.5, 0.5), Some(2 * S), Some(6 * S)).unwrap();
        q.blurs = add_blur(&q, area(0.5, 0.5, 0.2, 0.2), Some(4 * S), None).unwrap();
        // 5 s: both cover it.
        assert!(
            delete_blur(&q, 5 * S, None)
                .unwrap_err()
                .contains("2 blurs")
        );
        assert_eq!(delete_blur(&q, 5 * S, Some(2)).unwrap().len(), 1);
        assert!(delete_blur(&q, 5 * S, Some(3)).is_err());
        let c = change_blur(
            &q,
            3 * S,
            None,
            Area {
                x: Some(0.25),
                ..Area::default()
            },
            None,
            Some(9 * S),
        )
        .unwrap();
        assert_eq!((c[0].x, c[0].end_us, c[0].width), (0.25, 9 * S, 0.5));
        assert!(change_blur(&q, 3 * S, None, Area::default(), Some(9 * S), None).is_err());
        assert!(delete_blur(&q, 19 * S + 500_000, None).is_ok());
        assert!(
            delete_blur(&p, 5 * S, None)
                .unwrap_err()
                .contains("No blur")
        );
    }

    #[test]
    fn looks_change_only_what_is_given() {
        let p = demo();
        let looks = Looks {
            name: Some(" Tour ".into()),
            speed: Some(1.5),
            thumbnail: Some(Some(9 * S)),
            bubble_visible: Some(false),
            bubble_x: Some(0.7),
            ..Looks::default()
        };
        let q = set_looks(&p, &looks).unwrap();
        assert_eq!(
            (q.title.as_str(), q.speed, q.thumbnail_us),
            ("Tour", 1.5, Some(9 * S))
        );
        assert!(!q.camera_overlay.visible && q.camera_overlay.x == 0.7);
        assert_eq!(q.camera_overlay.y, p.camera_overlay.y);
        let bad = |l: Looks| set_looks(&p, &l).is_err();
        assert!(bad(Looks {
            speed: Some(3.0),
            ..Looks::default()
        }));
        assert!(bad(Looks {
            name: Some("  ".into()),
            ..Looks::default()
        }));
        // A removed moment can't be the thumbnail.
        assert!(bad(Looks {
            thumbnail: Some(Some(6 * S)),
            ..Looks::default()
        }));
        assert!(bad(Looks {
            bubble_size: Some(1.5),
            ..Looks::default()
        }));
        let reset = set_looks(
            &q,
            &Looks {
                thumbnail: Some(None),
                ..Looks::default()
            },
        )
        .unwrap();
        assert_eq!(reset.thumbnail_us, None);
    }

    #[test]
    fn recordings_are_described_for_agents() {
        let mut p = demo();
        p.blurs = vec![Blur {
            start_us: 0,
            end_us: 2 * S,
            x: 0.1,
            y: 0.2,
            width: 0.3,
            height: 0.4,
        }];
        let v = recording_json(&p, "/tmp/Demo", &[("Demo.mp4".into(), 1234)], true);
        assert_eq!(v["clips"], json!([[12.0, 20.0], [0.0, 5.0], [8.0, 12.0]]));
        assert_eq!(v["removed"][0]["range"], json!([5.0, 8.0]));
        assert_eq!(v["markers"][2]["text"], "Done");
        assert_eq!(v["blurs"][0]["width"], 0.3);
        assert_eq!(v["video_seconds"], 17.0);
        assert_eq!(v["exports"][0]["file"], "Demo.mp4");
        assert_eq!(v["editor_open"], true);
    }
}
