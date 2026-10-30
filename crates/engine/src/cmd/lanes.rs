//! Automation lanes shown under a track (Pro Tools style): which lanes, in which order.

use super::*;
use crate::cmd;
use serde_json::json;
use soundcraft_model::{AutoParam, Track};

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            "track.lanes_toggle",
            "Show/Hide Automation Lanes",
            [],
            None,
            "{tracks?, open?: bool} — opening with no lanes shows every lane with data, else volume",
            has_selection,
            toggle
        ),
        cmd!(
            "track.lane_add",
            "Add Automation Lane",
            [],
            None,
            "{track?, after?: lane id, param?: automation id} — default: next lane with data not shown, else volume",
            has_selection,
            add
        ),
        cmd!("track.lane_remove", "Remove Automation Lane", [], None, "{track?, param: automation id}", has_selection, remove),
        cmd!("track.lane_set", "Set Automation Lane", [], None, "{track?, lane: lane id, param: automation id}", has_selection, set),
        cmd!(
            "automation.enable",
            "Enable Automation Parameters",
            [],
            None,
            "{kind: volume|pan|mute|send_level|send_pan|send_mute|plugin, enabled?: bool (default: toggle)} — which parameter kinds record",
            always,
            enable
        ),
        cmd!("automation.suspend", "Suspend Automation", [], None, "{on?: bool (default: toggle)} — playback ignores all automation lanes and nothing records", always, suspend),
        cmd!("track.show_automated_lanes", "Show Lanes with Automation", ["Track"], None, "{tracks?}", has_selection, show_automated),
    ]
}

const KINDS: [&str; 7] = ["volume", "pan", "mute", "send_level", "send_pan", "send_mute", "plugin"];

fn enable(e: &mut Engine, p: &Value) -> Result<Value> {
    let kind = str_param(p, "kind").filter(|k| KINDS.contains(k)).ok_or_else(|| bad("automation.enable", format!("`kind` must be one of {}", KINDS.join(", "))))?;
    let ed = &mut e.session_mut().edit;
    let on = p.get("enabled").and_then(Value::as_bool).unwrap_or_else(|| !ed.automation_enabled(kind));
    ed.set_flag(&format!("automation.enable.{kind}.off"), !on);
    Ok(json!({"kind": kind, "enabled": on}))
}

fn suspend(e: &mut Engine, p: &Value) -> Result<Value> {
    let ed = &mut e.session_mut().edit;
    let on = p.get("on").and_then(Value::as_bool).unwrap_or_else(|| !ed.automation_suspended());
    ed.set_flag(soundcraft_model::AUTOMATION_SUSPEND, on);
    Ok(json!({"on": on}))
}

fn canon(cmd: &str, p: &Value, key: &str) -> Result<Option<String>> {
    match str_param(p, key) {
        None => Ok(None),
        Some(s) => AutoParam::parse(s).map(|a| Some(a.id())).ok_or_else(|| bad(cmd, format!("unknown automation param `{s}`"))),
    }
}

/// The lane `add` picks when none is named: next lane with data not shown, else volume, pan, mute.
pub(crate) fn next_lane(t: &Track) -> Option<String> {
    let free = |id: &String| !t.shown_lanes.contains(id);
    t.lanes_with_data().into_iter().find(free).or_else(|| ["volume", "pan", "mute"].map(String::from).into_iter().find(free))
}

fn toggle(e: &mut Engine, p: &Value) -> Result<Value> {
    let tracks = tracks_required(e, "track.lanes_toggle", p)?;
    let open = p.get("open").and_then(Value::as_bool);
    let s = e.session_mut();
    for tid in &tracks {
        let Some(t) = s.track_mut(*tid) else { continue };
        t.lanes_open = open.unwrap_or(!t.lanes_open);
        if t.lanes_open && t.shown_lanes.is_empty() {
            t.shown_lanes = t.default_lanes();
        }
    }
    Ok(json!({}))
}

fn add(e: &mut Engine, p: &Value) -> Result<Value> {
    let tracks = tracks_required(e, "track.lane_add", p)?;
    let param = canon("track.lane_add", p, "param")?;
    let after = canon("track.lane_add", p, "after")?;
    let s = e.session_mut();
    let mut added = None;
    for tid in &tracks {
        let Some(t) = s.track_mut(*tid) else { continue };
        let id = param.clone().or_else(|| next_lane(t)).ok_or_else(|| bad("track.lane_add", "every lane is already shown"))?;
        if t.shown_lanes.contains(&id) {
            return Err(bad("track.lane_add", format!("lane `{id}` is already shown")));
        }
        let at = after.as_ref().and_then(|a| t.shown_lanes.iter().position(|l| l == a)).map_or(t.shown_lanes.len(), |i| i + 1);
        t.shown_lanes.insert(at, id.clone());
        t.lanes_open = true;
        added = Some(id);
    }
    Ok(json!({"lane": added}))
}

