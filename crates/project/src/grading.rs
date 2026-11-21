//! Persistent color groups. Each member carries an independent, static balancing correction;
//! the group's creative grade is stored once and resolved at render time.
use std::collections::BTreeMap;

use filmcraft_color::{Gamut, WorkingSpace};
use serde::{Deserialize, Serialize};

use crate::{EffectInstance, Param, ParamKind, ParamValue, find_effect};

fn check_parameters(params: &BTreeMap<String, Param>) -> Result<(), String> {
    let definition = find_effect("lumetri").ok_or("Color Grading is unavailable")?;
    if params.len() > definition.params.len() {
        return Err("too many shared grading parameters".into());
    }
    for (id, param) in params {
        let schema = definition.param(id).ok_or("unknown shared grading parameter")?;
        let valid = match (&schema.kind, &param.value) {
            (ParamKind::Float { min, max, .. }, ParamValue::Float(v)) => v.is_finite() && (*min..=*max).contains(v),
            (ParamKind::Bool, ParamValue::Bool(_)) => true,
            (ParamKind::Choice(options), ParamValue::Choice(v)) => (*v as usize) < options.len(),
            (ParamKind::Text, ParamValue::Text(v)) => v.len() <= 4096,
            (ParamKind::Color, ParamValue::Color(v)) => v.iter().all(|x| x.is_finite() && (0.0..=1.0).contains(x)),
            (ParamKind::Wheel, ParamValue::Vec2(v)) => v.x.is_finite() && v.y.is_finite() && v.x.abs() <= 1.0 && v.y.abs() <= 1.0,
            (ParamKind::Curve { hue }, ParamValue::Curve(v)) => {
                (*hue || v.len() >= 2)
                    && v.len() <= 512
                    && v.iter().flatten().all(|x| x.is_finite() && (0.0..=1.0).contains(x))
                    && v.windows(2).all(|p| p[0][0] <= p[1][0])
            }
            _ => false,
        };
        if !valid || !param.keyframes.is_empty() {
            return Err(format!("invalid static shared grading parameter `{id}`"));
        }
    }
    Ok(())
}

pub const MAX_GRADING_GROUPS: usize = 256;
pub const MAX_GRADING_GROUP_MEMBERS: usize = 256;

/// Per-tonal-range color statistics of the equally weighted member clips, before the group look.
/// Numeric data lives in the model so renderer/engine implementation details are not dependencies.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GradeReference {
    /// Equal-clip average log RGB: exposure and white balance are multiplicative in linear light.
    pub log_rgb: [f64; 3],
    pub bands: [[f32; 4]; 3],
    pub chroma: f32,
    pub skin: [f32; 3],
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ColorGradeGroup {
    pub name: String,
    pub enabled: bool,
    /// Preserve the grading space in which the average and the member corrections were made.
    pub working: WorkingSpace,
    pub gamut: Gamut,
    pub protect_skin: bool,
    pub reference: GradeReference,
    pub look: EffectInstance,
    pub look_strength: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vibe: Option<VibeControls>,
}

/// Plain-language adjustments are offsets from a fixed base, never the last rendered result.
/// Advanced edits replace that base with the resulting custom look on the next Vibes adjustment.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VibeControls {
    pub style: Option<String>,
    pub base: BTreeMap<String, Param>,
    pub brightness: f64,
    pub warmth: f64,
    pub color: f64,
    pub drama: f64,
    pub softness: f64,
}

/// Embedded in a `grading_group` effect. Copies inherit the group and their correction; moving
/// a clip to another track or sequence retains both without an identity-indexed membership list.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GradeGroupLink {
    pub group: u64,
    pub gain_rgb: [f32; 3],
    pub balance: BTreeMap<String, Param>,
    pub distance_before: f32,
    pub distance_after: f32,
}

impl GradeGroupLink {
    pub fn check(&self) -> Result<(), String> {
        if self.balance.len() > 7 || !self.gain_rgb.iter().all(|g| g.is_finite() && (1.0 / 32.0..=32.0).contains(g)) {
            return Err("invalid automatic exposure/color gains".into());
        }
        for (id, param) in &self.balance {
            let valid = match (id.as_str(), &param.value) {
                ("wheel_shadows" | "wheel_midtones" | "wheel_highlights", ParamValue::Vec2(p)) => {
                    p.x.is_finite() && p.y.is_finite() && p.x.abs() <= 1.0 && p.y.abs() <= 1.0
                }
                ("wheel_shadows_l" | "wheel_midtones_l" | "wheel_highlights_l", ParamValue::Float(v)) => v.is_finite() && v.abs() <= 100.0,
                ("saturation", ParamValue::Float(v)) => v.is_finite() && (0.0..=200.0).contains(v),
                _ => false,
            };
            if !valid || !param.keyframes.is_empty() {
                return Err("invalid automatic tonal/color correction".into());
            }
        }
        if !self.distance_before.is_finite() || !self.distance_after.is_finite() {
            return Err("invalid color matching statistics".into());
        }
        Ok(())
    }

