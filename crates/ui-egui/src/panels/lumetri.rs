//! Color Grading panel: Basic Correction, Creative, Curves (RGB + hue curves), Color Wheels,
//! HSL Secondary, Vignette — editing the selected clip's Color Grading effect through engine commands.
//!
//! In an HDR sequence (Rec. 2100 PQ / HLG working space) Basic Correction shows HDR White and
//! HDR Specular and Curves shows HDR Range (cd/m²): the sliders and curves then span 0 … that
//! many nits (see `filmcraft_color::grade`). Sliders, wheels and choice controls register
//! `lumetri.param.<id>`; RGB/hue curve editors register `lumetri.curve.<id>`.
//! Color Match: `lumetri.match.comparisonView` (toggles the Program monitor's
//! Comparison View, whose reference frame Apply Match then uses).

use egui::{Align2, Color32, Pos2, Rect, Sense, Stroke, StrokeKind, pos2, vec2};
use filmcraft_project::{ClipId, EffectInstance, ParamValue};
use filmcraft_time::Tick;
use serde_json::{Value, json};

use crate::FilmcraftApp;
use crate::icons::{self, Icon};
use crate::native_dialogs::FileDialog;
use crate::theme::Tokens;
use crate::widgets::HotNumberOptions;

type Actions = Vec<Value>;

fn selected(app: &FilmcraftApp) -> Option<(ClipId, filmcraft_project::TrackItem)> {
    let seq = app.session.active_sequence()?;
    app.session.state.selection.iter().find_map(|c| {
        let (tid, it) = seq.find_item(*c)?;
        (seq.track(tid)?.kind == filmcraft_project::TrackKind::Video).then(|| (*c, it.clone()))
    })
}

pub fn show(app: &mut FilmcraftApp, ui: &mut egui::Ui, rect: Rect) {
    let header = Rect::from_min_max(rect.min + vec2(12.0, 4.0), pos2(rect.max.x - 12.0, rect.min.y + 34.0));
    let mut modes = ui.new_child(egui::UiBuilder::new().max_rect(header).id_salt("color-grade-mode"));
    super::grading_vibes::mode_bar(app, &mut modes);
    let mut body = Rect::from_min_max(pos2(rect.min.x, rect.min.y + 38.0), rect.max);
    let busy = super::grading_vibes::matching(app);
    let match_error = super::grading_vibes::matching_error(app);
    if busy || match_error.is_some() {
        let mut status = ui.new_child(egui::UiBuilder::new().max_rect(body.shrink2(vec2(12.0, 0.0))).id_salt("color-matching-progress"));
        let response = if let Some(error) = match_error {
            status.colored_label(ui.visuals().error_fg_color, tlf!("Matching stopped: {error}", error))
        } else {
            status.label(tl!("Matching clips in the background…"))
        };
        app.auto.add_response("lumetri.group.status", &response, "Color matching status");
        body.min.y += response.rect.height() + 5.0;
    }
    if body.height() <= 0.0 {
        return;
    }
    let mut content = ui.new_child(egui::UiBuilder::new().max_rect(body).id_salt("color-grade-view"));
    content.add_enabled_ui(!busy, |ui| {
        if app.session.prefs.color.grading_advanced {
            show_advanced(app, ui, body);
        } else {
            super::grading_vibes::show(app, ui, body);
        }
    });
}

