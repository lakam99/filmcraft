use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use filmcraft_export::Progress;
use filmcraft_project::{
    ClipId, ColorGradeGroup, GradeGroupLink, GradeReference, ItemId, MAX_GRADING_GROUP_MEMBERS, MAX_GRADING_GROUPS, Param, TrackKind, find_effect,
};
use filmcraft_render::effects::{FxCtx, apply};
use filmcraft_render::{Image, RenderOptions, color_groups, color_match};
use filmcraft_time::Tick;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::CMD;
use crate::commands::{bad, bool_p, str_p};
use crate::{Result, Session};

#[derive(Clone, Serialize, Deserialize)]
pub(crate) enum Operation {
    Create { sequence: ItemId, clips: Vec<ClipId>, name: String, protect_skin: bool, look: Value, replace_groups: bool },
    Balance { group: u64, protect_skin: Option<bool> },
    Add { group: u64, sequence: ItemId, clips: Vec<ClipId> },
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Correction {
    sequence: ItemId,
    clip: ClipId,
    gain: [f32; 3],
    params: BTreeMap<String, Param>,
    before: f32,
    after: f32,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Plan {
    group: Option<u64>,
    replace_groups: bool,
    grade: ColorGradeGroup,
    corrections: Vec<Correction>,
}

pub(crate) fn selected_clips(s: &Session, p: &Value) -> Result<Vec<ClipId>> {
    let sequence = s.active_sequence().ok_or_else(|| bad(CMD, "no active sequence"))?;
    let explicit = p.get("clips");
    let requested = if let Some(value) = explicit {
        let ids = value
            .as_array()
            .filter(|v| v.len() <= MAX_GRADING_GROUP_MEMBERS)
            .ok_or_else(|| bad(CMD, format!("`clips` must be an array of at most {MAX_GRADING_GROUP_MEMBERS} identities")))?;
        ids.iter().map(|id| id.as_u64().map(ClipId).ok_or_else(|| bad(CMD, "a clip identity must be an unsigned integer"))).collect::<Result<Vec<_>>>()?
    } else {
        if s.state.selection.len() > MAX_GRADING_GROUP_MEMBERS.saturating_mul(2) {
            return Err(bad(CMD, "select at most 256 video clips"));
        }
        s.state.selection.clone()
    };
    let mut seen = BTreeSet::new();
    let mut clips = Vec::new();
    for clip in requested {
        let video = sequence.find_item(clip).is_some_and(|(track, _)| sequence.track(track).is_some_and(|t| t.kind == TrackKind::Video));
        if !video {
            if explicit.is_some() {
                return Err(bad(CMD, "every requested clip must be a video clip in the active sequence"));
            }
            continue;
        }
        if seen.insert(clip) {
            clips.push(clip);
        }
    }
    if clips.is_empty() || clips.len() > MAX_GRADING_GROUP_MEMBERS {
        return Err(bad(CMD, "select 1 to 256 video clips"));
    }
    Ok(clips)
}

pub(crate) fn create_operation(s: &Session, p: &Value) -> Result<Operation> {
    super::validate_flags(p)?;
    let clips = selected_clips(s, p)?;
    if p.get("name").is_some_and(|v| !v.is_string()) {
        return Err(bad(CMD, "group name must be text"));
    }
    let name = str_p(p, "name").map(str::trim).map(str::to_string).unwrap_or_else(|| format!("Look group {}", s.project.color_groups.len().saturating_add(1)));
    if name.is_empty() || name.len() > 512 {
        return Err(bad(CMD, "group name must contain 1 to 512 bytes"));
    }
    let mut look = json!({"style":"natural"});
    for key in ["style", "strength", "brightness", "warmth", "color", "drama", "softness"] {
        if let Some(value) = p.get(key) {
            if (key == "style" && !value.as_str().is_some_and(|v| v.len() <= 64))
                || (key != "style" && !value.as_f64().is_some_and(|v| v.is_finite() && (-100.0..=100.0).contains(&v)))
            {
                return Err(bad(CMD, "invalid initial Vibes control"));
            }
            look[key] = value.clone();
        }
    }
    if let Some(vibe) = p.get("vibe") {
        if !vibe.as_str().is_some_and(|v| v.len() <= 64) {
            return Err(bad(CMD, "vibe must be a style identifier"));
        }
        look["style"] = vibe.clone();
    }
    Ok(Operation::Create {
        sequence: s.state.active_sequence.ok_or_else(|| bad(CMD, "no active sequence"))?,
        clips,
        name,
        protect_skin: bool_p(p, "protectSkin").unwrap_or(true),
        look,
        replace_groups: bool_p(p, "replaceGroups").unwrap_or(false),
    })
}

type Member = (ItemId, ClipId);
fn resolve(s: &Session, operation: &Operation) -> Result<(Option<u64>, ColorGradeGroup, Vec<Member>)> {
    let replacing = matches!(operation, Operation::Create { replace_groups: true, .. });
    let (id, mut grade, mut members) = match operation {
        Operation::Create { sequence, clips, name, protect_skin, look, .. } => {
            if clips.is_empty() || clips.len() > MAX_GRADING_GROUP_MEMBERS {
                return Err(bad(CMD, "select 1 to 256 video clips"));
            }
            if s.project.color_groups.len() >= MAX_GRADING_GROUPS {
                return Err(bad(CMD, "the project has reached its color group limit"));
            }
            let pipe = s.project.sequence(*sequence).ok_or_else(|| bad(CMD, "no such sequence"))?.settings.color;
            let working = pipe.working;
            let mut grade = ColorGradeGroup {
                name: name.clone(),
                enabled: true,
                working,
                gamut: pipe.working_gamut(),
                protect_skin: *protect_skin,
                reference: GradeReference { log_rgb: [0.0; 3], bands: [[0.0; 4]; 3], chroma: 0.0, skin: [0.0; 3] },
                look: find_effect("lumetri").ok_or_else(|| bad(CMD, "Color Grading is unavailable"))?.instance(),
                look_strength: 1.0,
                vibe: None,
            };
            super::apply_vibe(&mut grade, look)?;
            (None, grade, clips.iter().map(|clip| (*sequence, *clip)).collect())
        }
        Operation::Balance { group, .. } | Operation::Add { group, .. } => {
            let grade = s.project.color_groups.get(group).ok_or_else(|| bad(CMD, "no such group"))?.as_ref().clone();
            let members: Vec<Member> = super::members(&s.project, *group).take(MAX_GRADING_GROUP_MEMBERS.saturating_add(1)).collect();
            (Some(*group), grade, members)
        }
    };
    if let Operation::Balance { protect_skin: Some(protect), .. } = operation {
        grade.protect_skin = *protect;
    }
    if let Operation::Add { sequence, clips, .. } = operation {
        if clips.is_empty() || clips.len() > MAX_GRADING_GROUP_MEMBERS {
            return Err(bad(CMD, "add 1 to 256 video clips"));
        }
        for clip in clips {
            if !members.contains(&(*sequence, *clip)) {
                members.push((*sequence, *clip));
            }
        }
    }
    if members.is_empty() || members.len() > MAX_GRADING_GROUP_MEMBERS {
        return Err(bad(CMD, "automatic balancing supports 1 to 256 member clips per run"));
    }
    let mut seen = BTreeSet::new();
    for (sequence, clip) in &members {
        if !seen.insert((*sequence, *clip)) {
            return Err(bad(CMD, "a group member was repeated"));
        }
        let q = s.project.sequence(*sequence).ok_or_else(|| bad(CMD, "missing member sequence"))?;
        q.settings.validate().map_err(|e| bad(CMD, e))?;
        let (track, item) = q.find_item(*clip).ok_or_else(|| bad(CMD, "missing member clip"))?;
        if s.project.item(item.item).is_some_and(|source| matches!(source.kind, filmcraft_project::ItemKind::AdjustmentLayer { .. })) {
            return Err(bad(CMD, "select footage rather than adjustment layers: an adjustment layer has no picture of its own to match"));
        }
        if !q.track(track).is_some_and(|t| t.kind == TrackKind::Video) || item.duration <= Tick::ZERO {
            return Err(bad(CMD, "a group member must be a nonempty video clip"));
        }
        let mut count = 0usize;
        for effect in &item.effects {
            if effect.effect == "grading_group" || effect.color_group.is_some() {
                count = count.saturating_add(1);
                if count > 1 || !effect.color_group.as_ref().is_some_and(|g| Some(g.group) == id || replacing) {
                    return Err(bad(CMD, "a clip already belongs to another grading group; remove that group stage first"));
                }
            }
        }
    }
    grade.check().map_err(|e| bad(CMD, e))?;
    Ok((id, grade, members))
}

pub(crate) fn validate(s: &Session, operation: &Operation) -> Result<()> {
    resolve(s, operation).map(|_| ())
}

struct Sample {
    sequence: ItemId,
    clip: ClipId,
    image: Image,
    logs: Option<[f64; 3]>,
}

pub(crate) fn calculate(s: &Session, operation: &Operation, progress: &Arc<Progress>) -> Result<Plan> {
    let (id, mut grade, members) = resolve(s, operation)?;
    progress.total.store(members.len().saturating_mul(4) as u64, Ordering::Relaxed);
    let mut probe = (*s.project).clone();
    let replace_groups = matches!(operation, Operation::Create { replace_groups: true, .. });
    for (sequence, clip) in &members {
        let (_, item) = probe.sequence_mut(*sequence).and_then(|q| q.find_item_mut(*clip)).ok_or_else(|| bad(CMD, "missing group member"))?;
        if replace_groups {
            item.effects.retain(|e| e.effect != "grading_group" && e.color_group.is_none());
        } else if let Some(index) = item.effects.iter().position(|e| e.color_group.as_ref().is_some_and(|g| Some(g.group) == id)) {
            // Analyse the image entering the group stage, not its existing creative look or any
            // intentionally local effects placed after it.
            item.effects.truncate(index);
        }
    }
    let provider = s.media.full_res_provider(s.project.clone(), s.services.clone()).recording().stop_on_link_error(progress.clone());
    let mut samples = Vec::with_capacity(members.len());
    let mut total_logs = [0.0f64; 3];
    let mut valid_logs = 0u64;
    for (index, (sequence, clip)) in members.iter().enumerate() {
        if progress.cancel.load(Ordering::Acquire) {
            return Err(bad(CMD, "cancelled"));
        }
        let q = probe.sequence(*sequence).ok_or_else(|| bad(CMD, "missing sequence"))?;
        let (_, item) = q.find_item(*clip).ok_or_else(|| bad(CMD, "missing clip"))?;
        *progress.status.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = format!("Reading clip {} of {}: {}", index + 1, members.len(), item.name);
        let scale = (96.0 / q.settings.width as f32).min(96.0 / q.settings.height as f32).min(1.0);
        let opts = RenderOptions { scale, working_output: true, ..Default::default() };
        let mut frames = Vec::with_capacity(3);
        for quarter in [1i64, 2, 3] {
            if progress.cancel.load(Ordering::Acquire) {
                return Err(bad(CMD, "cancelled"));
            }
            let t = item.start + Tick((item.duration.0 / 4).saturating_mul(quarter));
            let mut image = filmcraft_render::render_clip(&probe, *sequence, *clip, t, opts, &provider)
                .ok_or_else(|| bad(CMD, format!("cannot read picture for '{}'", item.name)))?;
            if let Some(error) = provider.link_error() {
                return Err(bad(CMD, error));
            }
            if !provider.offline_used().is_empty() {
                return Err(bad(CMD, "some selected footage is offline; relink it before color matching"));
            }
            if image.w == 0 || image.h == 0 || image.w > 96 || image.h > 96 || !image.px.iter().all(|v| v.is_finite()) {
                return Err(bad(CMD, "a sample frame is invalid"));
            }
            color_groups::convert_gamut(&mut image, q.settings.color.working_gamut(), grade.gamut);
            frames.push(image);
            progress.done.fetch_add(1, Ordering::Relaxed);
        }
        let first = frames.first().ok_or_else(|| bad(CMD, "no sample frames"))?;
        let (width, height) = (first.w, first.h);
        let mut image = Image::new(width, height.saturating_mul(3));
        for (row, frame) in frames.iter().enumerate() {
            if frame.w != width || frame.h != height {
                return Err(bad(CMD, "sample size changed while balancing"));
            }
            let offset = row.saturating_mul(frame.px.len());
            let end = offset.saturating_add(frame.px.len());
            let target = image.px.get_mut(offset..end).ok_or_else(|| bad(CMD, "sample size exceeds the balancing limit"))?;
            target.copy_from_slice(&frame.px);
        }
        let logs = color_groups::log_rgb(&image);
        if let Some(logs) = logs {
            for k in 0..3 {
                total_logs[k] += logs[k];
            }
            valid_logs = valid_logs.saturating_add(1);
        }
        samples.push(Sample { sequence: *sequence, clip: *clip, image, logs });
    }
    let target_logs = total_logs.map(|v| v / valid_logs.max(1) as f64);
    let base = find_effect("lumetri").ok_or_else(|| bad(CMD, "Color Grading is unavailable"))?.instance();
    let cx = FxCtx {
        t: Tick::ZERO,
        px_scale: 1.0,
        seconds: 0.0,
        timecode: "",
        clip_name: "",
        project: Some(&s.project),
        env: None,
        frame_rate: 24.0,
        gamut: grade.gamut,
        working: grade.working,
    };
    let mut gained = Vec::with_capacity(samples.len());
    let mut targets = Vec::with_capacity(samples.len());
    for sample in &samples {
        let gain = sample.logs.and_then(|logs| color_groups::gains(logs, target_logs)).unwrap_or([1.0; 3]);
        let mut image = sample.image.clone();
        color_groups::apply_gain(&mut image, gain);
        apply(&mut image, &base, &cx);
        let stats = color_match::working_stats_in_gamut(&image, grade.protect_skin, grade.working, grade.gamut);
        if sample.logs.is_some() || valid_logs == 0 {
            targets.push(stats);
        }
        gained.push((gain, image));
    }
    let reference = color_match::average_stats(&targets).ok_or_else(|| bad(CMD, "no reliable average color reference"))?;
    grade.reference = GradeReference { log_rgb: target_logs, bands: reference.bands, chroma: reference.chroma, skin: reference.skin };
    let mut corrections = Vec::with_capacity(samples.len());
    for (index, (sample, (gain, image))) in samples.iter().zip(gained).enumerate() {
        if progress.cancel.load(Ordering::Acquire) {
            return Err(bad(CMD, "cancelled"));
        }
        *progress.status.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = format!("Matching clip {} of {}", index + 1, samples.len());
        let matched =
            if sample.logs.is_some() { color_match::solve_to_stats(&image, &reference, &base, grade.protect_skin, &cx).effect(&base) } else { base.clone() };
        let mut actual = image.clone();
        apply(&mut actual, &matched, &cx);
        let prior = color_match::distance(&color_match::working_stats_in_gamut(&image, grade.protect_skin, grade.working, grade.gamut), &reference);
        let actual_distance = color_match::distance(&color_match::working_stats_in_gamut(&actual, grade.protect_skin, grade.working, grade.gamut), &reference);
        let matched = if actual_distance <= prior + 1e-6 { matched } else { base.clone() };
        let after = actual_distance.min(prior);
        let before = color_match::distance(&color_match::working_stats_in_gamut(&sample.image, grade.protect_skin, grade.working, grade.gamut), &reference);
        let keys = ["wheel_shadows", "wheel_midtones", "wheel_highlights", "wheel_shadows_l", "wheel_midtones_l", "wheel_highlights_l", "saturation"];
        let params = keys.iter().filter_map(|id| matched.params.get(*id).cloned().map(|p| ((*id).to_string(), p))).collect();
        if !before.is_finite() || !after.is_finite() {
            return Err(bad(CMD, "matching produced invalid statistics"));
        }
        corrections.push(Correction { sequence: sample.sequence, clip: sample.clip, gain, params, before, after });
        progress.done.fetch_add(1, Ordering::Relaxed);
    }
    Ok(Plan { group: id, replace_groups, grade, corrections })
}

pub(crate) fn apply_plan(s: &mut Session, plan: Plan) -> Result<Value> {
    if plan.replace_groups && plan.group.is_some() {
        return Err(bad(CMD, "an existing group cannot replace unrelated group stages"));
    }
    if plan.corrections.is_empty() || plan.corrections.len() > MAX_GRADING_GROUP_MEMBERS {
        return Err(bad(CMD, "invalid group correction count"));
    }
    plan.grade.check().map_err(|e| bad(CMD, e))?;
    let mut seen = BTreeSet::new();
    for correction in &plan.corrections {
        if !seen.insert((correction.sequence, correction.clip)) {
            return Err(bad(CMD, "repeated group correction"));
        }
        let link = GradeGroupLink {
            group: plan.group.unwrap_or(1),
            gain_rgb: correction.gain,
            balance: correction.params.clone(),
            distance_before: correction.before,
            distance_after: correction.after,
        };
        if link.balance_effect().is_none() || !correction.before.is_finite() || !correction.after.is_finite() {
            return Err(bad(CMD, "invalid group correction"));
        }
    }
    let count = plan.corrections.len();
    let id = s.edit("Match color group", |project, _| {
        let id = if let Some(id) = plan.group {
            if !project.color_groups.contains_key(&id) {
                return Err(bad(CMD, "the group no longer exists"));
            }
            id
        } else {
            if project.color_groups.len() >= MAX_GRADING_GROUPS || project.next_id >= filmcraft_project::Project::MAX_NEXT_ID {
                return Err(bad(CMD, "project group or identity limit reached"));
            }
            project.alloc_id()
        };
        for correction in &plan.corrections {
            let (_, item) = project
                .sequence_mut(correction.sequence)
                .and_then(|q| q.find_item_mut(correction.clip))
                .ok_or_else(|| bad(CMD, "a group member no longer exists"))?;
            if !plan.replace_groups && item.effects.iter().any(|e| e.color_group.as_ref().is_some_and(|link| link.group != id)) {
                return Err(bad(CMD, "a member now belongs to another group"));
            }
            if plan.replace_groups {
                item.effects.retain(|e| e.effect != "grading_group" && e.color_group.is_none());
            }
            let mut stage = find_effect("grading_group").ok_or_else(|| bad(CMD, "group grading is unavailable"))?.instance();
            stage.color_group = Some(Box::new(GradeGroupLink {
                group: id,
                gain_rgb: correction.gain,
                balance: correction.params.clone(),
                distance_before: correction.before,
                distance_after: correction.after,
            }));
            if let Some(index) = item.effects.iter().position(|e| e.color_group.as_ref().is_some_and(|link| link.group == id)) {
                let target = item.effects.get_mut(index).ok_or_else(|| bad(CMD, "group stage disappeared"))?;
                *target = stage;
            } else {
                item.effects.push(stage);
            }
        }
        project.color_groups.insert(id, Arc::new(plan.grade));
        Ok(id)
    })?;
    Ok(
        json!({"group":id,"members":count,"balanced":true,"baseline":"equal-clip average exposure and color","sharedLook":"lumetri.group.setVibe or lumetri.group.setParam"}),
    )
}
