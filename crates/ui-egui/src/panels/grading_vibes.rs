//! Beginner grading: match the selected footage, choose a mood, then adjust a shared look.
use std::sync::Arc;

use egui::{Align2, Color32, Rect, Sense, Stroke, StrokeKind, pos2, vec2};
use filmcraft_project::{ClipId, ColorGradeGroup, TrackItem, TrackKind, find_effect};
use serde_json::{Value, json};

use crate::FilmcraftApp;
use crate::theme::Tokens;

type Actions = Vec<(String, Value)>;

pub(super) fn group_for(app: &FilmcraftApp, item: &TrackItem) -> Option<(u64, Arc<ColorGradeGroup>)> {
    item.effects
        .iter()
        .filter_map(|e| e.color_group.as_ref())
        .find_map(|link| app.session.project.color_groups.get(&link.group).map(|g| (link.group, g.clone())))
}

pub(super) fn matching(app: &FilmcraftApp) -> bool {
    #[cfg(not(target_arch = "wasm32"))]
    {
        app.session.backend.color_matching_state(app.session.document_epoch).0
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = app;
        false
    }
}

pub(super) fn matching_error(app: &FilmcraftApp) -> Option<String> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        app.session.backend.color_matching_state(app.session.document_epoch).1
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = app;
        None
    }
}

pub(super) fn mode_bar(app: &mut FilmcraftApp, ui: &mut egui::Ui) {
    let advanced = app.session.prefs.color.grading_advanced;
    let width = ((ui.available_width() - ui.spacing().item_spacing.x).max(0.0) * 0.5).min(100.0);
    ui.horizontal(|ui| {
        for (id, label, value) in [("vibes", tl!("Vibes"), false), ("advanced", tl!("Advanced"), true)] {
            let response = ui.add(egui::Button::new(label).selected(advanced == value).min_size(vec2(width, 25.0)));
            app.auto.add_response(&format!("lumetri.view.{id}"), &response, label);
            if response.clicked()
                && let Err(error) = app.session.execute("prefs.set", json!({"key":"color.gradingAdvanced","value":value}))
            {
                app.ui.status = error.to_string();
            }
        }
    });
}

fn selected(app: &FilmcraftApp) -> Vec<(ClipId, Option<u64>)> {
    let Some(sequence) = app.session.active_sequence() else { return Vec::new() };
    app.session
        .state
        .selection
        .iter()
        .take(513)
        .filter_map(|clip| {
            let (track, item) = sequence.find_item(*clip)?;
            if sequence.track(track)?.kind != TrackKind::Video {
                return None;
            }
            Some((*clip, group_for(app, item).map(|g| g.0)))
        })
        .collect()
}