fn show_advanced(app: &mut FilmcraftApp, ui: &mut egui::Ui, rect: Rect) {
    let t = app.tokens;
    let Some((clip, it)) = selected(app) else {
        crate::dock::placeholder(ui, rect, &t, tl!("(no clip selected)"));
        return;
    };
    let mut bui = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink2(vec2(12.0, 6.0))).id_salt("lumetri2"));
    let group = super::grading_vibes::group_for(app, &it);
    let idx = if group.is_some() { Some(0) } else { it.effects.iter().position(|e| e.effect == "lumetri") };
    let Some(idx) = idx else {
        bui.add_space(8.0);
        bui.label(egui::RichText::new(tlf!("Master · {name}", name = it.name)).color(t.text_dim));
        bui.add_space(8.0);
        let add = bui.button(tl!("Add Color Grading to clip"));
        app.auto.add_response("lumetri.add", &add, "Add Color Grading to clip");
        if add.clicked()
            && let Err(error) = app.session.execute("effects.apply", json!({"clips": [clip.0], "effect": "lumetri"}))
        {
            app.ui.status = error.to_string();
        }
        return;
    };
    let e = group.as_ref().map(|(_, g)| g.look.clone()).or_else(|| it.effects.get(idx).cloned());
    let Some(e) = e else {
        return;
    };
    // HDR mode: the sequence's working space is Rec. 2100 PQ or HLG
    let hdr = group.as_ref().map_or_else(|| app.session.active_sequence().is_some_and(|q| q.settings.color.working.is_hdr()), |(_, g)| g.working.is_hdr());
    let mut auto_rows: Vec<(String, Rect, &'static str)> = Vec::new();
    let ph = app.session.playhead();
    let mt = if group.is_some() { Tick::ZERO } else { it.effect_time_at(ph.clamp(it.start, (it.end() - Tick(1)).max(it.start))) };
    let mut actions: Actions = Vec::new();
    let mut group_actions = Vec::new();
    if let Some((id, grade)) = &group {
        super::grading_vibes::group_header(app, &mut bui, *id, grade, &t, &mut group_actions);
    } else {
        // header: clip context + fx toggle
        // the fx toggle is laid out first so a long clip name is truncated instead of widening the panel
        bui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let fx = ui.add(egui::Button::new(egui::RichText::new("fx").italics().color(if e.enabled { t.text } else { t.text_faint })).frame(false));
                app.auto.add_response("lumetri.enabled", &fx, "Toggle clip Color Grading");
                if fx.clicked()
                    && let Err(error) = app.session.execute("effects.toggleEnabled", json!({"clip": clip.0, "index": idx}))
                {
                    app.ui.status = error.to_string();
                }
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.add(egui::Label::new(egui::RichText::new(tlf!("Master · {name}", name = it.name)).color(t.text_dim)).truncate());
                });
            });
        });
    }
    let set = |actions: &mut Actions, param: &str, v: Value| actions.push(json!({"clip": clip.0, "effect": idx, "param": param, "value": v}));
    let mut sections: Vec<&'static str> = Vec::new();
    let mut lut_actions: Vec<(&'static str, Value)> = Vec::new();
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(&mut bui, |ui| {
        if open_section(ui, app, &e, "Basic Correction", "basic_on", "basic", &mut sections) {
            lut_combo(ui, app, &e, "input_lut", tl!("Input LUT"), true, &mut lut_actions);
            sub(ui, &t, tl!("Color"));
            slider(ui, &e, mt, "temperature", tl!("Temperature"), Some(Gradient::Temp), &t, &mut actions, clip, idx, &mut auto_rows);
            slider(ui, &e, mt, "tint", tl!("Tint"), Some(Gradient::Tint), &t, &mut actions, clip, idx, &mut auto_rows);
            slider(ui, &e, mt, "saturation", tl!("Saturation"), None, &t, &mut actions, clip, idx, &mut auto_rows);
            sub(ui, &t, tl!("Light"));
            for (id, label) in [
                ("exposure", tl!("Exposure")),
                ("contrast", tl!("Contrast")),
                ("highlights", tl!("Highlights")),
                ("shadows", tl!("Shadows")),
                ("whites", tl!("Whites")),
                ("blacks", tl!("Blacks")),
            ] {
                slider(ui, &e, mt, id, label, None, &t, &mut actions, clip, idx, &mut auto_rows);
            }
            if hdr {
                sub(ui, &t, tl!("HDR"));
                for (id, label) in [("hdr_white", tl!("HDR White")), ("hdr_specular", tl!("HDR Specular"))] {
                    slider(ui, &e, mt, id, label, None, &t, &mut actions, clip, idx, &mut auto_rows);
                }
            }
        }
        if open_section(ui, app, &e, "Creative", "creative_on", "creative", &mut sections) {
            let look = match e.param("look").map(|p| &p.value) {
                Some(ParamValue::Choice(c)) => *c as usize,
                _ => 0,
            };
            let opts = filmcraft_project::find_effect("lumetri")
                .and_then(|d| d.param("look"))
                .and_then(|p| if let filmcraft_project::ParamKind::Choice(o) = p.kind { Some(o) } else { None })
                .unwrap_or(&[]);
            ui.horizontal(|ui| {
                ui.add_sized(vec2(110.0, 20.0), egui::Label::new(egui::RichText::new(tl!("Look")).color(t.text_dim)));
                let mut sel = look;
                let r = craft_ui::ComboBox::from_id_salt("lumetri-look")
                    .selected_text(opts.get(look).map_or(tl!("None"), |o| crate::i18n::t(o)))
                    .width((ui.available_width() - 40.0).max(60.0))
                    .show_ui(ui, |ui| {
                        for (i, o) in opts.iter().enumerate() {
                            if ui.selectable_value(&mut sel, i, crate::i18n::t(o)).changed() {
                                set(&mut actions, "look", json!(i));
                            }
                        }
                    });
                auto_rows.push(("lumetri.param.look".into(), r.response.rect, "Look"));
            });
            lut_combo(ui, app, &e, "look_lut", tl!("Look LUT"), false, &mut lut_actions);
            slider(ui, &e, mt, "look_intensity", tl!("Intensity"), None, &t, &mut actions, clip, idx, &mut auto_rows);
            sub(ui, &t, tl!("Adjustments"));
            for (id, label) in
                [("faded_film", tl!("Faded Film")), ("sharpen", tl!("Sharpen")), ("vibrance", tl!("Vibrance")), ("creative_sat", tl!("Saturation"))]
            {
                slider(ui, &e, mt, id, label, None, &t, &mut actions, clip, idx, &mut auto_rows);
            }
        }
        if open_section(ui, app, &e, "Curves", "curves_on", "curves", &mut sections) {
            sub(ui, &t, tl!("RGB Curves"));
            let key = egui::Id::unique("lumetri-curve-channel");
            let mut ch: usize = ui.data(|d| d.get_temp(key)).unwrap_or(0);
            ui.horizontal(|ui| {
                for (i, c) in
                    [Color32::WHITE, Color32::from_rgb(230, 70, 70), Color32::from_rgb(80, 200, 90), Color32::from_rgb(80, 130, 250)].iter().enumerate()
                {
                    let (r, resp) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::click());
                    app.auto.add(&format!("lumetri.curveChannel.{i}"), r, ["Master", "Red", "Green", "Blue"][i]);
                    ui.painter().circle_filled(r.center(), 6.0, *c);
                    if ch == i {
                        ui.painter().circle_stroke(r.center(), 8.5, Stroke::new(1.5, t.hot_text));
                    }
                    if resp.clicked() {
                        ch = i;
                    }
                }
            });
            ui.data_mut(|d| d.insert_temp(key, ch));
            let (id, col) = [
                ("curve_luma", Color32::WHITE),
                ("curve_red", Color32::from_rgb(230, 70, 70)),
                ("curve_green", Color32::from_rgb(80, 200, 90)),
                ("curve_blue", Color32::from_rgb(80, 130, 250)),
            ][ch];
            let r = curve_editor(ui, &e, id, false, col, &t, &mut actions, clip, idx);
            auto_rows.push((format!("lumetri.curve.{id}"), r, "Curve"));
            if hdr {
                slider(ui, &e, mt, "curves_hdr_range", tl!("HDR Range"), None, &t, &mut actions, clip, idx, &mut auto_rows);
            }
            sub(ui, &t, tl!("Hue Saturation Curves"));
            for (id, label) in [
                ("hue_vs_sat", tl!("Hue vs Sat")),
                ("hue_vs_hue", tl!("Hue vs Hue")),
                ("hue_vs_luma", tl!("Hue vs Luma")),
                ("luma_vs_sat", tl!("Luma vs Sat")),
                ("sat_vs_sat", tl!("Sat vs Sat")),
            ] {
                ui.label(egui::RichText::new(label).color(t.text_dim));
                let r = curve_editor(ui, &e, id, true, Color32::WHITE, &t, &mut actions, clip, idx);
                auto_rows.push((format!("lumetri.curve.{id}"), r, label));
            }
        }
        if open_section(ui, app, &e, "Color Wheels & Match", "wheels_on", "wheels", &mut sections) {
            let w = ((ui.available_width() - 24.0) / 3.0).min(120.0);
            ui.horizontal(|ui| {
                for (id, lid, label) in [
                    ("wheel_shadows", "wheel_shadows_l", tl!("Shadows")),
                    ("wheel_midtones", "wheel_midtones_l", tl!("Midtones")),
                    ("wheel_highlights", "wheel_highlights_l", tl!("Highlights")),
                ] {
                    ui.vertical(|ui| {
                        ui.label(egui::RichText::new(label).color(t.text_dim));
                        let (wheel, lightness) = color_wheel(ui, &e, mt, id, lid, w, &t, &mut actions, clip, idx);
                        auto_rows.push((format!("lumetri.param.{id}"), wheel, label));
                        auto_rows.push((format!("lumetri.param.{lid}"), lightness, "Lightness"));
                    });
                }
            });
            if group.is_none() {
                match_controls(ui, app, &mut lut_actions);
            } else {
                ui.label(egui::RichText::new(tl!("Automatic clip matching stays separate from this shared look.")).small().color(t.text_dim));
            }
        }
        if open_section(ui, app, &e, "HSL Secondary", "hsl_on", "hsl", &mut sections) {
            sub(ui, &t, tl!("Key"));
            slider(ui, &e, mt, "hsl_hue", tl!("Hue"), Some(Gradient::Hue), &t, &mut actions, clip, idx, &mut auto_rows);
            for (id, label) in [
                ("hsl_hue_range", tl!("Hue Range")),
                ("hsl_sat_min", tl!("Saturation Min")),
                ("hsl_luma_min", tl!("Luma Min")),
                ("hsl_luma_max", tl!("Luma Max")),
                ("hsl_soft", tl!("Soften")),
            ] {
                slider(ui, &e, mt, id, label, None, &t, &mut actions, clip, idx, &mut auto_rows);
            }
            let mask = match e.param("hsl_show_mask").map(|p| &p.value) {
                Some(ParamValue::Choice(c)) => *c as usize,
                _ => 0,
            };
            ui.horizontal(|ui| {
                ui.add_sized(vec2(110.0, 20.0), egui::Label::new(egui::RichText::new(tl!("Show Mask")).color(t.text_dim)));
                let mut sel = mask;
                let modes = [tl!("Off"), tl!("Color/Gray"), tl!("Color/Black"), tl!("White/Black")];
                let r = craft_ui::ComboBox::from_id_salt("hsl-mask").selected_text(modes[mask.min(3)]).show_ui(ui, |ui| {
                    for (i, o) in modes.iter().enumerate() {
                        if ui.selectable_value(&mut sel, i, *o).changed() {
                            set(&mut actions, "hsl_show_mask", json!(i));
                        }
                    }
                });
                auto_rows.push(("lumetri.param.hsl_show_mask".into(), r.response.rect, "Show Mask"));
            });
            sub(ui, &t, tl!("Refine"));
            for (id, label) in [("hsl_denoise", tl!("Denoise")), ("hsl_blur", tl!("Blur"))] {
                slider(ui, &e, mt, id, label, None, &t, &mut actions, clip, idx, &mut auto_rows);
            }
            sub(ui, &t, tl!("Correction"));
            slider(ui, &e, mt, "hsl_temp", tl!("Temperature"), Some(Gradient::Temp), &t, &mut actions, clip, idx, &mut auto_rows);
            slider(ui, &e, mt, "hsl_tint", tl!("Tint"), Some(Gradient::Tint), &t, &mut actions, clip, idx, &mut auto_rows);
            slider(ui, &e, mt, "hsl_sat", tl!("Saturation"), None, &t, &mut actions, clip, idx, &mut auto_rows);
            slider(ui, &e, mt, "hsl_hue_shift", tl!("Hue Shift"), Some(Gradient::Hue), &t, &mut actions, clip, idx, &mut auto_rows);
        }
        if open_section(ui, app, &e, "Vignette", "vignette_on", "vignette", &mut sections) {
            for (id, label) in [
                ("vignette_amount", tl!("Amount")),
                ("vignette_midpoint", tl!("Midpoint")),
                ("vignette_roundness", tl!("Roundness")),
                ("vignette_feather", tl!("Feather")),
            ] {
                slider(ui, &e, mt, id, label, None, &t, &mut actions, clip, idx, &mut auto_rows);
            }
        }
        ui.add_space(20.0);
    });
    for (id, r, label) in auto_rows {
        if r.is_positive() {
            app.auto.add(&id, r, label);
        }
    }
    // a dragged value is one undo step, as in Effect Controls
    group_actions.extend(actions.into_iter().map(|mut a| {
        if let Some((id, _)) = &group {
            a["group"] = json!(id);
            ("lumetri.group.setParam".to_string(), a)
        } else {
            ("effects.setParam".to_string(), a)
        }
    }));
    crate::panels::effect_controls::run(app, ui.ctx(), group_actions);
    for sec in sections {
        let (command, params) = if let Some((id, _)) = &group {
            ("lumetri.group.setSection", json!({"group":id,"section":sec}))
        } else {
            ("lumetri.setSection", json!({"clip":clip.0,"section":sec}))
        };
        if let Err(err) = app.session.execute(command, params) {
            app.ui.status = err.to_string();
        }
    }
    for (cmd, mut p) in lut_actions {
        if cmd == "view.display" {
            // Comparison View on / off (a Program monitor display mode)
            let d = p["display"].as_str().unwrap_or("composite").to_string();
            if let Some(Err(err)) = crate::panels::monitor_view::route(app, &format!("view.display.{d}"), &json!({"monitor": "program"})) {
                app.ui.status = err;
            }
            continue;
        }
        if p.get("browse").is_some() {
            let (cmd, clip) = (cmd.to_string(), clip.0);
            let group = group.as_ref().map(|g| g.0);
            app.pick_ui(FileDialog::open_file(tl!("LUT"), &["cube", "3dl"]), move |app, paths| {
                let Some(path) = paths.into_iter().next() else { return };
                let result = if let Some(id) = group {
                    app.session.edit_group("Group LUT", |session| {
                        let imported = session.execute("lut.import", json!({"path":path}))?;
                        let reference =
                            imported["ref"].as_str().ok_or_else(|| filmcraft_engine::EngineError::Other("the imported LUT has no reference".into()))?;
                        session
                            .execute("lumetri.group.setLut", json!({"group":id,"slot":if cmd=="lumetri.setInputLut" {"input"} else {"look"},"lut":reference}))
                    })
                } else {
                    app.session.execute(&cmd, json!({"path":path,"clip":clip}))
                };
                if let Err(err) = result {
                    app.ui.status = err.to_string();
                }
            });
            continue;
        }
        if let Some((id, _)) = &group {
            if let Err(error) =
                app.session.execute("lumetri.group.setLut", json!({"group":id,"slot":if cmd=="lumetri.setInputLut" {"input"} else {"look"},"lut":p["lut"]}))
            {
                app.ui.status = error.to_string();
            }
            continue;
        }
        p["clip"] = json!(clip.0);
        if let Err(err) = app.session.execute(cmd, p) {
            app.ui.status = err.to_string();
        }
    }
}