    pub fn balance_effect(&self) -> Option<EffectInstance> {
        self.check().ok()?;
        // Start with neutral defaults before applying the bounded, static solver parameters.
        let mut effect = find_effect("lumetri")?.instance();
        effect.params.extend(self.balance.iter().map(|(id, param)| (id.clone(), param.clone())));
        Some(effect)
    }
}

impl ColorGradeGroup {
    pub fn check(&self) -> Result<(), String> {
        if self.name.trim().is_empty() || self.name.len() > 512 {
            return Err("a grading group needs a name of at most 512 bytes".into());
        }
        if self.look.effect != "lumetri" || self.look.color_group.is_some() || !self.look.masks.is_empty() {
            return Err("a grading group's shared look must be unmasked Color Grading".into());
        }
        if self.look.params.len() > 128 || self.look.params.values().any(|p| !p.keyframes.is_empty()) {
            return Err("a grading group's shared look must use bounded, static parameters".into());
        }
        check_parameters(&self.look.params)?;
        if !matches!(self.gamut, Gamut::Bt709 | Gamut::Bt2020) || (self.working.is_hdr() && self.gamut != Gamut::Bt2020) {
            return Err("unsupported grading group working gamut".into());
        }
        if !self.look_strength.is_finite() || !(0.0..=1.0).contains(&self.look_strength) {
            return Err("a grading group's look strength must be between 0 and 1".into());
        }
        if let Some(vibe) = &self.vibe {
            check_parameters(&vibe.base)?;
            if vibe.style.as_ref().is_some_and(|id| id.len() > 64) {
                return Err("invalid Vibes style identity".into());
            }
            if vibe.base.len() > 128
                || vibe.base.values().any(|p| !p.keyframes.is_empty())
                || ![vibe.brightness, vibe.warmth, vibe.color, vibe.drama, vibe.softness].iter().all(|v| v.is_finite() && v.abs() <= 1.0)
            {
                return Err("a grading group's Vibes controls are outside their supported range".into());
            }
        }
        if !self.reference.bands.iter().flatten().chain(std::iter::once(&self.reference.chroma)).chain(&self.reference.skin).all(|v| v.is_finite()) {
            return Err("a grading group's reference contains non-finite statistics".into());
        }
        if !self.reference.log_rgb.iter().all(|v| v.is_finite() && v.abs() <= 64.0) {
            return Err("a grading group's RGB reference is invalid".into());
        }
        Ok(())
    }
}

impl crate::Project {
    pub fn check_color_groups(&self) -> Result<(), String> {
        if self.color_groups.len() > MAX_GRADING_GROUPS {
            return Err(format!("a project supports at most {MAX_GRADING_GROUPS} grading groups"));
        }
        for (id, group) in &self.color_groups {
            if *id == 0 || *id > crate::Project::MAX_NEXT_ID {
                return Err("invalid grading group identity".into());
            }
            group.check()?;
        }
        for item in self.sequences() {
            let Some(sequence) = self.sequence(item.id) else { continue };
            for track in sequence.all_tracks() {
                for clip in &track.items {
                    let mut count = 0usize;
                    for effect in &clip.effects {
                        match (effect.effect.as_str(), effect.color_group.as_deref()) {
                            ("grading_group", Some(link)) if track.kind == crate::TrackKind::Video => {
                                count = count.saturating_add(1);
                                if count > 1
                                    || !self.color_groups.contains_key(&link.group)
                                    || link.check().is_err()
                                    || !link.distance_before.is_finite()
                                    || !link.distance_after.is_finite()
                                {
                                    return Err(format!("clip '{}' has invalid color grading group data", clip.name));
                                }
                            }
                            ("grading_group", _) | (_, Some(_)) => return Err(format!("clip '{}' has an invalid grading group stage", clip.name)),
                            _ => {}
                        }
                    }
                }
            }
        }
        Ok(())
    }
}