pub(super) fn show(app: &mut FilmcraftApp, ui: &mut egui::Ui, rect: Rect) {
    let tokens = app.tokens;
    let mut body = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink2(vec2(12.0, 6.0))).id_salt("grading-vibes"));
    let clips = selected(app);
    if clips.is_empty() {
        body.heading(tl!("Give your scene a feeling"));
        body.label(tl!("Select one or more video clips in the timeline. Match them, then choose a mood."));
        return;
    }
    if app.session.state.selection.len() > 512 || clips.len() > filmcraft_project::MAX_GRADING_GROUP_MEMBERS {
        body.label(tl!("Select up to 256 video clips at a time for automatic matching."));
        return;
    }
    let first_group = clips.first().and_then(|clip| clip.1);
    let common = first_group.filter(|id| clips.iter().all(|clip| clip.1 == Some(*id)));
    let group = common.and_then(|id| app.session.project.color_groups.get(&id).cloned().map(|g| (id, g)));
    let selected_ids = clips.iter().map(|clip| clip.0.0).collect::<Vec<_>>();
    let replace = clips.iter().any(|clip| clip.1.is_some());
    let mut actions = Actions::new();
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(&mut body, |ui| {
        // The floating scrollbar must not clip a maximum-value handle or right-aligned labels.
        ui.set_max_width((ui.available_width() - 12.0).max(1.0));
        if let Some((id, group)) = &group {
            group_header(app, ui, *id, group, &tokens, &mut actions);
        } else {
            ui.label(egui::RichText::new(tl!("Start with a consistent look")).strong().size(15.0));
            ui.label(egui::RichText::new(tl!("Balance the light and color automatically, then give the whole scene a mood.")).color(tokens.text_dim));
            let label = if clips.len() == 1 { tl!("Match this clip").to_string() } else { tlf!("Match {count} selected clips", count = clips.len()) };
            let response = ui.button(label);
            app.auto.add_response("lumetri.group.create", &response, "Match selected clips");
            if response.clicked() {
                actions.push(("lumetri.group.create".into(), json!({"clips":selected_ids,"replaceGroups":replace})));
            }
            if replace {
                ui.label(egui::RichText::new(tl!("This replaces the selected clips' previous group looks.")).small().color(tokens.text_dim));
            }
            if !app.session.project.color_groups.is_empty() {
                let menu = ui.menu_button(tl!("Join an existing look group"), |ui| {
                    for (id, existing) in &app.session.project.color_groups {
                        let response = ui.button(&existing.name);
                        app.auto.add_response(&format!("lumetri.group.join.{id}"), &response, &existing.name);
                        if response.clicked() {
                            actions.push(("lumetri.group.addClips".into(), json!({"group":id,"clips":selected_ids})));
                            ui.close();
                        }
                    }
                });
                app.auto.add_response("lumetri.group.join", &menu.response, "Join an existing look group");
            }
        }
        ui.add_space(10.0);
        ui.label(egui::RichText::new(tl!("Choose a mood")).strong());
        let active = group.as_ref().and_then(|(_, g)| g.vibe.as_ref()).and_then(|v| v.style.as_deref());
        cards(app, ui, &filmcraft_render::vibes::STYLES[..4], active, &mut actions, common, &selected_ids, replace);
        let extra = egui::CollapsingHeader::new(tl!("More moods")).id_salt("vibes-more-moods").show(ui, |ui| {
            cards(app, ui, &filmcraft_render::vibes::STYLES[4..], active, &mut actions, common, &selected_ids, replace);
        });
        app.auto.add_response("lumetri.vibe.more", &extra.header_response, "More moods");
        ui.add_space(8.0);
        if let Some((id, group)) = &group {
            if group.vibe.as_ref().is_none_or(|vibe| vibe.style.is_none()) {
                ui.label(egui::RichText::new(tl!("Custom look — these controls fine-tune what you already have.")).small().color(tokens.text_dim));
            }
            knob(app, ui, "strength", tl!("Look strength"), tl!("Subtle"), tl!("Strong"), f64::from(group.look_strength) * 100.0, 0.0, *id, &mut actions);
            let vibe = group.vibe.as_ref();
            knob(
                app,
                ui,
                "brightness",
                tl!("Brightness"),
                tl!("Darker"),
                tl!("Brighter"),
                vibe.map_or(0.0, |v| v.brightness) * 100.0,
                -100.0,
                *id,
                &mut actions,
            );
            knob(app, ui, "warmth", tl!("Warmth"), tl!("Cooler"), tl!("Warmer"), vibe.map_or(0.0, |v| v.warmth) * 100.0, -100.0, *id, &mut actions);
            ui.add_enabled_ui(active != Some("black-white"), |ui| {
                knob(app, ui, "color", tl!("Color"), tl!("Muted"), tl!("Colorful"), vibe.map_or(0.0, |v| v.color) * 100.0, -100.0, *id, &mut actions);
            });
            let tune = egui::CollapsingHeader::new(tl!("Fine-tune the feeling")).id_salt("vibes-fine-tune").show(ui, |ui| {
                knob(app, ui, "drama", tl!("Drama"), tl!("Gentle"), tl!("Dramatic"), vibe.map_or(0.0, |v| v.drama) * 100.0, -100.0, *id, &mut actions);
                knob(app, ui, "softness", tl!("Texture"), tl!("Crisp"), tl!("Soft"), vibe.map_or(0.0, |v| v.softness) * 100.0, -100.0, *id, &mut actions);
            });
            app.auto.add_response("lumetri.vibe.fineTune", &tune.header_response, "Fine-tune the feeling");
            let reset = ui.button(tl!("Reset adjustments"));
            app.auto.add_response("lumetri.vibe.resetAdjustments", &reset, "Reset adjustments");
            if reset.clicked() {
                actions.push(("lumetri.group.setVibe".into(), json!({"group":id,"brightness":0,"warmth":0,"color":0,"drama":0,"softness":0})));
            }
            ui.label(
                egui::RichText::new(tl!("Matching stays intact when you change the mood. Advanced offers the detailed controls."))
                    .small()
                    .color(tokens.text_dim),
            );
        } else {
            ui.label(egui::RichText::new(tl!("Picking a mood also matches the selected clips for you.")).small().color(tokens.text_dim));
        }
        ui.add_space(12.0);
    });
    crate::panels::effect_controls::run(app, ui.ctx(), actions);
}