fn open_section(
    ui: &mut egui::Ui,
    app: &mut FilmcraftApp,
    e: &EffectInstance,
    name: &str,
    param: &str,
    sec: &'static str,
    out: &mut Vec<&'static str>,
) -> bool {
    let on = e.param(param).and_then(|p| p.value.as_bool()).unwrap_or(param != "hsl_on");
    let (open, toggled) = section(ui, app, name, on);
    if toggled {
        out.push(sec);
    }
    open
}

/// The Color Grading panel's ≡ menu items: Export .cube… saves the grade of the clip the panel
/// shows (also an Apply Match result) as a 3D LUT (`lumetri.exportCube`; automation id
/// `panel.menu.LumetriColor.exportCube`). It is enabled only when that clip has an enabled
/// Color Grading, and exports that clip even when another selected clip has one.
/// True when the menu should close.
pub fn panel_menu(app: &mut FilmcraftApp, ui: &mut egui::Ui) -> bool {
    let group = selected(app).and_then(|(_, item)| super::grading_vibes::group_for(app, &item)).map(|(id, _)| id);
    if let Some(group) = group {
        let response = ui.button(tl!("Export shared look as .cube…"));
        app.auto.add_response("panel.menu.LumetriColor.exportCube", &response, "Export shared look as .cube");
        if response.clicked() {
            app.pick_ui(FileDialog::save_as(tl!("Cube LUT"), &["cube"], "Shared look.cube"), move |app, paths| {
                let Some(path) = paths.into_iter().next() else { return };
                let output = match app.session.execute("lumetri.group.exportCube", json!({"group":group,"path":path})) {
                    Ok(output) => output,
                    Err(error) => {
                        app.ui.status = error.to_string();
                        return;
                    }
                };
                let omitted = output["leftOut"].as_array().into_iter().flatten().filter_map(Value::as_str).collect::<Vec<_>>().join(", ");
                app.ui.status = tlf!("Exported {path}; not in the LUT: {effects}", path, effects = omitted);
            });
            return true;
        }
        return false;
    }
    let shown = selected(app).filter(|(_, it)| it.effects.iter().any(|e| e.enabled && e.effect == "lumetri")).map(|(c, _)| c);
    let r = ui.add_enabled(shown.is_some(), egui::Button::new(tl!("Export .cube…")));
    app.auto.add_response("panel.menu.LumetriColor.exportCube", &r, "Export .cube…");
    if let (true, Some(clip)) = (r.clicked(), shown) {
        if let Err(e) = crate::menus::invoke(app, ui.ctx(), "lumetri.exportCube", json!({"clip": clip.0})) {
            app.ui.status = e;
        }
        return true;
    }
    false
}

