//! Original, plain-language starting looks and non-accumulating beginner adjustments.
use filmcraft_project::{EffectInstance, ParamValue, VibeControls, find_effect};
use serde::Serialize;

#[derive(Clone, Copy, Debug, Serialize)]
pub struct VibeStyle {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
}

pub const STYLES: [VibeStyle; 8] = [
    VibeStyle { id: "natural", name: "Clean & natural", description: "True-to-life color without a strong mood." },
    VibeStyle { id: "warm-cozy", name: "Warm & cozy", description: "Golden warmth and gentle contrast." },
    VibeStyle { id: "cool-tense", name: "Cold & tense", description: "Cool colors and deeper shadows." },
    VibeStyle { id: "nostalgic", name: "Nostalgic film", description: "Soft detail, faded shadows and a little warmth." },
    VibeStyle { id: "bold", name: "Bold & cinematic", description: "Rich color, deeper contrast and warm highlights." },
    VibeStyle { id: "dark-gritty", name: "Dark & gritty", description: "A darker, less colorful, harder-edged look." },
    VibeStyle { id: "bright-airy", name: "Bright & airy", description: "Lifted light and open, gentle shadows." },
    VibeStyle { id: "black-white", name: "Classic black & white", description: "A clean monochrome look with stronger contrast." },
];

pub fn style(id: &str) -> Option<EffectInstance> {
    use ParamValue::{Choice as C, Float as F};
    let mut effect = find_effect("lumetri")?.instance();
    let params = match id {
        "natural" => vec![],
        "warm-cozy" => vec![("look", C(7)), ("look_intensity", F(35.0)), ("temperature", F(10.0)), ("contrast", F(-5.0)), ("shadows", F(10.0))],
        "cool-tense" => vec![
            ("look", C(3)),
            ("look_intensity", F(40.0)),
            ("temperature", F(-10.0)),
            ("contrast", F(10.0)),
            ("shadows", F(-10.0)),
            ("creative_sat", F(95.0)),
        ],
        "nostalgic" => vec![("look", C(2)), ("look_intensity", F(35.0)), ("faded_film", F(25.0)), ("sharpen", F(-15.0)), ("creative_sat", F(90.0))],
        "bold" => vec![("look", C(1)), ("look_intensity", F(30.0)), ("contrast", F(15.0)), ("vibrance", F(20.0))],
        "dark-gritty" => vec![
            ("look", C(4)),
            ("look_intensity", F(30.0)),
            ("exposure", F(-0.3)),
            ("contrast", F(25.0)),
            ("creative_sat", F(80.0)),
            ("vignette_amount", F(-0.75)),
        ],
        "bright-airy" => {
            vec![("exposure", F(0.25)), ("contrast", F(-10.0)), ("shadows", F(15.0)), ("faded_film", F(5.0)), ("whites", F(-10.0)), ("creative_sat", F(95.0))]
        }
        "black-white" => vec![("saturation", F(0.0)), ("contrast", F(10.0)), ("blacks", F(-5.0))],
        _ => return None,
    };
    for (id, value) in params {
        effect.params.get_mut(id)?.value = value;
    }
    Some(effect)
}

pub fn controls(base: &EffectInstance, style: Option<&str>) -> VibeControls {
    VibeControls { style: style.map(str::to_string), base: base.params.clone(), brightness: 0.0, warmth: 0.0, color: 0.0, drama: 0.0, softness: 0.0 }
}