pub(super) fn group_header(app: &mut FilmcraftApp, ui: &mut egui::Ui, id: u64, group: &ColorGradeGroup, tokens: &Tokens, actions: &mut Actions) {
    let count = filmcraft_engine::grading_groups::members(&app.session.project, id).count();
    ui.label(egui::RichText::new(&group.name).strong().size(15.0));
    let description = if count == 1 { tl!("One look for this clip").to_string() } else { tlf!("One look, shared by {count} clips", count = count) };
    ui.label(egui::RichText::new(description).color(tokens.text_dim));
    ui.horizontal_wrapped(|ui| {
        let mut enabled = group.enabled;
        let response = ui.checkbox(&mut enabled, tl!("Apply look"));
        app.auto.add_response("lumetri.group.enabled", &response, "Apply group look");
        if response.changed() {
            actions.push(("lumetri.group.setEnabled".into(), json!({"group":id,"enabled":enabled})));
        }
        let select = ui.button(tl!("Select clips"));
        app.auto.add_response("lumetri.group.select", &select, "Select group clips");
        if select.clicked() {
            let sequence = app.session.state.active_sequence;
            let ids = filmcraft_engine::grading_groups::members(&app.session.project, id)
                .filter(|(seq, _)| Some(*seq) == sequence)
                .take(256)
                .map(|(_, clip)| clip.0)
                .collect::<Vec<_>>();
            actions.push(("timeline.select".into(), json!({"clips":ids})));
        }
        let response = ui.button(tl!("Match again"));
        app.auto.add_response("lumetri.group.balance", &response, "Match group again");
        if response.clicked() {
            actions.push(("lumetri.group.balance".into(), json!({"group":id})));
        }
    });
    let menu = ui.menu_button(tl!("Group options"), |ui| {
        let draft_id = ui.scope_id().with(("group-name", id));
        let mut name = ui.ctx().data_mut(|data| data.get_temp::<String>(draft_id)).unwrap_or_else(|| group.name.clone());
        ui.label(tl!("Group name"));
        let edit = ui.add(egui::TextEdit::singleline(&mut name).char_limit(128));
        app.auto.add_response("lumetri.group.name", &edit, "Group name");
        ui.ctx().data_mut(|data| data.insert_temp(draft_id, name.clone()));
        let save = ui.add_enabled(!name.trim().is_empty() && name != group.name, egui::Button::new(tl!("Save name")));
        app.auto.add_response("lumetri.group.rename", &save, "Save group name");
        if save.clicked() {
            actions.push(("lumetri.group.rename".into(), json!({"group":id,"name":name})));
            ui.close();
        }
        ui.separator();
        let add = ui.button(tl!("Match selected clips into this group"));
        app.auto.add_response("lumetri.group.add", &add, "Match selected clips into this group");
        if add.clicked() {
            actions.push(("lumetri.group.addClips".into(), json!({"group":id})));
            ui.close();
        }
        let remove = ui.button(tl!("Remove group grading from selected clips"));
        app.auto.add_response("lumetri.group.remove", &remove, "Remove group grading from selected clips");
        if remove.clicked() {
            actions.push(("lumetri.group.removeClips".into(), json!({"group":id})));
            ui.close();
        }
        ui.separator();
        let delete =
            ui.button(tl!("Remove this group from all its clips")).on_hover_text(tl!("Keeps the clips and their original effects. Undo restores the group."));
        app.auto.add_response("lumetri.group.delete", &delete, "Remove this group from all its clips");
        if delete.clicked() {
            actions.push(("lumetri.group.delete".into(), json!({"group":id})));
            ui.close();
        }
    });
    app.auto.add_response("lumetri.group.options", &menu.response, "Group options");
}