/// LUT menu: None, built-ins, the project's LUT library, Browse… (file dialog → `lut.import`).
fn lut_combo(ui: &mut egui::Ui, app: &mut FilmcraftApp, e: &EffectInstance, param: &str, label: &str, input: bool, out: &mut Vec<(&'static str, Value)>) {
    let t = app.tokens;
    let cur = match e.param(param).map(|p| &p.value) {
        Some(ParamValue::Text(s)) => s.clone(),
        _ => String::new(),
    };
    let cmd: &'static str = if input { "lumetri.setInputLut" } else { "lumetri.setLook" };
    let mut rect = Rect::NOTHING;
    ui.horizontal(|ui| {
        ui.add_sized(vec2(110.0, 20.0), egui::Label::new(egui::RichText::new(label).color(t.text_dim)));
        let width = (ui.available_width() - 40.0).max(60.0);
        let (r, pick) = lut_menu(ui, app, ("lumetri-lut", param), &cur, input, width, &format!("lumetri.{param}"));
        match pick {
            Some(LutPick::Lut(r)) => out.push((cmd, json!({"lut": r}))),
            Some(LutPick::Browse) => out.push((cmd, json!({"browse": true}))),
            None => {}
        }
        rect = r;
    });
    app.auto.add(&format!("lumetri.{param}"), rect, label);
}

/// What was chosen in a [`lut_menu`].
pub(crate) enum LutPick {
    /// A LUT reference (`builtin:<id>`, `lib:<id>`), or "" for None.
    Lut(String),
    /// Browse…: pick a .cube / .3dl file.
    Browse,
}

/// The LUT combo box for the LUT reference `cur`: None, the built-in input LUTs (`input`) or looks,
/// the project's LUT library, Browse…. Shared by the Color Grading panel and Effect Controls (#1123).
/// The entries get automation ids `<auto_id>.lut.<reference>` (`.lut.none`) and `<auto_id>.browse`.
pub(crate) fn lut_menu(
    ui: &mut egui::Ui,
    app: &mut FilmcraftApp,
    salt: impl std::hash::Hash + std::fmt::Debug,
    cur: &str,
    input: bool,
    width: f32,
    auto_id: &str,
) -> (Rect, Option<LutPick>) {
    let shown = filmcraft_render::luts::label(Some(&app.session.project), cur);
    let mut entries: Vec<(String, String)> = vec![(String::new(), tl!("None").into())];
    let builtins: Vec<&filmcraft_render::luts::Builtin> = if input {
        filmcraft_render::luts::input_builtins().collect()
    } else {
        filmcraft_render::luts::builtins().iter().filter(|b| b.id.starts_with("look-")).collect()
    };
    entries.extend(builtins.into_iter().map(|b| (format!("builtin:{}", b.id), b.label.clone())));
    entries.extend(app.session.project.luts.iter().map(|l| (format!("lib:{}", l.id), l.name.clone())));
    let mut pick = None;
    let r = craft_ui::ComboBox::from_id_salt(salt).selected_text(shown).truncate().width(width).show_ui(ui, |ui| {
        for (r, name) in &entries {
            let b = ui.selectable_label(r == cur, name);
            app.auto.add(&format!("{auto_id}.lut.{}", if r.is_empty() { "none" } else { r }), b.rect, name);
            if b.clicked() && r != cur {
                pick = Some(LutPick::Lut(r.clone()));
            }
        }
        ui.separator();
        let b = ui.selectable_label(false, tl!("Browse…"));
        app.auto.add(&format!("{auto_id}.browse"), b.rect, tl!("Browse…"));
        if b.clicked() {
            pick = Some(LutPick::Browse);
        }
    });
    (r.response.rect, pick)
}

/// A section header; returns (expanded, switch clicked). `on` is the section's enable switch.
fn section(ui: &mut egui::Ui, app: &mut FilmcraftApp, name: &str, on: bool) -> (bool, bool) {
    let t = app.tokens;
    let key = format!("lumetri2:{name}");
    let open = !app.ui.collapsed_fx.contains(&key) && (name == "Basic Correction" || app.ui.expanded_fx.contains(&key));
    ui.add_space(2.0);
    let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 40.0), Sense::click());
    ui.painter().line_segment([r.left_top(), r.right_top()], Stroke::new(1.0, t.separator));
    icons::paint(
        ui.painter(),
        Rect::from_center_size(pos2(r.min.x + 6.0, r.center().y), vec2(10.0, 10.0)),
        if open { Icon::ChevronDown } else { Icon::ChevronRight },
        t.text_dim,
    );
    ui.painter().text(pos2(r.min.x + 20.0, r.center().y), Align2::LEFT_CENTER, crate::i18n::t(name), Tokens::semibold(13.0), t.text);
    // enable switch (section bypass)
    let sw = Rect::from_center_size(pos2(r.max.x - 18.0, r.center().y), vec2(26.0, 14.0));
    let sw_resp = ui.interact(sw.expand(3.0), egui::Id::unique(("lumetri-switch", name)), Sense::click());
    ui.painter().rect_filled(sw, 7.0, if on { t.accent } else { Color32::from_rgb(0x55, 0x55, 0x55) });
    ui.painter().circle_filled(pos2(if on { sw.max.x - 7.0 } else { sw.min.x + 7.0 }, sw.center().y), 5.0, Color32::WHITE);
    app.auto.add(&format!("lumetri.switch.{name}"), sw.expand(3.0), &format!("{name} on/off"));
    app.auto.add(&format!("lumetri.section.{name}"), r, name);
    if sw_resp.clicked() {
        return (open, true);
    }
    if resp.clicked() {
        if open {
            app.ui.collapsed_fx.push(key.clone());
            app.ui.expanded_fx.retain(|k| *k != key);
        } else {
            app.ui.collapsed_fx.retain(|k| *k != key);
            app.ui.expanded_fx.push(key);
        }
    }
    (open, false)
}