fn remove(e: &mut Engine, p: &Value) -> Result<Value> {
    let tracks = tracks_required(e, "track.lane_remove", p)?;
    let id = canon("track.lane_remove", p, "param")?.ok_or_else(|| bad("track.lane_remove", "`param` required"))?;
    let s = e.session_mut();
    for tid in &tracks {
        let Some(t) = s.track_mut(*tid) else { continue };
        t.shown_lanes.retain(|l| *l != id);
        if t.shown_lanes.is_empty() {
            t.lanes_open = false;
        }
        // The lane's points go too, so playback and later passes see no trace of it.
        if let Some(param) = soundcraft_model::AutoParam::parse(&id) {
            t.automation.retain(|l| l.param != param);
        }
    }
    Ok(json!({}))
}

fn set(e: &mut Engine, p: &Value) -> Result<Value> {
    let tracks = tracks_required(e, "track.lane_set", p)?;
    let lane = canon("track.lane_set", p, "lane")?.ok_or_else(|| bad("track.lane_set", "`lane` required"))?;
    let id = canon("track.lane_set", p, "param")?.ok_or_else(|| bad("track.lane_set", "`param` required"))?;
    let s = e.session_mut();
    for tid in &tracks {
        let Some(t) = s.track_mut(*tid) else { continue };
        let Some(i) = t.shown_lanes.iter().position(|l| *l == lane) else {
            return Err(bad("track.lane_set", format!("lane `{lane}` is not shown")));
        };
        if lane != id && t.shown_lanes.contains(&id) {
            return Err(bad("track.lane_set", format!("lane `{id}` is already shown")));
        }
        t.shown_lanes[i] = id.clone();
    }
    Ok(json!({}))
}