#[allow(clippy::too_many_arguments)]
fn cards(
    app: &mut FilmcraftApp,
    ui: &mut egui::Ui,
    styles: &[filmcraft_render::vibes::VibeStyle],
    active: Option<&str>,
    actions: &mut Actions,
    group: Option<u64>,
    clips: &[u64],
    replace: bool,
) {
    let columns = if ui.available_width() >= 250.0 { 2 } else { 1 };
    let width = ((ui.available_width().max(1.0) - 8.0 * (columns - 1) as f32) / columns as f32).max(1.0);
    for row in styles.chunks(columns) {
        ui.horizontal(|ui| {
            for style in row {
                let height = width * 0.5 + 27.0;
                let (rect, response) = ui.allocate_exact_size(vec2(width, height), Sense::click());
                let chosen = active == Some(style.id);
                let fill = if chosen { ui.visuals().selection.bg_fill } else { app.tokens.panel_bg };
                ui.painter().rect_filled(rect, 4.0, fill);
                if let Some(texture) = thumbnail(app, ui.ctx(), style) {
                    let picture = Rect::from_min_max(rect.min + vec2(4.0, 4.0), pos2(rect.max.x - 4.0, rect.max.y - 25.0));
                    ui.painter().image(texture, picture, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
                }
                ui.painter().text(
                    pos2(rect.center().x, rect.max.y - 13.0),
                    Align2::CENTER_CENTER,
                    crate::i18n::t(style.name),
                    Tokens::ui(11.0),
                    app.tokens.text,
                );
                ui.painter().rect_stroke(
                    rect,
                    4.0,
                    Stroke::new(if chosen { 1.5 } else { 0.5 }, if chosen { app.tokens.hot_text } else { app.tokens.text_faint }),
                    StrokeKind::Inside,
                );
                app.auto.add_response(&format!("lumetri.vibe.style.{}", style.id), &response, style.name);
                if response.clicked() {
                    actions.push(if let Some(id) = group {
                        ("lumetri.group.setVibe".into(), json!({"group":id,"style":style.id}))
                    } else {
                        ("lumetri.group.create".into(), json!({"clips":clips,"vibe":style.id,"replaceGroups":replace}))
                    });
                }
                response.on_hover_text(crate::i18n::t(style.description));
            }
        });
        ui.add_space(6.0);
    }
}

fn thumbnail(app: &mut FilmcraftApp, ctx: &egui::Context, style: &filmcraft_render::vibes::VibeStyle) -> Option<egui::TextureId> {
    if let Some((id, _)) = app.texture_existing(&format!("lumetri-preset-{}-96", style.name)) {
        return Some(id);
    }
    let effect = filmcraft_render::vibes::style(style.id)?;
    let def = find_effect("lumetri")?;
    let params = def.params.iter().filter_map(|p| effect.params.get(p.id).map(|value| (p.id, value.value.clone()))).collect();
    let preset = filmcraft_render::lumetri_presets::LumetriPreset { folder: "Vibes", name: style.name, description: style.description, params };
    Some(super::effects::preset_texture(app, ctx, &preset, 96))
}

#[allow(clippy::too_many_arguments)]
fn knob(app: &mut FilmcraftApp, ui: &mut egui::Ui, id: &str, label: &str, left: &str, right: &str, value: f64, min: f64, group: u64, actions: &mut Actions) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(label).strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(egui::RichText::new(format!("{value:.0}")).color(app.tokens.text_dim));
        });
    });
    let mut next = value;
    let slider = ui
        .scope(|ui| {
            ui.spacing_mut().slider_width = ui.available_width().max(1.0);
            ui.add(egui::Slider::new(&mut next, min..=100.0).show_value(false))
        })
        .inner;
    app.auto.add_response(&format!("lumetri.vibe.{id}"), &slider, label);
    if slider.changed() {
        let mut p = json!({"group":group});
        p[id] = json!(next.round());
        actions.push(("lumetri.group.setVibe".into(), p));
    }
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(left).small().color(app.tokens.text_dim));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(egui::RichText::new(right).small().color(app.tokens.text_dim));
        });
    });
    ui.add_space(4.0);
}