fn sub(ui: &mut egui::Ui, t: &Tokens, name: &str) {
    ui.add_space(4.0);
    ui.label(egui::RichText::new(name).color(t.text).strong().size(12.0));
}

#[derive(Clone, Copy)]
enum Gradient {
    Temp,
    Tint,
    Hue,
}

/// Spectrum slider: 1 pt track (or gradient), hollow ring knob, scrubby value at the right.
#[allow(clippy::too_many_arguments)]
fn slider(
    ui: &mut egui::Ui,
    e: &EffectInstance,
    mt: Tick,
    id: &str,
    label: &'static str,
    grad: Option<Gradient>,
    t: &Tokens,
    actions: &mut Actions,
    clip: ClipId,
    idx: usize,
    auto_rows: &mut Vec<(String, Rect, &'static str)>,
) -> Rect {
    let Some(pd) = e.def().and_then(|d| d.param(id)) else { return Rect::NOTHING };
    let filmcraft_project::ParamKind::Float { min, max, soft_min, soft_max, decimals, .. } = pd.kind else { return Rect::NOTHING };
    let v = e.param(id).map(|p| p.f64_at(mt)).unwrap_or(pd.default.as_f64().unwrap_or(0.0));
    let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::hover());
    ui.painter().text(pos2(row.min.x + 4.0, row.center().y), Align2::LEFT_CENTER, label, Tokens::ui(12.0), t.text_dim);
    let track = Rect::from_min_max(pos2(row.min.x + 112.0, row.center().y - 1.0), pos2(row.max.x - 58.0, row.center().y + 1.0));
    match grad {
        Some(g) => {
            let n = 24;
            for i in 0..n {
                let f0 = i as f32 / n as f32;
                let c = match g {
                    Gradient::Temp => lerp_col(Color32::from_rgb(70, 120, 230), Color32::from_rgb(240, 150, 50), f0),
                    Gradient::Tint => lerp_col(Color32::from_rgb(60, 190, 80), Color32::from_rgb(220, 70, 200), f0),
                    Gradient::Hue => {
                        let rgb = filmcraft_color::hsl_to_rgb(f0, 0.8, 0.5);
                        Color32::from_rgb((rgb[0] * 255.0) as u8, (rgb[1] * 255.0) as u8, (rgb[2] * 255.0) as u8)
                    }
                };
                let x0 = track.min.x + track.width() * f0;
                ui.painter().rect_filled(Rect::from_min_max(pos2(x0, track.min.y - 1.0), pos2(x0 + track.width() / n as f32 + 0.5, track.max.y + 1.0)), 0.0, c);
            }
        }
        None => {
            ui.painter().rect_filled(track, 0.0, Color32::from_rgb(0x6e, 0x6e, 0x6e));
        }
    }
    let f = ((v - soft_min) / (soft_max - soft_min)).clamp(0.0, 1.0) as f32;
    let kx = track.min.x + f * track.width();
    let hit = Rect::from_min_max(pos2(track.min.x - 6.0, row.min.y), pos2(track.max.x + 6.0, row.max.y));
    let resp = ui.interact(hit, egui::Id::unique(("lslider", clip.0, id)), Sense::click_and_drag());
    ui.painter().circle_filled(pos2(kx, track.center().y), 5.0, t.panel_bg);
    ui.painter().circle_stroke(
        pos2(kx, track.center().y),
        5.0,
        Stroke::new(1.5, if resp.dragged() { t.hot_text } else { Color32::from_rgb(0xb0, 0xb0, 0xb0) }),
    );
    let mut out = None;
    if (resp.dragged() || resp.clicked())
        && let Some(p) = resp.interact_pointer_pos()
    {
        let nf = ((p.x - track.min.x) / track.width()).clamp(0.0, 1.0) as f64;
        out = Some(soft_min + nf * (soft_max - soft_min));
    }
    if resp.double_clicked() {
        out = pd.default.as_f64();
    }
    let mut vui = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(Rect::from_min_max(pos2(row.max.x - 52.0, row.min.y + 3.0), pos2(row.max.x, row.max.y - 3.0)))
            .layout(egui::Layout::right_to_left(egui::Align::Center)),
    );
    let (_, nv) = crate::widgets::hot_number(
        &mut vui,
        label,
        egui::Id::unique(("lval", clip.0, id)),
        v,
        HotNumberOptions::new((soft_max - soft_min) / 300.0, (min, max), decimals as usize, "", t),
    );
    if let Some(nv) = out.or(nv) {
        actions.push(json!({"clip": clip.0, "effect": idx, "param": id, "value": (nv * 10f64.powi(decimals as i32)).round() / 10f64.powi(decimals as i32)}));
    }
    auto_rows.push((format!("lumetri.param.{id}"), row, label));
    row
}

