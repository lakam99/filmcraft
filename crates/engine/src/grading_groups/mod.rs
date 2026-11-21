//! Auto-balanced groups and one shared creative look, available to UI, assistants and MCP.
mod prepare;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use filmcraft_project::{ColorGradeGroup, Param, ParamKind, ParamValue, Project, find_effect};
use serde_json::{Value, json};

use crate::commands::{CommandSpec, NamedKeys, always, bad, bool_p, has_seq, str_p};
use crate::{EditorState, Result, Session};

pub(crate) use prepare::{Operation, Plan, apply_plan, calculate};

const CMD: &str = "lumetri.group";

fn spec(id: &'static str, label: &'static str, params: &'static str, run: fn(&mut Session, &Value) -> Result<Value>, journal: bool) -> CommandSpec {
    CommandSpec {
        id,
        label,
        menu: &[],
        shortcut: None,
        params,
        enabled: if matches!(id, "lumetri.group.create" | "lumetri.group.addClips" | "lumetri.group.removeClips") { has_seq } else { always },
        run,
        journal,
        named: NamedKeys::NONE,
    }
}

pub(crate) fn commands() -> Vec<CommandSpec> {
    vec![
        spec(
            "lumetri.group.create",
            "Match selected clips into a color group",
            r#"{"clips":[id]?,"name":str?,"protectSkin":bool=true,"vibe":str?,"strength":percent?,"replaceGroups":bool=false}"#,
            create,
            true,
        ),
        spec("lumetri.group.list", "Inspect color grading groups", r#"{"group":id?,"parameters":bool=false}"#, list, false),
        spec("lumetri.group.exportCube", "Export the shared creative look as .cube", r#"{"group":id?,"path":str,"size":n=33}"#, export_cube, false),
        spec("lumetri.group.balance", "Rebalance a color group", r#"{"group":id?,"protectSkin":bool?}"#, balance, true),
        spec("lumetri.group.addClips", "Match more clips into a color group", r#"{"group":id,"clips":[id]?}"#, add, true),
        spec("lumetri.group.removeClips", "Remove group grading from clips", r#"{"group":id?,"clips":[id]?}"#, remove, true),
        spec("lumetri.group.delete", "Remove a color grading group", r#"{"group":id?}"#, delete, true),
        spec("lumetri.group.rename", "Name a color grading group", r#"{"group":id?,"name":str}"#, rename, true),
        spec("lumetri.group.setParam", "Adjust the shared group grade", r#"{"group":id?,"param":str,"value":any,"merge":bool=false}"#, set_param, true),
        spec(
            "lumetri.group.setSection",
            "Toggle a shared grading section",
            r#"{"group":id?,"section":"basic"|"creative"|"curves"|"wheels"|"hsl"|"vignette","on":bool?}"#,
            set_section,
            true,
        ),
        spec("lumetri.group.setLut", "Set a shared Input LUT or Look LUT", r#"{"group":id?,"slot":"input"|"look","lut":str}"#, set_lut, true),
        spec(
            "lumetri.group.setVibe",
            "Adjust a shared look in everyday terms",
            r#"{"group":id?,"style":str?,"strength":percent?,"brightness":-100..100?,"warmth":-100..100?,"color":-100..100?,"drama":-100..100?,"softness":-100..100?,"merge":bool=false}"#,
            set_vibe,
            true,
        ),
        spec("lumetri.group.setEnabled", "Compare original and group grading", r#"{"group":id?,"enabled":bool?}"#, set_enabled, true),
        spec(
            "lumetri.vibes",
            "List plain-language color looks and controls",
            "{}",
            |_, _| {
                Ok(
                    json!({"styles":filmcraft_render::vibes::STYLES,"controls":{"brightness":"darker to brighter","warmth":"cooler to warmer","color":"muted to colorful","drama":"gentle to dramatic","softness":"crisp to soft","strength":"balanced original to full creative look"},"range":"adjustments -100..100; strength 0..100"}),
                )
            },
            false,
        ),
    ]
}

fn validate_flags(p: &Value) -> Result<()> {
    for key in ["protectSkin", "replaceGroups", "enabled", "on", "merge", "begin", "parameters"] {
        if p.get(key).is_some_and(|v| !v.is_boolean()) {
            return Err(bad(CMD, format!("`{key}` must be true or false")));
        }
    }
    Ok(())
}

pub fn group_id(s: &Session, p: &Value) -> Result<u64> {
    validate_flags(p)?;
    if let Some(value) = p.get("group") {
        let id = value.as_u64().filter(|id| *id > 0).ok_or_else(|| bad(CMD, "`group` must be a positive identity"))?;
        return s.project.color_groups.contains_key(&id).then_some(id).ok_or_else(|| bad(CMD, "no such color grading group"));
    }
    let sequence = s.active_sequence().ok_or_else(|| bad(CMD, "no active sequence"))?;
    let mut found = None;
    for clip in &s.state.selection {
        let Some((_, item)) = sequence.find_item(*clip) else { continue };
        for link in item.effects.iter().filter_map(|e| e.color_group.as_deref()) {
            if found.is_some_and(|id| id != link.group) {
                return Err(bad(CMD, "selected clips belong to different groups; provide `group`"));
            }
            found = Some(link.group);
        }
    }
    found.filter(|id| s.project.color_groups.contains_key(id)).ok_or_else(|| bad(CMD, "select a group member or provide `group`"))
}

fn start(s: &mut Session, operation: Operation) -> Result<Value> {
    prepare::validate(s, &operation)?;
    #[cfg(not(target_arch = "wasm32"))]
    if crate::backend::enabled(s) {
        return crate::backend::start_color_group(s, operation);
    }
    let progress = Arc::new(filmcraft_export::Progress::default());
    let plan = calculate(s, &operation, &progress)?;
    apply_plan(s, plan)
}

fn create(s: &mut Session, p: &Value) -> Result<Value> {
    let operation = prepare::create_operation(s, p)?;
    start(s, operation)
}
fn balance(s: &mut Session, p: &Value) -> Result<Value> {
    let group = group_id(s, p)?;
    start(s, Operation::Balance { group, protect_skin: bool_p(p, "protectSkin") })
}
fn add(s: &mut Session, p: &Value) -> Result<Value> {
    let group = group_id(s, p)?;
    let clips = prepare::selected_clips(s, p)?;
    start(s, Operation::Add { group, sequence: s.state.active_sequence.ok_or_else(|| bad(CMD, "no active sequence"))?, clips })
}

pub fn members(project: &Project, group: u64) -> impl Iterator<Item = (filmcraft_project::ItemId, filmcraft_project::ClipId)> + '_ {
    project.sequences().filter_map(move |item| project.sequence(item.id).map(|q| (item.id, q))).flat_map(move |(id, q)| {
        q.video_tracks
            .iter()
            .flat_map(|t| &t.items)
            .filter(move |c| c.effects.iter().any(|e| e.color_group.as_ref().is_some_and(|g| g.group == group)))
            .map(move |c| (id, c.id))
    })
}

fn list(s: &mut Session, p: &Value) -> Result<Value> {
    validate_flags(p)?;
    let selected = if p.get("group").is_some() { Some(group_id(s, p)?) } else { None };
    let mut groups = Vec::new();
    for (id, group) in &s.project.color_groups {
        if selected.is_some_and(|selected| selected != *id) {
            continue;
        }
        let count = members(&s.project, *id).count();
        let clips = members(&s.project, *id).take(filmcraft_project::MAX_GRADING_GROUP_MEMBERS).collect::<Vec<_>>();
        let mut info = json!({"group":id,"name":group.name,"enabled":group.enabled,"members":count,"workingSpace":group.working.id(),"strength":group.look_strength*100.0,"style":group.vibe.as_ref().and_then(|v| v.style.as_deref()),"protectSkin":group.protect_skin});
        if selected.is_some() {
            info["clips"] = json!(clips.iter().map(|(sequence, clip)| json!({"sequence":sequence.0,"clip":clip.0})).collect::<Vec<_>>());
            info["moreMembers"] = json!(count > clips.len());
            if bool_p(p, "parameters") == Some(true) {
                info["parameters"] = json!(
                    group
                        .look
                        .params
                        .iter()
                        .map(|(id, p)| (id.clone(), crate::commands::param_value_to_json(&p.value)))
                        .collect::<std::collections::BTreeMap<_, _>>()
                );
            }
        }
        groups.push(info);
    }
    Ok(if selected.is_some() { groups.into_iter().next().ok_or_else(|| bad(CMD, "no such group"))? } else { json!({"groups":groups}) })
}

fn export_cube(s: &mut Session, p: &Value) -> Result<Value> {
    let id = group_id(s, p)?;
    let path = str_p(p, "path").ok_or_else(|| bad(CMD, "need `path`"))?;
    let size = crate::commands::checked_u32_p(p, "size", CMD)?.unwrap_or(33) as usize;
    let group = s.project.color_groups.get(&id).ok_or_else(|| bad(CMD, "no such group"))?;
    let pipe = filmcraft_color::ColorPipeline { working: group.working, wide_gamut: group.gamut != filmcraft_color::Gamut::Bt709, auto_tone_map: true };
    let title: String = group.name.chars().filter(|c| !c.is_control()).collect();
    let sample = filmcraft_render::lumetri_cube::sample_lumetri_with_strength(
        &[&group.look],
        Some(&s.project),
        filmcraft_time::Tick::ZERO,
        24.0,
        &pipe,
        size,
        &title,
        group.look_strength,
    )
    .map_err(|error| bad(CMD, error))?;
    let mut left_out = sample.left_out;
    left_out.push("Per-clip automatic matching (retained in the project)");
    let text = sample.lut.to_cube();
    s.services.write_file(path, text.as_bytes()).map_err(|error| bad(CMD, format!("{path}: {error}")))?;
    Ok(json!({"group":id,"path":path,"size":size,"bytes":text.len(),"leftOut":left_out}))
}

fn edit_group<R>(s: &mut Session, id: u64, label: &str, merge: Option<String>, edit: impl FnOnce(&mut ColorGradeGroup) -> Result<R>) -> Result<R> {
    let body = move |project: &mut Project, _: &mut EditorState| {
        let group = project.color_groups.get_mut(&id).ok_or_else(|| bad(CMD, "no such group"))?;
        let group = Arc::make_mut(group);
        let output = edit(group)?;
        group.check().map_err(|e| bad(CMD, e))?;
        Ok(output)
    };
    if let Some(key) = merge { s.edit_merged(label, &key, body) } else { s.edit(label, body) }
}

fn merge_key(p: &Value, id: u64, kind: &str) -> Option<String> {
    (bool_p(p, "merge") == Some(true)).then(|| format!("grading-group:{id}:{kind}"))
}

fn checked_param(id: &str, value: &Value) -> Result<ParamValue> {
    let def = find_effect("lumetri").and_then(|d| d.param(id)).ok_or_else(|| bad(CMD, format!("unknown grading parameter `{id}`")))?;
    let named = match (&def.kind, value) {
        (ParamKind::Choice(options), Value::String(name)) => {
            json!(options.iter().position(|o| o.eq_ignore_ascii_case(name)).ok_or_else(|| bad(CMD, "unknown choice"))?)
        }
        _ => value.clone(),
    };
    let parsed = crate::commands::json_to_param(&def.default, &named).ok_or_else(|| bad(CMD, "grading value has the wrong type or is not finite"))?;
    if let (ParamKind::Float { min, max, .. }, ParamValue::Float(value)) = (&def.kind, &parsed)
        && !(*min..=*max).contains(value)
    {
        return Err(bad(CMD, format!("`{id}` must be between {min} and {max}")));
    }
    if let (ParamKind::Choice(options), ParamValue::Choice(choice)) = (&def.kind, &parsed)
        && *choice as usize >= options.len()
    {
        return Err(bad(CMD, "choice is outside the supported range"));
    }
    if let (ParamKind::Wheel, ParamValue::Vec2(point)) = (&def.kind, &parsed)
        && (point.x.abs() > 1.0 || point.y.abs() > 1.0)
    {
        return Err(bad(CMD, "wheel coordinates must be between -1 and 1"));
    }
    if let ParamValue::Curve(points) = &parsed
        && points.len() > 512
    {
        return Err(bad(CMD, "a shared curve supports at most 512 points"));
    }
    Ok(parsed)
}

fn set_param(s: &mut Session, p: &Value) -> Result<Value> {
    let id = group_id(s, p)?;
    let name = str_p(p, "param").ok_or_else(|| bad(CMD, "need `param`"))?.to_string();
    let value = checked_param(&name, p.get("value").ok_or_else(|| bad(CMD, "need `value`"))?)?;
    if matches!(name.as_str(), "input_lut" | "look_lut")
        && let ParamValue::Text(reference) = &value
        && !reference.is_empty()
        && filmcraft_render::luts::resolve(Some(&s.project), reference).is_none()
    {
        return Err(bad(CMD, "unknown LUT; import it with `lut.import` first"));
    }
    let output = json!({"group":id,"param":name,"value":crate::commands::param_value_to_json(&value)});
    if bool_p(p, "begin") == Some(true) {
        s.history.merge_key = None;
    }
    edit_group(s, id, "Group color grading", merge_key(p, id, &name), |g| {
        g.look.params.insert(name, Param::new(value));
        g.vibe = None;
        Ok(())
    })?;
    Ok(output)
}

fn set_section(s: &mut Session, p: &Value) -> Result<Value> {
    let id = group_id(s, p)?;
    let param = match str_p(p, "section").unwrap_or_default() {
        "basic" => "basic_on",
        "creative" => "creative_on",
        "curves" => "curves_on",
        "wheels" => "wheels_on",
        "hsl" => "hsl_on",
        "vignette" => "vignette_on",
        _ => return Err(bad(CMD, "unknown grading section")),
    };
    let current = s.project.color_groups.get(&id).and_then(|g| g.look.param(param)).and_then(|p| p.value.as_bool()).unwrap_or(param != "hsl_on");
    let on = bool_p(p, "on").unwrap_or(!current);
    set_param(s, &json!({"group":id,"param":param,"value":on}))
}

fn set_lut(s: &mut Session, p: &Value) -> Result<Value> {
    let id = group_id(s, p)?;
    let param = match str_p(p, "slot") {
        Some("input") => "input_lut",
        Some("look") => "look_lut",
        _ => return Err(bad(CMD, "LUT slot must be `input` or `look`")),
    };
    let lut = str_p(p, "lut").ok_or_else(|| bad(CMD, "import the LUT with `lut.import`, then provide its `lut` reference"))?;
    if !lut.is_empty() && filmcraft_render::luts::resolve(Some(&s.project), lut).is_none() {
        return Err(bad(CMD, "unknown LUT"));
    }
    set_param(s, &json!({"group":id,"param":param,"value":lut}))
}

fn set_vibe(s: &mut Session, p: &Value) -> Result<Value> {
    let id = group_id(s, p)?;
    let mut next = s.project.color_groups.get(&id).ok_or_else(|| bad(CMD, "no such group"))?.as_ref().clone();
    apply_vibe(&mut next, p)?;
    if bool_p(p, "begin") == Some(true) {
        s.history.merge_key = None;
    }
    edit_group(s, id, "Group vibe", merge_key(p, id, "vibe"), |g| {
        *g = next;
        Ok(())
    })?;
    let g = s.project.color_groups.get(&id).ok_or_else(|| bad(CMD, "no such group"))?;
    Ok(json!({"group":id,"style":g.vibe.as_ref().and_then(|v| v.style.as_deref()),"strength":g.look_strength*100.0}))
}

pub(crate) fn apply_vibe(group: &mut ColorGradeGroup, p: &Value) -> Result<()> {
    for key in ["style", "vibe"] {
        if p.get(key).is_some_and(|v| !v.as_str().is_some_and(|name| name.len() <= 64)) {
            return Err(bad(CMD, "style must be a Vibes style identifier"));
        }
    }
    if !["style", "vibe", "strength", "brightness", "warmth", "color", "drama", "softness"].iter().any(|key| p.get(key).is_some()) {
        return Err(bad(CMD, "provide a mood or adjustment; use `lumetri.vibes` for choices"));
    }
    group.enabled = true;
    if let Some(style) = str_p(p, "style").or_else(|| str_p(p, "vibe")) {
        let mut base = filmcraft_render::vibes::style(style).ok_or_else(|| bad(CMD, "unknown vibe; use `lumetri.vibes` for choices"))?;
        if let Some(input) = group.look.params.get("input_lut") {
            base.params.insert("input_lut".into(), input.clone());
        }
        group.vibe = Some(filmcraft_render::vibes::controls(&base, Some(style)));
    }
    let controls = group.vibe.get_or_insert_with(|| filmcraft_render::vibes::controls(&group.look, None));
    for (name, slot) in [
        ("brightness", &mut controls.brightness),
        ("warmth", &mut controls.warmth),
        ("color", &mut controls.color),
        ("drama", &mut controls.drama),
        ("softness", &mut controls.softness),
    ] {
        if let Some(value) = p.get(name) {
            let value = value
                .as_f64()
                .filter(|v| v.is_finite() && (-100.0..=100.0).contains(v))
                .ok_or_else(|| bad(CMD, format!("`{name}` must be between -100 and 100")))?;
            *slot = value / 100.0;
        }
    }
    group.look = filmcraft_render::vibes::build(controls).ok_or_else(|| bad(CMD, "invalid Vibes controls"))?;
    if let Some(strength) = p.get("strength") {
        group.look_strength =
            strength.as_f64().filter(|v| v.is_finite() && (0.0..=100.0).contains(v)).ok_or_else(|| bad(CMD, "strength must be between 0 and 100"))? as f32
                / 100.0;
    }
    group.check().map_err(|e| bad(CMD, e))
}

fn rename(s: &mut Session, p: &Value) -> Result<Value> {
    let id = group_id(s, p)?;
    let name = str_p(p, "name").ok_or_else(|| bad(CMD, "need a group name"))?.trim().to_string();
    edit_group(s, id, "Name color group", None, |g| {
        g.name = name.clone();
        Ok(())
    })?;
    Ok(json!({"group":id,"name":name}))
}
fn set_enabled(s: &mut Session, p: &Value) -> Result<Value> {
    let id = group_id(s, p)?;
    let enabled = edit_group(s, id, "Toggle group grading", None, |g| {
        g.enabled = bool_p(p, "enabled").unwrap_or(!g.enabled);
        Ok(g.enabled)
    })?;
    Ok(json!({"group":id,"enabled":enabled}))
}

fn remove(s: &mut Session, p: &Value) -> Result<Value> {
    let id = group_id(s, p)?;
    let clips = prepare::selected_clips(s, p)?;
    let sequence = s.state.active_sequence.ok_or_else(|| bad(CMD, "no active sequence"))?;
    s.edit("Remove group grading", |project, _| {
        let sequence = project.sequence_mut(sequence).ok_or_else(|| bad(CMD, "no active sequence"))?;
        for clip in &clips {
            let (_, item) = sequence.find_item_mut(*clip).ok_or_else(|| bad(CMD, "no such clip"))?;
            if !item.effects.iter().any(|e| e.color_group.as_ref().is_some_and(|g| g.group == id)) {
                return Err(bad(CMD, "a selected clip is not a member of this group"));
            }
            item.effects.retain(|e| !e.color_group.as_ref().is_some_and(|g| g.group == id));
        }
        Ok(())
    })?;
    Ok(json!({"group":id,"removed":clips.len(),"underlyingClipGradesPreserved":true}))
}
fn delete(s: &mut Session, p: &Value) -> Result<Value> {
    let id = group_id(s, p)?;
    let count = members(&s.project, id).count();
    s.edit("Remove color group", |project, _| {
        for item in project.items.values_mut() {
            let filmcraft_project::ItemKind::Sequence(sequence) = &mut item.kind else { continue };
            if !sequence.video_tracks.iter().flat_map(|t| &t.items).any(|c| c.effects.iter().any(|e| e.color_group.as_ref().is_some_and(|g| g.group == id))) {
                continue;
            }
            for clip in Arc::make_mut(sequence).video_tracks.iter_mut().flat_map(|t| &mut t.items) {
                clip.effects.retain(|e| !e.color_group.as_ref().is_some_and(|g| g.group == id));
            }
        }
        project.color_groups.remove(&id);
        Ok(())
    })?;
    Ok(json!({"removedGroup":id,"clips":count,"underlyingClipGradesPreserved":true}))
}