fn show_automated(e: &mut Engine, p: &Value) -> Result<Value> {
    let tracks = tracks_required(e, "track.show_automated_lanes", p)?;
    let s = e.session_mut();
    let mut n = 0;
    for tid in &tracks {
        let Some(t) = s.track_mut(*tid) else { continue };
        let data = t.lanes_with_data();
        if data.is_empty() {
            continue;
        }
        for id in data {
            if !t.shown_lanes.contains(&id) {
                t.shown_lanes.push(id);
            }
        }
        t.lanes_open = true;
        n += 1;
    }
    Ok(json!({"tracks": n}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> (Engine, u64) {
        let mut e = Engine::default();
        e.execute("track.new", &json!({})).unwrap();
        let t = e.session().tracks[0].id.0;
        (e, t)
    }

    fn shown(e: &Engine) -> Vec<String> {
        e.session().tracks[0].shown_lanes.clone()
    }

    #[test]
    fn opening_with_no_lanes_shows_volume_or_those_with_data() {
        let (mut e, t) = session();
        e.execute("track.lanes_toggle", &json!({"track": t})).unwrap();
        assert!(e.session().tracks[0].lanes_open);
        assert_eq!(shown(&e), ["volume"]);
        e.execute("track.lanes_toggle", &json!({"track": t})).unwrap();
        assert!(!e.session().tracks[0].lanes_open);
        e.session_mut().tracks[0].shown_lanes.clear();
        e.execute("automation.set_point", &json!({"track": t, "param": "pan", "at": 0, "value": 0.5})).unwrap();
        e.execute("automation.set_point", &json!({"track": t, "param": "plugin:a:13", "at": 0, "value": 0.5})).unwrap();
        e.execute("track.lanes_toggle", &json!({"track": t, "open": true})).unwrap();
        assert_eq!(shown(&e), ["pan", "plugin:a:13"]);
    }

    #[test]
    fn enable_and_suspend_set_flags_and_undo() {
        let (mut e, _) = session();
        assert!(e.session().edit.automation_enabled("pan"));
        e.execute("automation.enable", &json!({"kind": "pan", "enabled": false})).unwrap();
        assert!(!e.session().edit.automation_enabled("pan"));
        assert!(e.session().edit.automation_enabled("volume"));
        e.execute("automation.enable", &json!({"kind": "pan"})).unwrap();
        assert!(e.session().edit.automation_enabled("pan"));
        assert!(e.execute("automation.enable", &json!({"kind": "bogus"})).is_err());
        e.execute("automation.suspend", &json!({})).unwrap();
        assert!(e.session().edit.automation_suspended());
        e.execute("edit.undo", &json!({})).unwrap();
        assert!(!e.session().edit.automation_suspended());
    }

    #[test]
    fn add_remove_set_and_undo() {
        let (mut e, t) = session();
        e.execute("track.lane_add", &json!({"track": t})).unwrap();
        assert_eq!(shown(&e), ["volume"]);
        assert!(e.session().tracks[0].lanes_open);
        e.execute("track.lane_add", &json!({"track": t, "param": "mute"})).unwrap();
        e.execute("track.lane_add", &json!({"track": t, "after": "volume", "param": "plugin:b:7"})).unwrap();
        assert_eq!(shown(&e), ["volume", "plugin:b:7", "mute"]);
        assert!(e.execute("track.lane_add", &json!({"track": t, "param": "mute"})).is_err());
        e.execute("track.lane_set", &json!({"track": t, "lane": "mute", "param": "pan"})).unwrap();
        assert_eq!(shown(&e), ["volume", "plugin:b:7", "pan"]);
        assert!(e.execute("track.lane_set", &json!({"track": t, "lane": "pan", "param": "volume"})).is_err());
        e.execute("track.lane_remove", &json!({"track": t, "param": "pan"})).unwrap();
        assert_eq!(shown(&e), ["volume", "plugin:b:7"]);
        e.execute("edit.undo", &json!({})).unwrap();
        assert_eq!(shown(&e), ["volume", "plugin:b:7", "pan"]);
        assert!(e.execute("track.lane_add", &json!({"track": t, "param": "bogus"})).is_err());
    }

    #[test]
    fn remove_deletes_the_lane_points_and_undo_restores_them() {
        let (mut e, t) = session();
        e.execute("automation.set_point", &json!({"track": t, "param": "pan", "at": 0, "value": 0.5})).unwrap();
        e.execute("track.lane_add", &json!({"track": t, "param": "pan"})).unwrap();
        e.execute("track.lane_remove", &json!({"track": t, "param": "pan"})).unwrap();
        assert!(e.session().tracks[0].lane(&soundcraft_model::AutoParam::Pan(0)).is_none());
        e.execute("edit.undo", &json!({})).unwrap();
        assert!(e.session().tracks[0].lane(&soundcraft_model::AutoParam::Pan(0)).is_some());
    }

    #[test]
    fn default_add_prefers_lanes_with_data() {
        let (mut e, t) = session();
        e.execute("automation.set_point", &json!({"track": t, "param": "pan", "at": 0, "value": 0.5})).unwrap();
        e.execute("track.lane_add", &json!({"track": t})).unwrap();
        assert_eq!(shown(&e), ["pan"]);
        e.execute("track.lane_add", &json!({"track": t})).unwrap();
        assert_eq!(shown(&e), ["pan", "volume"]);
    }

    #[test]
    fn show_automated_lanes_opens_only_tracks_with_data() {
        let (mut e, t) = session();
        e.execute("track.new", &json!({})).unwrap();
        let t2 = e.session().tracks[1].id.0;
        e.execute("automation.set_point", &json!({"track": t, "param": "volume", "at": 0, "value": -3.0})).unwrap();
        e.execute("track.show_automated_lanes", &json!({"tracks": [t, t2]})).unwrap();
        assert_eq!(shown(&e), ["volume"]);
        assert!(!e.session().tracks[1].lanes_open);
    }

    #[test]
    fn a_new_lane_made_while_lanes_are_open_is_shown_but_closed_tracks_stay_closed() {
        let (mut e, t) = session();
        e.execute("automation.set_point", &json!({"track": t, "param": "plugin:a:5", "at": 0, "value": 1.0})).unwrap();
        assert!(shown(&e).is_empty() && !e.session().tracks[0].lanes_open);
        e.execute("track.lane_add", &json!({"track": t, "param": "volume"})).unwrap();
        e.execute("automation.set_point", &json!({"track": t, "param": "plugin:a:9", "at": 0, "value": 1.0})).unwrap();
        e.execute("automation.set_point", &json!({"track": t, "param": "plugin:a:9", "at": 10, "value": 0.0})).unwrap();
        assert_eq!(shown(&e), ["volume", "plugin:a:9"]);
    }
}