fn lerp_col(a: Color32, b: Color32, f: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * f) as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

/// Curve editor: drag points, click the curve to add, double-click a point to remove. Returns its
/// rectangle (its automation element, `lumetri.curve.<param>`).
#[allow(clippy::too_many_arguments)]
fn curve_editor(ui: &mut egui::Ui, e: &EffectInstance, id: &str, hue: bool, col: Color32, t: &Tokens, actions: &mut Actions, clip: ClipId, idx: usize) -> Rect {
    let pts: Vec<[f32; 2]> = e.param(id).and_then(|p| p.value.as_curve().map(|c| c.to_vec())).unwrap_or_default();
    // as wide as the panel (it stayed 260 px wide in a wide panel, #1658), not taller than 420 px
    let w = ui.available_width().max(1.0);
    let h = if hue { w * 0.45 } else { w }.min(420.0);
    let (area, resp) = ui.allocate_exact_size(vec2(w, h), Sense::click_and_drag());
    let p = ui.painter();
    p.rect_filled(area, 2.0, Color32::from_rgb(0x16, 0x16, 0x16));
    for i in 1..4 {
        let f = i as f32 / 4.0;
        p.line_segment(
            [pos2(area.min.x + area.width() * f, area.min.y), pos2(area.min.x + area.width() * f, area.max.y)],
            Stroke::new(1.0, Color32::from_rgb(0x2a, 0x2a, 0x2a)),
        );
        p.line_segment(
            [pos2(area.min.x, area.min.y + area.height() * f), pos2(area.max.x, area.min.y + area.height() * f)],
            Stroke::new(1.0, Color32::from_rgb(0x2a, 0x2a, 0x2a)),
        );
    }
    if hue {
        let n = 36;
        for i in 0..n {
            let f0 = i as f32 / n as f32;
            let rgb = filmcraft_color::hsl_to_rgb(f0, 0.8, 0.5);
            let c = Color32::from_rgb((rgb[0] * 255.0) as u8, (rgb[1] * 255.0) as u8, (rgb[2] * 255.0) as u8);
            p.rect_filled(
                Rect::from_min_max(
                    pos2(area.min.x + area.width() * f0, area.max.y - 6.0),
                    pos2(area.min.x + area.width() * (f0 + 1.0 / n as f32) + 0.5, area.max.y),
                ),
                0.0,
                c,
            );
        }
    }
    let to_screen = |q: [f32; 2]| pos2(area.min.x + q[0] * area.width(), area.max.y - q[1] * area.height());
    let from_screen = |s: Pos2| [((s.x - area.min.x) / area.width()).clamp(0.0, 1.0), ((area.max.y - s.y) / area.height()).clamp(0.0, 1.0)];
    // curve polyline
    let draw_pts: Vec<[f32; 2]> = if hue && pts.is_empty() { vec![[0.0, 0.5], [1.0, 0.5]] } else { pts.clone() };
    // the hue curves wrap around red; Luma vs Sat and Sat vs Sat do not (same LUTs as the render)
    let lut = match (hue, id.starts_with("hue_vs_")) {
        (true, true) => filmcraft_render::effects::hue_lut(&pts, 128),
        (true, false) => filmcraft_render::effects::vs_sat_lut(&pts, 128),
        _ => None,
    }
    .unwrap_or_else(|| filmcraft_render::effects::curve_lut(&draw_pts, 128));
    let line: Vec<Pos2> = lut.iter().enumerate().map(|(i, y)| to_screen([i as f32 / 127.0, y.clamp(0.0, 1.0)])).collect();
    p.add(egui::Shape::line(line.clone(), Stroke::new(1.5, col)));
    // the point nearest `pos` within reach (drag and double-click pick the same one)
    let nearest = |pos: Pos2| {
        pts.iter().enumerate().map(|(i, q)| (i, to_screen(*q).distance(pos))).filter(|(_, d)| *d < 9.0).min_by(|a, b| a.1.total_cmp(&b.1)).map(|x| x.0)
    };
    let drag_key = egui::Id::unique(("curve-drag", clip.0, id));
    let mut dragging: Option<usize> = ui.data(|d| d.get_temp(drag_key));
    let mut new_pts = pts.clone();
    let mut changed = false;
    for (i, q) in pts.iter().enumerate() {
        let sp = to_screen(*q);
        p.circle_filled(sp, 4.0, if dragging == Some(i) { t.hot_text } else { col });
    }
    if resp.drag_started()
        && let Some(pos) = resp.interact_pointer_pos()
    {
        let near = nearest(pos);
        dragging = near.or_else(|| {
            new_pts.push(from_screen(pos));
            changed = true;
            Some(new_pts.len() - 1)
        });
    }
    if resp.dragged()
        && let (Some(i), Some(pos)) = (dragging, resp.interact_pointer_pos())
        && i < new_pts.len()
    {
        new_pts[i] = from_screen(pos);
        changed = true;
    }
    if resp.drag_stopped() {
        dragging = None;
    }
    // A plain click on the curve places a point where it landed (#3431): before, only a drag
    // created points, so the curve ignored the click the interaction promises. A click on a
    // point selects it (as a drag start does); a click off the curve adds nothing, so the RGB
    // double-click reset and the hue double-click remove keep working.
    if resp.clicked()
        && let Some(pos) = resp.interact_pointer_pos()
    {
        match nearest(pos) {
            Some(i) => dragging = Some(i),
            None if line.iter().any(|p| p.distance(pos) < 9.0) => {
                dragging = None;
                new_pts.push(from_screen(pos));
                changed = true;
            }
            _ => dragging = None,
        }
    }
    if resp.double_clicked()
        && let Some(pos) = resp.interact_pointer_pos()
        && let Some(i) = nearest(pos)
    {
        if let Some(rest) = without_point(&pts, i, hue) {
            new_pts = rest;
            changed = true;
        }
    } else if resp.double_clicked() && !hue {
        new_pts = vec![[0.0, 0.0], [1.0, 1.0]];
        changed = true;
    }
    ui.data_mut(|d| {
        if let Some(i) = dragging {
            d.insert_temp(drag_key, i);
        } else {
            d.remove::<usize>(drag_key);
        }
    });
    if changed {
        let v: Vec<Value> = new_pts.iter().map(|q| json!([q[0], q[1]])).collect();
        actions.push(json!({"clip": clip.0, "effect": idx, "param": id, "value": v}));
    }
    ui.add_space(6.0);
    area
}