/// Every gesture is recomputed from the saved base. Moving a knob out and back restores the
/// exact starting grade, and switching to Advanced keeps the same real Color Grading values.
pub fn build(vibe: &VibeControls) -> Option<EffectInstance> {
    let values = [vibe.brightness, vibe.warmth, vibe.color, vibe.drama, vibe.softness];
    if !values.iter().all(|v| v.is_finite() && v.abs() <= 1.0) || vibe.base.len() > 128 {
        return None;
    }
    let mut effect = find_effect("lumetri")?.instance();
    for (id, param) in &vibe.base {
        if !param.keyframes.is_empty() || !effect.params.contains_key(id) {
            return None;
        }
        effect.params.insert(id.clone(), param.clone());
    }
    // A beginner adjustment must remain visible even if Advanced disabled its section.
    // Start that section at neutral rather than reviving unrelated, previously bypassed settings.
    let defaults = find_effect("lumetri")?.instance();
    let basic_changed = vibe.brightness != 0.0 || vibe.warmth != 0.0 || vibe.drama != 0.0;
    let creative_changed = vibe.color != 0.0 || vibe.softness != 0.0;
    for (switch, section, changed) in [("basic_on", "Basic Correction", basic_changed), ("creative_on", "Creative", creative_changed)] {
        if changed && effect.param(switch)?.value.as_bool() == Some(false) {
            for definition in &find_effect("lumetri")?.params {
                if definition.group == Some(section) {
                    effect.params.insert(definition.id.to_string(), defaults.params.get(definition.id)?.clone());
                }
            }
            effect.params.get_mut(switch)?.value = ParamValue::Bool(true);
        }
    }
    for (id, delta, lo, hi) in [
        ("exposure", vibe.brightness * 1.5, -5.0, 5.0),
        ("temperature", vibe.warmth * 40.0, -100.0, 100.0),
        ("contrast", vibe.drama * 30.0, -100.0, 100.0),
        ("shadows", -vibe.drama * 10.0, -100.0, 100.0),
        ("sharpen", -vibe.softness * 40.0, -100.0, 100.0),
    ] {
        let param = effect.params.get_mut(id)?;
        let base = param.value.as_f64()?;
        if !base.is_finite() {
            return None;
        }
        param.value = ParamValue::Float((base + delta).clamp(lo, hi));
    }
    let saturation = effect.params.get_mut("creative_sat")?;
    let base = saturation.value.as_f64()?;
    if !base.is_finite() {
        return None;
    }
    saturation.value = ParamValue::Float((base * (1.0 + vibe.color * 0.75)).clamp(0.0, 200.0));
    Some(effect)
}

#[cfg(test)]
mod tests {
    use super::{STYLES, build, controls, style};

    #[test]
    fn grading_vibes_keep_their_base_when_a_control_returns_to_zero() {
        for descriptor in STYLES {
            let base = style(descriptor.id).unwrap();
            let mut knobs = controls(&base, Some(descriptor.id));
            assert_eq!(build(&knobs).unwrap(), base);
            knobs.warmth = 0.75;
            assert_ne!(build(&knobs).unwrap(), base);
            knobs.warmth = 0.0;
            assert_eq!(build(&knobs).unwrap(), base, "{} accumulated a previous edit", descriptor.id);
            knobs.brightness = f64::NAN;
            assert!(build(&knobs).is_none());
        }
        assert!(style("unknown").is_none());
    }

    #[test]
    fn grading_vibes_adjustments_activate_bypassed_sections_without_reviving_old_settings() {
        use filmcraft_project::ParamValue;
        let mut base = style("natural").unwrap();
        for (key, value) in [
            ("basic_on", ParamValue::Bool(false)),
            ("creative_on", ParamValue::Bool(false)),
            ("exposure", ParamValue::Float(4.0)),
            ("temperature", ParamValue::Float(80.0)),
            ("creative_sat", ParamValue::Float(0.0)),
        ] {
            base.params.get_mut(key).unwrap().value = value;
        }
        let mut knobs = controls(&base, None);
        knobs.brightness = 0.5;
        knobs.color = 0.5;
        let grade = build(&knobs).unwrap();
        assert_eq!(grade.param("basic_on").unwrap().value, ParamValue::Bool(true));
        assert_eq!(grade.param("creative_on").unwrap().value, ParamValue::Bool(true));
        assert_eq!(grade.f64_at("exposure", filmcraft_time::Tick::ZERO), 0.75);
        assert_eq!(grade.f64_at("temperature", filmcraft_time::Tick::ZERO), 0.0);
        assert_eq!(grade.f64_at("creative_sat", filmcraft_time::Tick::ZERO), 137.5);
        knobs.brightness = 0.0;
        knobs.color = 0.0;
        assert_eq!(build(&knobs).unwrap(), base);
    }
}