/// `pts` without point `i` (double-click), or `None` for a point that stays: the leftmost and
/// rightmost points of an RGB curve. Points are stored in the order they were added, not by x, so
/// list position says nothing (the newest point could never be removed, #1658).
fn without_point(pts: &[[f32; 2]], i: usize, hue: bool) -> Option<Vec<[f32; 2]>> {
    let q = pts.get(i)?;
    let (lo, hi) = pts.iter().fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), p| (lo.min(p[0]), hi.max(p[0])));
    if !hue && (q[0] <= lo || q[0] >= hi) {
        return None;
    }
    let mut rest = pts.to_vec();
    rest.remove(i);
    Some(rest)
}

/// Colour wheel: hue ring, draggable puck (offset), lightness slider on the left.
#[allow(clippy::too_many_arguments)]
fn color_wheel(
    ui: &mut egui::Ui,
    e: &EffectInstance,
    mt: Tick,
    id: &str,
    lid: &str,
    size: f32,
    t: &Tokens,
    actions: &mut Actions,
    clip: ClipId,
    idx: usize,
) -> (Rect, Rect) {
    let v = e.param(id).map(|p| p.vec2_at(mt)).unwrap_or_default();
    let (area, resp) = ui.allocate_exact_size(vec2(size, size), Sense::click_and_drag());
    let c = area.center() + vec2(6.0, 0.0);
    let r_out = size / 2.0 - 4.0;
    let r_in = r_out - 10.0;
    let p = ui.painter();
    let n = 72;
    let mut mesh = egui::Mesh::default();
    for i in 0..n {
        let a0 = i as f32 / n as f32 * std::f32::consts::TAU;
        let a1 = (i + 1) as f32 / n as f32 * std::f32::consts::TAU;
        // Positive wheel y points up: match the renderer's red → green → blue rotation.
        let rgb = filmcraft_color::hsl_to_rgb(i as f32 / n as f32, 0.75, 0.5);
        let col = Color32::from_rgb((rgb[0] * 255.0) as u8, (rgb[1] * 255.0) as u8, (rgb[2] * 255.0) as u8);
        let base = mesh.vertices.len() as u32;
        for (a, r) in [(a0, r_in), (a0, r_out), (a1, r_out), (a1, r_in)] {
            mesh.colored_vertex(c + vec2(a.cos(), -a.sin()) * r, col);
        }
        mesh.add_triangle(base, base + 1, base + 2);
        mesh.add_triangle(base, base + 2, base + 3);
    }
    p.add(mesh);
    p.circle_filled(c, r_in - 1.0, Color32::from_rgb(0x16, 0x16, 0x16));
    p.line_segment([c - vec2(6.0, 0.0), c + vec2(6.0, 0.0)], Stroke::new(1.0, t.text_faint));
    p.line_segment([c - vec2(0.0, 6.0), c + vec2(0.0, 6.0)], Stroke::new(1.0, t.text_faint));
    let puck = c + vec2(v.x as f32, -v.y as f32) * (r_in - 4.0);
    p.circle_stroke(puck, 5.0, Stroke::new(1.5, Color32::WHITE));
    if (resp.dragged() || resp.clicked())
        && let Some(pos) = resp.interact_pointer_pos()
    {
        let d = (pos - c) / (r_in - 4.0);
        let len = d.length().min(1.0);
        let dir = if d.length() > 1e-6 { d / d.length() } else { d };
        actions.push(json!({"clip": clip.0, "effect": idx, "param": id, "value": [dir.x * len, -dir.y * len]}));
    }
    if resp.double_clicked() {
        actions.push(json!({"clip": clip.0, "effect": idx, "param": id, "value": [0.0, 0.0]}));
    }
    // lightness slider (vertical) at the wheel's left edge
    let l = e.param(lid).map(|p| p.f64_at(mt)).unwrap_or(0.0) as f32;
    let track = Rect::from_min_max(pos2(area.min.x, area.min.y + 8.0), pos2(area.min.x + 4.0, area.max.y - 8.0));
    p.rect_filled(track, 2.0, Color32::from_rgb(0x4b, 0x4b, 0x4b));
    let ky = track.center().y - l / 100.0 * track.height() / 2.0;
    p.circle_stroke(pos2(track.center().x, ky), 4.0, Stroke::new(1.5, Color32::from_rgb(0xb0, 0xb0, 0xb0)));
    let lr = ui.interact(track.expand2(vec2(6.0, 0.0)), egui::Id::unique(("wl", clip.0, lid)), Sense::click_and_drag());
    if (lr.dragged() || lr.clicked())
        && let Some(pos) = lr.interact_pointer_pos()
    {
        let nv = ((track.center().y - pos.y) / (track.height() / 2.0) * 100.0).clamp(-100.0, 100.0);
        actions.push(json!({"clip": clip.0, "effect": idx, "param": lid, "value": nv}));
    }
    let _ = StrokeKind::Inside;
    (area, lr.rect)
}

/// Color Match: reference frame (sequence timecode), skin-tone protection,
/// Apply Match (`lumetri.applyMatch`).
fn match_controls(ui: &mut egui::Ui, app: &mut FilmcraftApp, out: &mut Vec<(&'static str, Value)>) {
    let t = app.tokens;
    sub(ui, &t, tl!("Color Match"));
    let key = egui::Id::unique("lumetri-match-ref");
    let fd_key = egui::Id::unique("lumetri-match-face");
    let mut tc: String = ui.data(|d| d.get_temp(key)).unwrap_or_else(|| "00:00:00:00".to_string());
    let mut face: bool = ui.data(|d| d.get_temp(fd_key)).unwrap_or(true);
    // Comparison View: the Program monitor shows the reference frame next to the current one, and
    // Apply Match uses that reference
    let comparing = app.ui.program.display_mode() == Some(crate::state::DisplayMode::Comparison);
    let b = ui.selectable_label(comparing, tl!("Comparison View"));
    app.auto.add("lumetri.match.comparisonView", b.rect, "Comparison View");
    if b.clicked() {
        out.push(("view.display", json!({"display": if comparing { "composite" } else { "comparison" }})));
    }
    let reference = app.ui.program.compare_ref.filter(|_| comparing).map(filmcraft_time::Tick);
    if let (Some(r), Some(q)) = (reference, app.session.active_sequence()) {
        tc = filmcraft_time::format_time(r, q.settings.frame_rate, q.settings.drop_frame, filmcraft_time::TimeDisplay::Timecode, 48000);
    }
    ui.horizontal(|ui| {
        ui.add_sized(vec2(110.0, 20.0), egui::Label::new(egui::RichText::new(tl!("Reference")).color(t.text_dim)));
        let r = crate::widgets::accessible_name(ui.add(egui::TextEdit::singleline(&mut tc).desired_width(96.0)), "Reference timecode");
        app.auto.add("lumetri.match.reference", r.rect, "Reference timecode");
        crate::widgets::select_all_on_focus(ui, &r, &tc);
    });
    let r = ui.checkbox(&mut face, tl!("Protect skin tones")).on_hover_text(tl!("Reduces hue changes in pixels near the skin-tone color range."));
    // Keep the existing automation id for saved assistant guides and control-channel clients.
    app.auto.add("lumetri.match.faceDetection", r.rect, "Protect skin tones");
    let b = ui.button(tl!("Apply Match"));
    app.auto.add("lumetri.match.apply", b.rect, "Apply Match");
    if b.clicked() {
        let p = match reference {
            Some(r) => json!({"referenceTime": r.0, "faceDetection": face}),
            None => json!({"referenceTimecode": tc, "faceDetection": face}),
        };
        out.push(("lumetri.applyMatch", p));
    }
    ui.data_mut(|d| {
        d.insert_temp(key, tc);
        d.insert_temp(fd_key, face);
    });
}

#[cfg(test)]
mod tests {
    use super::without_point;

    /// Exercise the painted wheel, rather than setting its vector through the engine: a blue
    /// swatch must actually tint a neutral image blue when the user clicks it.
    #[test]
    fn grading_wheel_painted_hues_agree_with_the_rendered_colour() {
        use filmcraft_project::{ParamValue, find_effect};
        use filmcraft_render::effects::{FxCtx, apply};
        use filmcraft_time::Tick;

        for channel in [1, 2] {
            let ctx = egui::Context::default();
            let tokens = crate::theme::Tokens::for_kind(crate::theme::ThemeKind::Dark);
            let mut effect = find_effect("lumetri").unwrap().instance();
            let mut actions = Vec::new();
            let mut draw = |events| {
                let mut output = ctx.run_ui(
                    egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(240.0, 200.0))), events, ..Default::default() },
                    |ui| {
                        super::color_wheel(
                            ui,
                            &effect,
                            Tick::ZERO,
                            "wheel_midtones",
                            "wheel_midtones_l",
                            120.0,
                            &tokens,
                            &mut actions,
                            filmcraft_project::ClipId(1),
                            0,
                        );
                    },
                );
                // This interaction-only test inspects shapes; it intentionally has no painter
                // consuming the font atlas updates.
                output.textures_delta.clear();
                output
            };
            let painted = draw(Vec::new());
            let position = painted
                .shapes
                .iter()
                .find_map(|shape| {
                    if let egui::Shape::Mesh(mesh) = &shape.shape {
                        mesh.vertices.iter().find_map(|v| {
                            let rgba = v.color.to_array();
                            (rgba[channel] > 180 && rgba[channel] > rgba[3 - channel].saturating_add(60) && rgba[channel] > rgba[0].saturating_add(60))
                                .then_some(v.pos)
                        })
                    } else {
                        None
                    }
                })
                .expect("painted green/blue wheel swatch");
            let button =
                |pressed| egui::Event::PointerButton { pos: position, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
            let _ = draw(vec![egui::Event::PointerMoved(position), button(true)]);
            let _ = draw(vec![button(false)]);
            let value = actions.iter().rev().find(|a| a["param"] == "wheel_midtones").expect("click must edit the wheel")["value"].as_array().unwrap();
            effect.params.get_mut("wheel_midtones").unwrap().value =
                ParamValue::Vec2(filmcraft_geom::Vec2::new(value[0].as_f64().unwrap(), value[1].as_f64().unwrap()));
            let mut image = filmcraft_render::Image::new(1, 1);
            image.px.copy_from_slice(&[0.18, 0.18, 0.18, 1.0]);
            apply(
                &mut image,
                &effect,
                &FxCtx {
                    t: Tick::ZERO,
                    px_scale: 1.0,
                    seconds: 0.0,
                    timecode: "",
                    clip_name: "",
                    project: None,
                    env: None,
                    frame_rate: 24.0,
                    gamut: (filmcraft_color::WorkingSpace::Rec709).output_space().gamut(),
                    working: filmcraft_color::WorkingSpace::Rec709,
                },
            );
            assert!(
                image.px[channel] > image.px[3 - channel] + 0.01 && image.px[channel] > image.px[0] + 0.01,
                "Clicking a painted {} swatch produced {:?}",
                if channel == 1 { "green" } else { "blue" },
                image.px
            );
        }
    }

    #[test]
    fn a_curve_point_is_removed_by_position_not_by_list_order() {
        // #1658: added points are appended, so the newest one is last in the list
        let pts = [[0.0, 0.0], [1.0, 1.0], [0.5, 0.7]];
        assert_eq!(without_point(&pts, 2, false), Some(vec![[0.0, 0.0], [1.0, 1.0]]), "the newest point can go");
        assert_eq!(without_point(&pts, 1, false), None, "the right end stays");
        assert_eq!(without_point(&pts, 0, false), None, "the left end stays");
        // hue curves wrap around: any point can go
        assert_eq!(without_point(&pts, 0, true), Some(vec![[1.0, 1.0], [0.5, 0.7]]));
        assert_eq!(without_point(&pts, 7, false), None);
    }
}
