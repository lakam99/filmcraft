//! Structural checks of written AAF files against the required properties and rules of the AAF
//! Object Specification v1.1 and the Edit Protocol, read back through our compound file reader.

use std::collections::{HashMap, HashSet};

use filmcraft_project::{Project, SequenceSettings, TrackKind, TransitionAlign, TransitionId, find_effect};
use filmcraft_time::{FrameRate, Tick, TimeRange};

use super::ids::{self, cls, ddef, def, path, pid};
use super::store::{self, Obj, Value};
use super::*;

fn project() -> (Project, ItemId) {
    let mut p = Project::new("t");
    let info = filmcraft_media::MediaInfo {
        name: "a.mov".into(),
        kind: filmcraft_media::MediaKind::Movie,
        duration: FrameRate::FPS_24.tick_of(2400),
        video: Some(filmcraft_media::VideoStreamInfo {
            width: 1280,
            height: 720,
            frame_rate: FrameRate::FPS_24,
            par: (1, 1),
            codec: String::new(),
            pixel_format: String::new(),
            color: Default::default(),
            has_alpha: false,
            bitrate: None,
            hdr: None,
            color_assumed: None,
        }),
        audio_streams: vec![filmcraft_media::AudioStreamInfo { sample_rate: 48_000, channels: 2, codec: String::new(), bits_per_sample: Some(24) }],
        container: String::new(),
        start_timecode: Some(86_400),
        file_size: None,
    };
    let a = p.add_item(
        "a.mov",
        filmcraft_project::Label::Iris,
        filmcraft_project::ItemKind::Media(Box::new(filmcraft_project::MediaClip {
            media: filmcraft_project::MediaRef::File { path: "/m/a.mov".into() },
            info,
            interpret: Default::default(),
            mark_in: None,
            mark_out: None,
            markers: vec![],
            offline: false,
            proxy: None,
            identity: None,
        })),
        None,
    );
    let r = FrameRate::FPS_24;
    let s = p.new_sequence("S", SequenceSettings { frame_rate: r, ..Default::default() }, 1, 1, None);
    let mut ids = Vec::new();
    for (kind, start, dur, src) in
        [(TrackKind::Video, 0, 48, 10), (TrackKind::Video, 48, 48, 200), (TrackKind::Audio, 0, 48, 10), (TrackKind::Audio, 48, 48, 200)]
    {
        let ti = p.make_track_item(a, kind, r.tick_of(start), TimeRange::new(r.tick_of(src), r.tick_of(dur)), r).unwrap();
        ids.push(ti.id);
        let q = p.sequence_mut(s).unwrap();
        q.tracks_mut(kind)[0].items.push(ti);
    }
    for (kind, i, eff) in [(TrackKind::Video, 0, "cross_dissolve"), (TrackKind::Audio, 2, "constant_power")] {
        let id = TransitionId(p.alloc_id());
        let q = p.sequence_mut(s).unwrap();
        q.tracks_mut(kind)[0].transitions.push(filmcraft_project::Transition {
            id,
            effect: find_effect(eff).unwrap().instance(),
            start: r.tick_of(42),
            duration: r.tick_of(12),
            from: Some(ids[i]),
            to: Some(ids[i + 1]),
            align: TransitionAlign::CenterAtCut,
            reverse: false,
        });
    }
    let q = p.sequence_mut(s).unwrap();
    q.audio_tracks[0].items[1].gain_db = -3.0;
    (p, s)
}

fn walk<'a>(o: &'a Obj, out: &mut Vec<&'a Obj>) {
    out.push(o);
    for (_, v) in &o.props {
        match v {
            Value::Strong(c) => walk(c, out),
            Value::StrongVec(v) | Value::StrongSet(v, _) => v.iter().for_each(|c| walk(c, out)),
            _ => {}
        }
    }
}

fn keys(dict: &Obj, p: u16) -> HashSet<Vec<u8>> {
    dict.objs(p).iter().filter_map(|d| d.data(pid::IDENTIFICATION).map(<[u8]>::to_vec)).collect()
}

/// The project of [`project`] with a sequence marker, volume keyframes and embedded audio, so that
/// an export uses every class the writer knows.
fn rich_export(small_sectors: bool) -> Vec<u8> {
    use crate::essence::{AudioEssence, EssenceData, MediaOptions, NeedOptions, audio_needs};
    use filmcraft_project::{Interpolation, Keyframe, Label, Marker, MarkerId, MarkerKind, Param, ParamValue};
    let (mut p, s) = project();
    let r = FrameRate::FPS_24;
    let q = p.sequence_mut(s).unwrap();
    q.markers.push(Marker {
        url: String::new(),
        author: String::new(),
        custom_color: None,
        id: MarkerId(9001),
        start: r.tick_of(50),
        duration: r.tick_of(10),
        name: "Fix this".into(),
        comment: "colour".into(),
        kind: MarkerKind::Comment,
        color: Label::Mango,
    });
    let mut level = Param::new(ParamValue::Float(0.0));
    for (f, db) in [(0, -12.0), (24, 0.0)] {
        let mut k = Keyframe::new(r.tick_of(f), ParamValue::Float(db));
        k.interp = Interpolation::Linear;
        level.keyframes.push(k);
    }
    q.audio_tracks[0].items[0].effect_mut("volume").unwrap().params.insert("level".into(), level);
    let essence = audio_needs(&p, s, &NeedOptions::default())
        .iter()
        .map(|n| {
            let frames = ((n.end - n.start).0 / (filmcraft_time::TICKS_PER_SECOND / 48_000)) as u64;
            let data = EssenceData::Embedded(vec![0; frames as usize * 2 * 3]);
            AudioEssence {
                key: n.key,
                channel: None,
                start: n.start,
                frames,
                sample_rate: 48_000,
                bits: 24,
                channels: 2,
                data,
                effects_rendered: false,
                timing_rendered: false,
                render_gain: 1.0,
            }
        })
        .collect();
    let opts = AafOptions { small_sectors, media: MediaOptions { essence, ..Default::default() }, ..Default::default() };
    export(&p, s, &opts).unwrap().0
}

fn weak_target(v: Option<&Value>, path: [u16; 2]) -> Vec<u8> {
    match v {
        Some(Value::Weak(p, k, key)) if *p == path && *k == pid::META_IDENTIFICATION => key.clone(),
        other => panic!("not a weak reference into {path:04x?}: {other:?}"),
    }
}

// A model that resolves is insufficient if its instances omit required properties.
fn assert_required_properties(o: &Obj, classes: &HashMap<Vec<u8>, &Obj>) {
    let mut at = o.class.to_vec();
    for _ in 0..classes.len() {
        let class = &classes[&at];
        for prop in class.objs(pid::CLASS_PROPERTIES) {
            if prop.u8(pid::PROPERTY_IS_OPTIONAL) == Some(0) {
                let local = prop.u16(pid::PROPERTY_LOCAL_ID).unwrap();
                assert!(local == 0x0101 || o.get(local).is_some(), "required property {local:#06x} of {:?}", class.string(pid::META_NAME));
            }
        }
        let parent = weak_target(class.get(pid::CLASS_PARENT), path::META_CLASSES);
        if parent == at {
            return;
        }
        at = parent;
    }
    panic!("cyclic class model");
}

/// AAF Object Specification v1.1 §5: "An AAF file shall contain the class model being used by the
/// file in the MetaDictionary object's ClassDefinitions and TypeDefinitions properties." LibAAF
/// refuses a file whose ClassDefinitions set is empty. Every property of every object written is
/// defined, with its local tag, on the object's class or an ancestor, and every class, type and
/// reference in that model resolves.
#[test]
fn the_meta_dictionary_holds_the_class_model_of_the_file() {
    let (p, s) = project();
    for (bytes, small, rich) in
        [(export(&p, s, &AafOptions::default()).unwrap().0, false, false), (rich_export(false), false, true), (rich_export(true), true, true)]
    {
        let cf = filmcraft_cfb::CompoundFile::open(&bytes).unwrap();
        assert_eq!(cf.version, if small { 3 } else { 4 });
        assert_eq!(cf.entries[cf.find("MetaDictionary-1").unwrap()].clsid, ids::META_DICTIONARY);
        let root = store::read(&bytes).unwrap();
        let meta = root.strong(pid::ROOT_META_DICTIONARY).unwrap();
        let by_id = |set: u16| -> HashMap<Vec<u8>, &Obj> {
            assert!(matches!(meta.get(set), Some(Value::StrongSet(items, k)) if !items.is_empty() && *k == pid::META_IDENTIFICATION), "{set:#06x}");
            meta.objs(set).iter().map(|o| (o.data(pid::META_IDENTIFICATION).unwrap().to_vec(), o)).collect()
        };
        let (classes, types) = (by_id(pid::META_CLASS_DEFINITIONS), by_id(pid::META_TYPE_DEFINITIONS));
        assert!(classes.values().all(|c| c.class == ids::meta::CLASS_DEFINITION));

        // Every class reaches its root (its own parent); every property has a known type.
        let interchange_object = ids::class(0x01).to_vec();
        let meta_definition = ids::meta::META_DEFINITION.to_vec();
        let mut defined: HashMap<Vec<u8>, HashSet<u16>> = HashMap::new();
        for (id, c) in &classes {
            let mut at = id.clone();
            for _ in 0..classes.len() {
                at = weak_target(classes[&at].get(pid::CLASS_PARENT), path::META_CLASSES);
                assert!(classes.contains_key(&at), "parent of {id:02x?}");
            }
            assert!(at == interchange_object || at == meta_definition || at == ids::META_DICTIONARY);
            for prop in c.objs(pid::CLASS_PROPERTIES) {
                assert_eq!(prop.class, ids::meta::PROPERTY_DEFINITION);
                assert!(types.contains_key(prop.data(pid::PROPERTY_TYPE).unwrap()), "type of {:?}", prop.string(pid::META_NAME));
                assert!(defined.entry(id.clone()).or_default().insert(prop.u16(pid::PROPERTY_LOCAL_ID).unwrap()));
            }
        }
        // every type a type is made of is defined, and every class it refers to
        for t in types.values() {
            for (_, v) in &t.props {
                match v {
                    Value::Weak(p, ..) if *p == path::META_TYPES => assert!(types.contains_key(&weak_target(Some(v), path::META_TYPES))),
                    Value::Weak(..) => assert!(classes.contains_key(&weak_target(Some(v), path::META_CLASSES))),
                    Value::WeakVec(p, _, keys) => assert!(*p == path::META_TYPES.to_vec() && keys.iter().all(|k| types.contains_key(k))),
                    _ => {}
                }
            }
        }

        // every object written uses a defined class and defined properties
        let header = root.strong(pid::ROOT_HEADER).unwrap();
        let mut all = Vec::new();
        walk(header, &mut all);
        walk(meta, &mut all);
        let mut used = HashSet::new();
        for o in all {
            used.insert(o.class);
            assert_required_properties(o, &classes);
            for (p, _) in &o.props {
                let mut at = o.class.to_vec();
                while !defined.get(&at).is_some_and(|d| d.contains(p)) {
                    assert_ne!(at, interchange_object, "property {p:#06x} of class {:02x?} is not in the meta-dictionary", o.class);
                    at = weak_target(classes.get(&at).unwrap_or_else(|| panic!("class {at:02x?}")).get(pid::CLASS_PARENT), path::META_CLASSES);
                }
            }
        }
        if rich {
            for c in [cls::ESSENCE_DATA, cls::PCM_DESCRIPTOR, cls::COMMENT_MARKER, cls::EVENT_MOB_SLOT, cls::VARYING_VALUE, cls::CONTROL_POINT] {
                assert!(used.contains(&c), "the rich export uses {c:02x?}");
            }
        }
    }
}

/// AAF Stored Format §1.6 defines a file-kind signature for each sector size. This is not
/// the Root object's CLSID: generic compound-file readers can open files without this marker,
/// masking the defect from a round trip while an AAF file-type recognizer rejects the file.
fn assert_file_kind_signature(small_sectors: bool) {
    let (p, s) = project();
    let bytes = export(&p, s, &AafOptions { small_sectors, ..Default::default() }).unwrap().0;
    let expected = if small_sectors {
        ids::guid(0x42464141, 0x000D, 0x4D4F, [6, 14, 43, 52, 1, 1, 1, 255])
    } else {
        ids::guid(0x0D010201, 0x0200, 0, [6, 14, 43, 52, 3, 2, 1, 1])
    };
    assert_eq!(bytes.get(8..24).unwrap(), expected, "AAF file-kind signature, small sectors = {small_sectors}");
    let cf = filmcraft_cfb::CompoundFile::open(&bytes).unwrap();
    assert_eq!(cf.version, if small_sectors { 3 } else { 4 });
    assert_eq!(cf.root().clsid, ids::ROOT, "the object root retains its own class");
    assert_ne!(expected, ids::ROOT);
    assert!(sniff(&bytes));
}

#[test]
fn file_kind_signature_for_512_byte_sectors() {
    assert_file_kind_signature(true);
}

#[test]
fn file_kind_signature_for_4096_byte_sectors() {
    assert_file_kind_signature(false);
}

#[test]
fn parameter_definitions_resolve_their_required_value_type() {
    let (p, s) = project();
    let bytes = export(&p, s, &AafOptions::default()).unwrap().0;
    let root = store::read(&bytes).unwrap();
    let types = root.strong(pid::ROOT_META_DICTIONARY).unwrap().objs(pid::META_TYPE_DEFINITIONS);
    let dict = root.strong(pid::ROOT_HEADER).unwrap().strong(pid::DICTIONARY).unwrap();
    for parameter in dict.objs(pid::PARAMETER_DEFINITIONS) {
        let ty = weak_target(parameter.get(pid::PARAMDEF_TYPE), path::META_TYPES);
        assert_eq!(ty, def::TYPE_RATIONAL);
        assert!(types.iter().any(|t| t.data(pid::META_IDENTIFICATION) == Some(ty.as_slice())));
    }
}

#[test]
fn embedded_essence_is_stored_as_an_opaque_stream() {
    for small in [false, true] {
        let bytes = rich_export(small);
        let cf = filmcraft_cfb::CompoundFile::open(&bytes).unwrap();
        let root = store::read(&bytes).unwrap();
        let essence = root.strong(pid::ROOT_HEADER).unwrap().strong(pid::CONTENT).unwrap().objs(pid::ESSENCE_DATA);
        assert!(!essence.is_empty());
        for (i, e) in essence.iter().enumerate() {
            let path = format!("Header-2/Content-3b03/EssenceData-1902{{{i:x}}}");
            let props = cf.read_path(&format!("{path}/properties")).unwrap();
            let count = u16::from_le_bytes(props[2..4].try_into().unwrap()) as usize;
            let mut offset = 4 + count * 6;
            let mut found = false;
            for entry in props[4..offset].as_chunks::<6>().0 {
                let pid = u16::from_le_bytes(entry[0..2].try_into().unwrap());
                let form = u16::from_le_bytes(entry[2..4].try_into().unwrap());
                let len = u16::from_le_bytes(entry[4..6].try_into().unwrap()) as usize;
                if pid == pid::ESSENCE_STREAM {
                    assert_eq!(form, store::SF_DATA_STREAM);
                    assert_eq!(props[offset], b'U', "opaque essence, independent of property-index byte order");
                    found = true;
                }
                offset += len;
            }
            assert!(found);
            assert_eq!(cf.read_path(&format!("{path}/Data-2702")).unwrap(), e.stream(pid::ESSENCE_STREAM).unwrap());
        }
    }
}

#[test]
fn audio_and_picture_use_distinct_masters_without_losing_source_identity() {
    let (p, s) = project();
    let bytes = export(&p, s, &AafOptions::default()).unwrap().0;
    let root = store::read(&bytes).unwrap();
    let content = root.strong(pid::ROOT_HEADER).unwrap().strong(pid::CONTENT).unwrap();
    let masters: Vec<_> = content.objs(pid::MOBS).iter().filter(|m| m.class == cls::MASTER_MOB).collect();
    assert_eq!(masters.len(), 2, "a first-slot reader must resolve the right media kind");
    for master in masters {
        let kinds: HashSet<_> = master
            .objs(pid::SLOTS)
            .iter()
            .filter_map(|s| s.strong(pid::SEGMENT))
            .filter_map(|s| s.weak_key(pid::DATA_DEFINITION))
            .filter(|k| *k == ddef::PICTURE || *k == ddef::SOUND)
            .collect();
        assert_eq!(kinds.len(), 1);
    }
    let document = read::read(&bytes, &mut Report::default()).unwrap();
    let picture = document.sources.iter().find(|s| s.kind == crate::comp::CKind::Picture).unwrap();
    let sound = document.sources.iter().find(|s| s.kind == crate::comp::CKind::Sound).unwrap();
    assert_eq!(picture.key, sound.key, "FilmCraft rejoins the original source group on import");
}

#[test]
fn operation_parameters_use_the_native_sdk_stored_set_form() {
    // The AAF baseline declares a vector, but the SDK's GetParameters enumerates
    // its legacy stored set. Native testing read zero parameters from vectors.
    for small in [false, true] {
        let root = store::read(&rich_export(small)).unwrap();
        let mut objects = Vec::new();
        walk(&root, &mut objects);
        let groups: Vec<_> = objects.into_iter().filter(|o| o.class == cls::OPERATION_GROUP && o.get(pid::PARAMETERS).is_some()).collect();
        assert!(!groups.is_empty());
        for group in groups {
            let Some(Value::StrongSet(parameters, key)) = group.get(pid::PARAMETERS) else {
                panic!("OperationGroup::Parameters must use the SDK's stored strong-reference set");
            };
            assert_eq!(*key, pid::PARAMETER_DEFINITION);
            assert!(!parameters.is_empty());
            assert!(parameters.iter().all(|p| p.auid(*key).is_some()));
        }
    }
}

#[test]
fn split_audio_channels_have_unambiguous_master_sources() {
    let (p, s) = project();
    let opts = AafOptions { media: crate::essence::MediaOptions { breakout_to_mono: true, ..Default::default() }, ..Default::default() };
    let bytes = export(&p, s, &opts).unwrap().0;
    let root = store::read(&bytes).unwrap();
    let content = root.strong(pid::ROOT_HEADER).unwrap().strong(pid::CONTENT).unwrap();
    let masters: Vec<_> = content
        .objs(pid::MOBS)
        .iter()
        .filter(|m| m.class == cls::MASTER_MOB)
        .filter(|m| m.objs(pid::SLOTS).iter().any(|s| s.strong(pid::SEGMENT).is_some_and(|g| g.weak_key(pid::DATA_DEFINITION) == Some(ddef::SOUND.as_slice()))))
        .collect();
    assert_eq!(masters.len(), 2, "each exposed channel has its own first source");
    assert!(masters.iter().all(|m| m.objs(pid::SLOTS).iter().filter(|s| s.strong(pid::SEGMENT).is_some_and(|g| g.class == cls::SOURCE_CLIP)).count() == 1));
    let doc = read::read(&bytes, &mut Report::default()).unwrap();
    let sources: Vec<_> = doc.sources.iter().filter(|s| s.kind == crate::comp::CKind::Sound).collect();
    assert_eq!(sources.iter().map(|s| s.channel).collect::<HashSet<_>>(), HashSet::from([Some(0), Some(1)]));
    assert_eq!(sources.iter().map(|s| &s.key).collect::<HashSet<_>>().len(), 1, "source identity survives the split");
}

#[test]
fn clip_gain_curves_are_bounded_scaled_and_reset_for_the_next_clip() {
    use crate::comp::{CItem, CKind, Gain, Nests};
    let (p, s) = project();
    let mut report = Report::default();
    let mut doc = crate::comp::from_project(&p, s, "gain", &Default::default(), Nests::Compositions, &mut report).unwrap();
    doc.compositions[0].tracks.retain(|t| t.kind == CKind::Sound);
    let track = &mut doc.compositions[0].tracks[0];
    track.items.retain(|i| matches!(i, CItem::Clip(_)));
    let CItem::Clip(first) = &mut track.items[0] else { panic!("first clip") };
    first.len = Tick::from_units(4, 1);
    first.gain = Some(Gain::Varying {
        linear: true,
        points: vec![(Tick::from_units(-1, 1), 1.0), (Tick::ZERO, 4.0), (Tick::from_units(1, 1), 8.0), (Tick::from_units(8, 1), 2.0)],
    });
    let CItem::Clip(second) = &mut track.items[1] else { panic!("second clip") };
    second.len = Tick::from_units(2, 1);
    second.gain = Some(Gain::Constant(0.5));
    let bytes = write::write(&doc, filmcraft_cfb::Version::V4, &mut report).unwrap();
    let root = store::read(&bytes).unwrap();
    let mut objects = Vec::new();
    walk(&root, &mut objects);
    let curves: Vec<_> = objects.iter().filter(|o| o.class == cls::VARYING_VALUE).collect();
    assert_eq!(curves.len(), 2, "the static neighbor resets host track automation to unity");
    for curve in &curves {
        let points = curve.objs(pid::POINT_LIST);
        assert_eq!(points.first().unwrap().rational(pid::CP_TIME).map(|(n, d)| n as f64 / d as f64), Some(0.0));
        let (n, d) = points.last().unwrap().rational(pid::CP_TIME).unwrap();
        let end = n as f64 / d as f64;
        assert!((1.0 - 1e-9..1.0).contains(&end), "adjacent clips need strictly ordered curve boundaries after floating-point time conversion");
        for point in points {
            let (n, d) = point.rational(pid::CP_TIME).unwrap();
            assert!(d > 0 && n >= 0 && n <= d, "keyframes outside the visible clip must be sampled at its boundaries");
            let value = point.data(pid::CP_VALUE).unwrap();
            let (n, d) = store::rational_of(&value[17..]).unwrap();
            assert!((0.0..=1.0).contains(&(n as f64 / d as f64)), "high boosts are carried by a constant gain factor");
        }
    }
    let imported = read::read(&bytes, &mut report).unwrap();
    let items = &imported.compositions[0].tracks[0].items;
    let CItem::Clip(first) = &items[0] else { panic!("first clip") };
    let Some(Gain::Varying { linear, points }) = &first.gain else { panic!("nested scalar and curve must be multiplied on import") };
    assert!(*linear);
    assert_eq!(points.iter().take(2).map(|p| p.0).collect::<Vec<_>>(), [Tick::ZERO, Tick::from_units(1, 1)]);
    assert!((points.last().unwrap().0 - Tick::from_units(4, 1)).abs() <= Tick(filmcraft_time::TICKS_PER_SECOND / 48_000 / 2));
    for (point, expected) in points.iter().zip([4.0, 8.0, 38.0 / 7.0]) {
        assert!((point.1 - expected).abs() <= 1.0 / 65536.0);
    }
    let CItem::Clip(second) = &items[1] else { panic!("second clip") };
    assert_eq!(second.gain, Some(Gain::Constant(0.5)));
}

#[test]
fn audio_gain_uses_the_registered_gain_id_instead_of_pan() {
    let root = store::read(&rich_export(false)).unwrap();
    let mut objects = Vec::new();
    walk(&root, &mut objects);
    let amplitude = ids::guid(0xE4962321, 0x2267, 0x11D3, [0x8A, 0x4C, 0x00, 0x50, 0x04, 0x0E, 0xF7, 0xD2]);
    let gain = ids::guid(0x9D2EA894, 0x0968, 0x11D3, [0x8A, 0x38, 0x00, 0x50, 0x04, 0x0E, 0xF7, 0xD2]);
    let groups: Vec<_> = objects
        .iter()
        .filter(|o| o.class == cls::OPERATION_GROUP && o.objs(pid::PARAMETERS).iter().any(|p| p.auid(pid::PARAMETER_DEFINITION) == Some(amplitude)))
        .collect();
    assert!(!groups.is_empty());
    for group in groups {
        assert_eq!(group.weak_key(pid::OPERATION), Some(gain.as_slice()));
    }
}

#[test]
fn audio_gain_import_distinguishes_legacy_gain_from_real_pan() {
    fn replace_id(o: &mut Obj, old: &[u8], new: &[u8]) {
        for (_, value) in &mut o.props {
            match value {
                Value::Data(bytes) | Value::Weak(_, _, bytes) if bytes == old => *bytes = new.to_vec(),
                Value::WeakVec(_, _, keys) => {
                    for key in keys {
                        if key == old {
                            *key = new.to_vec();
                        }
                    }
                }
                Value::Strong(child) => replace_id(child, old, new),
                Value::StrongVec(children) | Value::StrongSet(children, _) => {
                    for child in children {
                        replace_id(child, old, new);
                    }
                }
                _ => {}
            }
        }
    }
    let bytes = rich_export(false);
    let expected = read::read(&bytes, &mut Report::default()).unwrap();
    let mut legacy = store::read(&bytes).unwrap();
    replace_id(&mut legacy, &def::MONO_AUDIO_GAIN, &def::MONO_AUDIO_PAN);
    let legacy_bytes = store::write(&legacy, filmcraft_cfb::Version::V4).unwrap();
    let got = read::read(&legacy_bytes, &mut Report::default()).unwrap();
    assert_eq!(got.compositions[0].tracks, expected.compositions[0].tracks, "old FilmCraft gain is preserved");
    let pan = ids::guid(0xE4962322, 0x2267, 0x11D3, [0x8A, 0x4C, 0x00, 0x50, 0x04, 0x0E, 0xF7, 0xD2]);
    replace_id(&mut legacy, &def::PARAM_AMPLITUDE, &pan);
    let pan_bytes = store::write(&legacy, filmcraft_cfb::Version::V4).unwrap();
    let got = read::read(&pan_bytes, &mut Report::default()).unwrap();
    let clips: Vec<_> =
        got.compositions[0].tracks.iter().flat_map(|t| &t.items).filter_map(|i| if let crate::comp::CItem::Clip(c) = i { Some(c) } else { None }).collect();
    assert!(!clips.is_empty());
    assert!(clips.iter().all(|c| c.gain.is_none()), "real pan never becomes clip gain");
}

#[test]
fn original_wave_sources_use_the_probed_wave_format() {
    use filmcraft_media::MediaSource;
    for (float, bits) in [(false, 16u16), (true, 32)] {
        let mut bytes = crate::wav::wav_header(4 * u32::from(bits / 8), 1, 48_000, bits);
        bytes.extend(vec![0; 4 * usize::from(bits / 8)]);
        if float {
            bytes[20..22].copy_from_slice(&3u16.to_le_bytes());
        }
        let wave = filmcraft_media::wav::WavSource::parse("source.wav", bytes.into()).unwrap();
        let mut p = Project::new("wave");
        let a = p.add_item(
            "source.wav",
            filmcraft_project::Label::Iris,
            filmcraft_project::ItemKind::Media(Box::new(filmcraft_project::MediaClip {
                media: filmcraft_project::MediaRef::File { path: "/audio/source.wav".into() },
                info: wave.info().clone(),
                interpret: Default::default(),
                mark_in: None,
                mark_out: None,
                markers: vec![],
                offline: false,
                proxy: None,
                identity: None,
            })),
            None,
        );
        let s = p.new_sequence("S", SequenceSettings::default(), 0, 1, None);
        let clip = p.make_track_item(a, TrackKind::Audio, Tick::ZERO, TimeRange::new(Tick::ZERO, Tick::from_units(4, 48_000)), FrameRate::FPS_24).unwrap();
        p.sequence_mut(s).unwrap().audio_tracks[0].items.push(clip);
        let bytes = export(&p, s, &AafOptions::default()).unwrap().0;
        let root = store::read(&bytes).unwrap();
        let mut objects = Vec::new();
        walk(&root, &mut objects);
        let descriptor = objects.iter().find(|o| o.class == cls::WAVE_DESCRIPTOR).expect("original WAV keeps its WAVE descriptor");
        let summary = descriptor.data(pid::WAVE_SUMMARY).unwrap();
        assert_eq!(u16::from_le_bytes(summary[20..22].try_into().unwrap()), if float { 3 } else { 1 });
        assert_eq!(u16::from_le_bytes(summary[34..36].try_into().unwrap()), bits);
    }
}

#[test]
fn separate_wave_essence_declares_its_wave_codec() {
    use crate::essence::{AudioEssence, EssenceData, MediaOptions, NeedOptions, audio_needs};
    let (p, s) = project();
    let essence = audio_needs(&p, s, &NeedOptions::default())
        .iter()
        .map(|n| AudioEssence {
            key: n.key,
            channel: None,
            start: n.start,
            frames: ((n.end - n.start).0 / (filmcraft_time::TICKS_PER_SECOND / 48_000)) as u64,
            sample_rate: 48_000,
            bits: 24,
            channels: 2,
            data: EssenceData::File { path: "/audio/consolidated.wav".into() },
            effects_rendered: false,
            timing_rendered: false,
            render_gain: 1.0,
        })
        .collect();
    let opts = AafOptions { media: MediaOptions { essence, ..Default::default() }, ..Default::default() };
    let root = store::read(&export(&p, s, &opts).unwrap().0).unwrap();
    let header = root.strong(pid::ROOT_HEADER).unwrap();
    let content = header.strong(pid::CONTENT).unwrap();
    let descriptors: Vec<_> = content
        .objs(pid::MOBS)
        .iter()
        .filter_map(|m| m.strong(pid::ESSENCE_DESCRIPTION))
        .filter(|d| d.objs(pid::LOCATOR).iter().any(|l| l.string(pid::URL_STRING).is_some_and(|url| url.ends_with("consolidated.wav"))))
        .collect();
    assert!(!descriptors.is_empty());
    // MS-01 §11.1: WAVE files use WAVEDescriptor. Native Studio Pro testing also
    // isolates the WAVE codec reference: removing only it leaves the track empty.
    let wave_codec = ids::guid(0x820F09B1, 0xEB9B, 0x11D2, [0x80, 0x9F, 0x00, 0x60, 0x08, 0x14, 0x3E, 0x6F]);
    for descriptor in descriptors {
        assert_eq!(descriptor.weak_key(0x3005), Some(wave_codec.as_slice()), "FileDescriptor::CodecDefinition");
        assert_eq!(descriptor.class, cls::WAVE_DESCRIPTOR);
        let codec = header
            .strong(pid::DICTIONARY)
            .unwrap()
            .objs(0x2607)
            .iter()
            .find(|c| c.auid(pid::IDENTIFICATION) == Some(wave_codec))
            .expect("referenced WAVE codec is in the dictionary");
        assert_eq!(codec.weak_key(0x2301), Some(cls::WAVE_DESCRIPTOR.as_slice()));
        let (_, channels, rate, bits) = crate::wav::parse_wav(descriptor.data(pid::WAVE_SUMMARY).unwrap()).unwrap();
        assert_eq!((channels, rate, bits), (2, 48_000, 24));
    }
    let bytes = export(&p, s, &opts).unwrap().0;
    let doc = read::read(&bytes, &mut Report::default()).unwrap();
    for source in doc.sources.iter().filter(|s| s.kind == crate::comp::CKind::Sound) {
        assert_eq!((source.file_channels, source.sample_rate, source.bits), (2, 48_000, 24));
    }
    let opts = AafOptions { media: MediaOptions { breakout_to_mono: true, ..opts.media }, ..opts };
    let bytes = export(&p, s, &opts).unwrap().0;
    let doc = read::read(&bytes, &mut Report::default()).unwrap();
    let channels: HashSet<_> = doc.sources.iter().filter(|s| s.kind == crate::comp::CKind::Sound).map(|s| s.channel).collect();
    assert_eq!(channels, HashSet::from([Some(0), Some(1)]), "WAVE summary channels are resolved before selecting mono slots");
}

#[test]
fn oversized_wave_summary_returns_an_error_without_allocating_audio() {
    let (p, s) = project();
    let mut report = Report::default();
    let mut doc = crate::comp::from_project(&p, s, "wave", &Default::default(), crate::comp::Nests::Compositions, &mut report).unwrap();
    for source in doc.sources.iter_mut().filter(|s| s.kind == crate::comp::CKind::Sound) {
        source.path = Some("/audio/huge.wav".into());
        source.wave_format = Some(1);
        source.length = Tick::from_units(86_400, 1);
    }
    assert!(write::write(&doc, filmcraft_cfb::Version::V4, &mut report).unwrap_err().to_string().contains("RIFF size limit"));
}

#[test]
fn storage_names_match_the_sdk_property_names() {
    let bytes = rich_export(false);
    let cf = filmcraft_cfb::CompoundFile::open(&bytes).unwrap();
    for expected in [
        "Identifi-ionList-3b06 index",
        "Operatio-nitions-2603 index",
        "Paramete-nitions-2604 index",
        "Containe-nitions-2608 index",
        "Interpol-nitions-2609 index",
        "UserComments-204 index",
        "Paramete-Defined-1e09 index",
    ] {
        assert!(cf.entries.iter().any(|e| e.name == expected), "native SDK storage {expected} is missing");
    }
}

#[test]
fn progressive_video_line_map_has_one_packed_scan_line() {
    let (p, s) = project();
    let root = store::read(&export(&p, s, &AafOptions::default()).unwrap().0).unwrap();
    let mut objects = Vec::new();
    walk(&root, &mut objects);
    let descriptors: Vec<_> = objects.into_iter().filter(|o| o.class == cls::CDCI_DESCRIPTOR).collect();
    assert!(!descriptors.is_empty());
    for descriptor in descriptors {
        assert_eq!(descriptor.u8(pid::FRAME_LAYOUT), Some(0));
        assert_eq!(descriptor.data(pid::VIDEO_LINE_MAP), Some(0i32.to_le_bytes().as_slice()));
    }
}

#[test]
fn written_files_have_the_required_structure() {
    let (p, s) = project();
    for small in [false, true] {
        let (bytes, _) = export(&p, s, &AafOptions { small_sectors: small, ..Default::default() }).unwrap();
        assert_written_file(&bytes, small);
    }
}

/// The top-level composition is tagged `Usage_TopLevel`, which the SMPTE Labels register gives as
/// urn:smpte:ul:060e2b34.04010101.0d010102.01010700, stored as the AUID
/// {0d010102-0101-0700-060e-2b3404010101}. Readers that look for it (LibAAF) find no sequence
/// without it.
#[test]
fn the_top_level_composition_has_the_registered_usage_code() {
    let (p, s) = project();
    let (bytes, _) = export(&p, s, &AafOptions::default()).unwrap();
    let root = store::read(&bytes).unwrap();
    let content = root.strong(pid::ROOT_HEADER).unwrap().strong(pid::CONTENT).unwrap();
    let comp = content.objs(pid::MOBS).iter().find(|m| m.class == cls::COMPOSITION_MOB).unwrap();
    let usage = [0x02, 0x01, 0x01, 0x0D, 0x01, 0x01, 0x00, 0x07, 0x06, 0x0E, 0x2B, 0x34, 0x04, 0x01, 0x01, 0x01];
    assert_eq!(comp.data(pid::USAGE_CODE), Some(&usage[..]));
}

/// Files from FilmCraft 0.16 and earlier tag the top-level composition with the unregistered label
/// 060e2b34.04010105.0d011201.02030000. They still import: the composition no other one uses is
/// the edit.
#[test]
fn files_with_the_old_usage_code_still_import() {
    let (p, s) = project();
    let (bytes, _) = export(&p, s, &AafOptions::default()).unwrap();
    let mut root = store::read(&bytes).unwrap();
    let content = strong_mut(strong_mut(&mut root, pid::ROOT_HEADER), pid::CONTENT);
    for mob in objs_mut(content, pid::MOBS).iter_mut().filter(|m| m.class == cls::COMPOSITION_MOB) {
        mob.set(pid::USAGE_CODE, Value::Data(def::USAGE_TOP_LEVEL_OLD.to_vec()));
    }
    let old_file = store::write(&root, filmcraft_cfb::Version::V4).unwrap();
    let (got, _, report) = import(&old_file, &ImportOptions::default()).unwrap();
    assert!(!report.has_warnings(), "{report}");
    let (want, _, _) = import(&bytes, &ImportOptions::default()).unwrap();
    let clips = |imp: &Imported| {
        let q = imp.project.sequence(imp.sequences[0]).unwrap();
        q.video_tracks.iter().chain(&q.audio_tracks).map(|t| t.items.iter().map(|c| (c.start, c.duration, c.source_in)).collect::<Vec<_>>()).collect::<Vec<_>>()
    };
    assert_eq!((got.sequences.len(), clips(&got)), (1, clips(&want)));
}

/// A file that holds one edit tagged with each label (a tool merged a composition from an older
/// FilmCraft file into a newer one) imports both edits.
#[test]
fn edits_tagged_with_either_usage_code_both_import() {
    let (p, s) = project();
    let (bytes, _) = export(&p, s, &AafOptions::default()).unwrap();
    let mut root = store::read(&bytes).unwrap();
    let content = strong_mut(strong_mut(&mut root, pid::ROOT_HEADER), pid::CONTENT);
    let mobs = objs_mut(content, pid::MOBS);
    let mut old = mobs.iter().find(|m| m.class == cls::COMPOSITION_MOB).unwrap().clone();
    let mut id = old.mob_id(pid::MOB_ID).unwrap();
    id[31] ^= 0xFF;
    old.set(pid::MOB_ID, Value::Data(id.to_vec()));
    old.set(pid::MOB_NAME, Value::Data(store::utf16z("Old")));
    old.set(pid::USAGE_CODE, Value::Data(def::USAGE_TOP_LEVEL_OLD.to_vec()));
    mobs.push(old);
    let mixed = store::write(&root, filmcraft_cfb::Version::V4).unwrap();
    let (imported, _, report) = import(&mixed, &ImportOptions::default()).unwrap();
    assert!(!report.has_warnings(), "{report}");
    let mut names: Vec<String> = imported.sequences.iter().map(|&q| imported.project.item(q).unwrap().name.clone()).collect();
    names.sort();
    assert_eq!(names, ["Old", "S"]);
}

fn assert_written_file(bytes: &[u8], small: bool) {
    assert_compound_file(bytes, small);
    let root = store::read(bytes).unwrap();
    assert!(root.strong(pid::ROOT_META_DICTIONARY).is_some());
    let header = root.strong(pid::ROOT_HEADER).unwrap();
    let content = assert_header(header);
    let dict = header.strong(pid::DICTIONARY).unwrap();
    let definitions = Definitions {
        data: keys(dict, pid::DATA_DEFINITIONS),
        operations: keys(dict, pid::OPERATION_DEFINITIONS),
        interpolation: keys(dict, pid::INTERPOLATION_DEFINITIONS),
        containers: keys(dict, pid::CONTAINER_DEFINITIONS),
        parameters: keys(dict, pid::PARAMETER_DEFINITIONS),
    };
    let mobs = content.objs(pid::MOBS);
    let comp = assert_mob_set(content, mobs);
    for mob in mobs {
        assert_mob(mob, mobs, &definitions);
    }
    assert_composition(comp);
}

fn assert_compound_file(bytes: &[u8], small: bool) {
    let cf = filmcraft_cfb::CompoundFile::open(bytes).unwrap();
    assert_eq!(cf.version, if small { 3 } else { 4 });
    assert_eq!(cf.root().clsid, ids::ROOT);
    let references = cf.read_path("referenced properties").unwrap();
    let pids: Vec<u16> = references[7..].as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    assert!(pids.windows(3).any(|w| w == path::DATA_DEFS));
    let header = cf.find("Header-2").unwrap();
    assert_eq!(cf.entries[header].clsid, cls::HEADER);
    assert!(cf.read_path("Header-2/properties").unwrap().starts_with(&[0x4C, 0x20]));
}

fn assert_header(header: &Obj) -> &Obj {
    for req in [pid::BYTE_ORDER, pid::LAST_MODIFIED, pid::CONTENT, pid::DICTIONARY, pid::VERSION, pid::IDENTIFICATION_LIST, pid::OBJECT_MODEL_VERSION] {
        assert!(header.get(req).is_some(), "Header property {req:#06x}");
    }
    assert_eq!(header.auid(pid::OPERATIONAL_PATTERN), Some(def::OP_EDIT_PROTOCOL));
    let ident = &header.objs(pid::IDENTIFICATION_LIST)[0];
    for req in [pid::COMPANY_NAME, pid::PRODUCT_NAME, pid::PRODUCT_VERSION_STRING, pid::PRODUCT_ID, pid::DATE, pid::GENERATION_AUID] {
        assert!(ident.get(req).is_some(), "Identification property {req:#06x}");
    }
    header.strong(pid::CONTENT).unwrap()
}

struct Definitions {
    data: HashSet<Vec<u8>>,
    operations: HashSet<Vec<u8>>,
    interpolation: HashSet<Vec<u8>>,
    containers: HashSet<Vec<u8>>,
    parameters: HashSet<Vec<u8>>,
}

fn assert_mob_set<'a>(content: &Obj, mobs: &'a [Obj]) -> &'a Obj {
    let mob_ids: Vec<[u8; 32]> = mobs.iter().map(|m| m.mob_id(pid::MOB_ID).unwrap()).collect();
    assert_eq!(mob_ids.iter().collect::<HashSet<_>>().len(), mob_ids.len(), "unique mob ids");
    assert!(matches!(content.get(pid::MOBS), Some(Value::StrongSet(_, k)) if *k == pid::MOB_ID));
    let compositions: Vec<&Obj> = mobs.iter().filter(|m| m.class == cls::COMPOSITION_MOB).collect();
    assert_eq!(compositions.len(), 1);
    assert_eq!(compositions[0].auid(pid::USAGE_CODE), Some(def::USAGE_TOP_LEVEL));
    assert_eq!(mobs.iter().filter(|m| m.class == cls::MASTER_MOB).count(), 2);
    assert_eq!(mobs.iter().filter(|m| m.class == cls::SOURCE_MOB).count(), 4, "picture and sound each have a file and tape source");
    compositions[0]
}

fn assert_mob(mob: &Obj, mobs: &[Obj], definitions: &Definitions) {
    for req in [pid::MOB_ID, pid::MOB_NAME, pid::SLOTS, pid::MOB_LAST_MODIFIED, pid::MOB_CREATION_TIME] {
        assert!(mob.get(req).is_some(), "Mob property {req:#06x}");
    }
    assert_source_descriptor(mob, &definitions.containers);
    for slot in mob.objs(pid::SLOTS) {
        assert!(slot.get(pid::SLOT_ID).is_some() && slot.get(pid::SEGMENT).is_some());
        if slot.class == cls::TIMELINE_MOB_SLOT {
            assert!(slot.rational(pid::EDIT_RATE).is_some_and(|r| r.0 > 0 && r.1 > 0));
            assert!(slot.get(pid::ORIGIN).is_some());
        }
    }
    let mut all = Vec::new();
    walk(mob, &mut all);
    for component in all {
        assert_component(component, mobs, definitions);
    }
}

fn assert_source_descriptor(mob: &Obj, containers: &HashSet<Vec<u8>>) {
    if mob.class != cls::SOURCE_MOB {
        return;
    }
    let descriptor = mob.strong(pid::ESSENCE_DESCRIPTION).expect("SourceMob::EssenceDescription");
    if descriptor.class != cls::TAPE_DESCRIPTOR {
        for req in [pid::SAMPLE_RATE, pid::FILE_LENGTH, pid::CONTAINER_FORMAT] {
            assert!(descriptor.get(req).is_some(), "FileDescriptor property {req:#06x}");
        }
        assert!(containers.contains(descriptor.weak_key(pid::CONTAINER_FORMAT).unwrap()));
        let url = descriptor.objs(pid::LOCATOR)[0].string(pid::URL_STRING).unwrap();
        assert_eq!(url, "file:///m/a.mov");
    }
    if descriptor.class == cls::PCM_DESCRIPTOR {
        assert_eq!(descriptor.u32(pid::CHANNELS), Some(2));
        assert_eq!(descriptor.u32(pid::QUANTIZATION_BITS), Some(24));
        assert_eq!(descriptor.u16(pid::BLOCK_ALIGN), Some(6));
    }
}

fn assert_component(o: &Obj, mobs: &[Obj], definitions: &Definitions) {
    if is_component(o) {
        let dd = o.weak_key(pid::DATA_DEFINITION).expect("Component::DataDefinition");
        assert!(definitions.data.contains(dd), "data definition in the dictionary");
        let event_seq = o.class == cls::SEQUENCE && dd == ddef::DESCRIPTIVE_METADATA;
        assert!(event_seq || o.get(pid::LENGTH).is_some(), "Component::Length");
    }
    if o.class == cls::SOURCE_CLIP {
        assert_source_clip(o, mobs);
    } else if o.class == cls::OPERATION_GROUP {
        assert_operation_group(o, definitions);
    } else if o.class == cls::SEQUENCE && o.get(pid::LENGTH).is_some() {
        assert_sequence_length(o);
    }
}

fn is_component(o: &Obj) -> bool {
    [cls::FILLER, cls::SOURCE_CLIP, cls::SEQUENCE, cls::TRANSITION, cls::OPERATION_GROUP, cls::TIMECODE, cls::COMMENT_MARKER].contains(&o.class)
}

fn assert_source_clip(clip: &Obj, mobs: &[Obj]) {
    let id = clip.mob_id(pid::SOURCE_ID).unwrap();
    let slot = clip.u32(pid::SOURCE_MOB_SLOT_ID).unwrap();
    assert!(clip.get(pid::START_TIME).is_some());
    if id != [0; 32] {
        let target = mobs.iter().find(|m| m.mob_id(pid::MOB_ID) == Some(id)).expect("source clip target in the file");
        assert!(target.objs(pid::SLOTS).iter().any(|s| s.u32(pid::SLOT_ID) == Some(slot)), "target slot exists");
    }
}

fn assert_operation_group(group: &Obj, definitions: &Definitions) {
    assert!(definitions.operations.contains(group.weak_key(pid::OPERATION).unwrap()));
    for parameter in group.objs(pid::PARAMETERS) {
        assert!(definitions.parameters.contains(parameter.auid(pid::PARAMETER_DEFINITION).unwrap().as_slice()));
        if parameter.class == cls::VARYING_VALUE {
            assert!(definitions.interpolation.contains(parameter.weak_key(pid::INTERPOLATION).unwrap()));
            assert!(!parameter.objs(pid::POINT_LIST).is_empty());
        }
    }
}

fn assert_sequence_length(sequence: &Obj) {
    let components = sequence.objs(pid::COMPONENTS);
    let sum = components
        .iter()
        .enumerate()
        .map(|(i, component)| {
            let len = component.i64(pid::LENGTH).unwrap();
            if component.class == cls::TRANSITION {
                assert!(i > 0 && i + 1 < components.len(), "transition at a sequence end");
                assert!(components[i - 1].class != cls::TRANSITION && components[i + 1].class != cls::TRANSITION);
                assert!(components[i - 1].i64(pid::LENGTH).unwrap() >= len && components[i + 1].i64(pid::LENGTH).unwrap() >= len);
                let cut = component.i64(pid::CUT_POINT).unwrap();
                assert!((0..=len).contains(&cut));
                assert!(component.strong(pid::OPERATION_GROUP).is_some());
                -len
            } else {
                len
            }
        })
        .sum::<i64>();
    assert_eq!(sum, sequence.i64(pid::LENGTH).unwrap(), "sequence length = segments − transitions");
}

fn assert_composition(comp: &Obj) {
    let slots = comp.objs(pid::SLOTS);
    let tc = slots[0].strong(pid::SEGMENT).unwrap();
    assert_eq!(tc.class, cls::TIMECODE);
    assert_eq!(tc.u16(pid::TC_FPS), Some(24));
    let video = slots[1].strong(pid::SEGMENT).unwrap().objs(pid::COMPONENTS);
    assert_eq!(video.iter().map(|c| c.class).collect::<Vec<_>>(), vec![cls::SOURCE_CLIP, cls::TRANSITION, cls::SOURCE_CLIP]);
    assert_eq!(video[0].i64(pid::LENGTH), Some(54), "outgoing clip extends to the transition end");
    assert_eq!(video[2].i64(pid::LENGTH), Some(54), "incoming clip starts at the transition start");
    assert_eq!(video[2].i64(pid::START_TIME), Some(194));
    assert_eq!(video[1].i64(pid::CUT_POINT), Some(6));
    assert_eq!(video[1].strong(pid::OPERATION_GROUP).unwrap().weak_key(pid::OPERATION), Some(&def::VIDEO_DISSOLVE[..]));
    assert_audio_composition_slot(&slots[2]);
}

fn assert_audio_composition_slot(slot: &Obj) {
    let audio = slot.strong(pid::SEGMENT).unwrap().objs(pid::COMPONENTS);
    assert_eq!(slot.rational(pid::EDIT_RATE), Some((48_000, 1)));
    assert_eq!(audio[2].class, cls::OPERATION_GROUP);
    assert_eq!(audio[2].weak_key(pid::OPERATION), Some(&def::MONO_AUDIO_GAIN[..]));
    let amplitude = audio[2].objs(pid::PARAMETERS)[0].data(pid::CONSTANT_VALUE).unwrap();
    let (numerator, denominator) = store::rational_of(&amplitude[17..]).unwrap();
    assert!((numerator as f64 / denominator as f64 - 10f64.powf(-3.0 / 20.0)).abs() < 1e-4);
    assert_eq!(audio[1].strong(pid::OPERATION_GROUP).unwrap().weak_key(pid::OPERATION), Some(&def::MONO_AUDIO_DISSOLVE[..]));
}

#[test]
fn store_round_trips_every_stored_form() {
    let leaf = |n: u8| Obj::new([n; 16]).data_prop(0x1B01, vec![n; 16]);
    let root = Obj::new(ids::ROOT)
        .data_prop(0x0003, vec![1, 2, 3])
        .with(0x0004, Value::Strong(Box::new(leaf(1).with(0x0005, Value::StrongVec(vec![leaf(2), leaf(3)])))))
        .with(
            0x0002,
            Value::Strong(Box::new(
                Obj::new([9; 16]).with(0x3B04, Value::Strong(Box::new(Obj::new([8; 16]).with(0x2605, Value::StrongSet(vec![leaf(4), leaf(5)], 0x1B01))))),
            )),
        )
        .with(0x0006, Value::Weak(path::DATA_DEFS.to_vec(), 0x1B01, vec![5; 16]))
        .with(0x0007, Value::WeakVec(path::DATA_DEFS.to_vec(), 0x1B01, vec![vec![4; 16], vec![5; 16]]))
        .with(0x0008, Value::Stream((0..10_000u32).map(|i| i as u8).collect()));
    for v in [filmcraft_cfb::Version::V3, filmcraft_cfb::Version::V4] {
        let bytes = store::write(&root, v).unwrap();
        assert_eq!(store::read(&bytes).unwrap(), root);
    }
    // a data value longer than a property can hold is an error, not a panic
    assert!(store::write(&Obj::new(ids::ROOT).data_prop(1, vec![0; 70_000]), filmcraft_cfb::Version::V4).is_err());
}

/// The property `p` of `o` as a list of objects, to change.
fn objs_mut(o: &mut Obj, p: u16) -> &mut Vec<Obj> {
    match o.props.iter_mut().find(|(k, _)| *k == p).map(|(_, v)| v) {
        Some(Value::StrongVec(v)) | Some(Value::StrongSet(v, _)) => v,
        _ => panic!("no object list {p:#06x}"),
    }
}

fn strong_mut(o: &mut Obj, p: u16) -> &mut Obj {
    match o.props.iter_mut().find(|(k, _)| *k == p).map(|(_, v)| v) {
        Some(Value::Strong(v)) => v,
        _ => panic!("no object {p:#06x}"),
    }
}

fn is_picture(o: &Obj) -> bool {
    o.weak_key(pid::DATA_DEFINITION) == Some(&ddef::PICTURE[..])
}

/// How Premiere Pro 26.5.2 lays a sequence with a nested sequence out in AAF (seen in a file it
/// exported): every composition is tagged top-level, the nested one too; all video tracks of a
/// composition are in ONE slot, a nested scope with a segment per track, lowest first; a clip of
/// the nested sequence is a source clip that points at its composition. Our own file is rewritten
/// into that layout and must read back as the same edit.
#[test]
fn premiere_style_layers_and_nested_compositions_are_read() {
    let (mut p, inner) = project();
    let r = FrameRate::FPS_24;
    let outer = p.new_sequence("Outer", SequenceSettings { frame_rate: r, ..Default::default() }, 3, 1, None);
    let media = p.sequence(inner).unwrap().video_tracks[0].items[0].item;
    for (kind, track, item, start, dur, src) in [
        (TrackKind::Video, 0, media, 0, 30, 5),
        (TrackKind::Video, 1, inner, 10, 40, 12),
        (TrackKind::Audio, 0, inner, 10, 40, 12),
        (TrackKind::Video, 2, media, 60, 20, 100),
    ] {
        let ti = p.make_track_item(item, kind, r.tick_of(start), TimeRange::new(r.tick_of(src), r.tick_of(dur)), r).unwrap();
        p.sequence_mut(outer).unwrap().tracks_mut(kind)[track].items.push(ti);
    }
    let (ours, report) = export(&p, outer, &AafOptions::default()).unwrap();
    assert!(!report.has_warnings(), "{report}");

    let mut root = store::read(&ours).unwrap();
    let content = strong_mut(strong_mut(&mut root, pid::ROOT_HEADER), pid::CONTENT);
    let mut compositions = 0;
    for mob in objs_mut(content, pid::MOBS).iter_mut().filter(|m| m.class == cls::COMPOSITION_MOB) {
        compositions += 1;
        mob.set(pid::USAGE_CODE, Value::Data(def::USAGE_TOP_LEVEL.to_vec()));
        let slots = objs_mut(mob, pid::SLOTS);
        let picture: Vec<usize> = slots.iter().enumerate().filter(|(_, s)| s.strong(pid::SEGMENT).is_some_and(is_picture)).map(|(i, _)| i).collect();
        let layers: Vec<Obj> = picture.iter().map(|&i| slots[i].strong(pid::SEGMENT).unwrap().clone()).collect();
        let length = layers.iter().filter_map(|l| l.i64(pid::LENGTH)).max().unwrap_or(0);
        let scope = Obj::new(cls::NESTED_SCOPE)
            .with(pid::DATA_DEFINITION, Value::Weak(path::DATA_DEFS.to_vec(), pid::IDENTIFICATION, ddef::PICTURE.to_vec()))
            .data_prop(pid::LENGTH, length.to_le_bytes().to_vec())
            .with(pid::NESTED_SLOTS, Value::StrongVec(layers));
        // the first video slot keeps its id (clips of the nested sequence point at it) and holds
        // every layer; the other video slots go
        slots[picture[0]].set(pid::SEGMENT, Value::Strong(Box::new(scope)));
        for &i in picture[1..].iter().rev() {
            slots.remove(i);
        }
    }
    assert_eq!(compositions, 2);
    let theirs = store::write(&root, filmcraft_cfb::Version::V4).unwrap();
    assert_ne!(theirs, ours);

    let opts = ImportOptions::default();
    let (want, _, _) = import(&ours, &opts).unwrap();
    let (got, _, report) = import(&theirs, &opts).unwrap();
    assert!(!report.has_warnings(), "{report}");
    // one edit; the nested composition is a sequence it uses, not a second edit
    assert_eq!(got.sequences.len(), 1);
    let shape =
        |imp: &Imported| {
            let mut out = Vec::new();
            for item in imp.project.items.values() {
                let Some(q) = imp.project.sequence(item.id) else { continue };
                for (kind, tracks) in [("V", &q.video_tracks), ("A", &q.audio_tracks)] {
                    for (n, t) in tracks.iter().enumerate() {
                        for c in &t.items {
                            let source = imp.project.item(c.item).map(|i| i.name.clone()).unwrap_or_default();
                            out.push((item.name.clone(), format!("{kind}{}", n + 1), source, c.start, c.duration, c.source_in, c.link.is_some()));
                        }
                        out.extend(t.transitions.iter().map(|x| {
                            (item.name.clone(), format!("{kind}{} transition", n + 1), x.effect.effect.clone(), x.start, x.duration, Tick::ZERO, false)
                        }));
                    }
                }
            }
            out.sort();
            out
        };
    assert_eq!(shape(&got), shape(&want));
    // and that edit is the one that was exported: three video tracks, the nest on the second
    let q = got.project.sequence(got.sequences[0]).unwrap();
    assert_eq!(q.video_tracks.iter().map(|t| t.items.len()).collect::<Vec<_>>(), [1, 1, 1]);
    let nest = &q.video_tracks[1].items[0];
    assert_eq!(got.project.item(nest.item).unwrap().name, "S");
    assert_eq!((nest.start, nest.duration, nest.source_in), (r.tick_of(10), r.tick_of(40), r.tick_of(12)));
    assert_eq!(nest.link, q.audio_tracks[0].items[0].link);
    assert!(nest.link.is_some());
}

#[test]
fn audio_tracks_keep_their_names() {
    // A track named as a role (what a sound editor's DAW should show), whole and broken out.
    let (mut p, s) = project();
    p.sequence_mut(s).unwrap().audio_tracks[0].name = "Dialogue.Dialogue-1".into();
    let names = |p: &Project, opts: &AafOptions| {
        let (bytes, _) = export(p, s, opts).unwrap();
        let (imported, _, _) = import(&bytes, &ImportOptions::default()).unwrap();
        let seq = imported.project.sequence(imported.sequences[0]).unwrap();
        seq.audio_tracks.iter().map(|t| t.name.clone()).collect::<Vec<_>>()
    };
    assert_eq!(names(&p, &AafOptions::default()), ["Dialogue.Dialogue-1"]);
    let mut broken = AafOptions::default();
    broken.media.breakout_to_mono = true;
    assert_eq!(names(&p, &broken), ["Dialogue.Dialogue-1 L", "Dialogue.Dialogue-1 R"]);
    // No name: written numbered (`A1`), as before, and read back with the reader's own name.
    p.sequence_mut(s).unwrap().audio_tracks[0].name.clear();
    assert_eq!(names(&p, &AafOptions::default()), ["Audio 1"], "the reader's own name");
    // Names that start with a character of more than one byte come back as they are.
    for name in ["é", "音声", "🎤 Dialogue", "Ä1"] {
        p.sequence_mut(s).unwrap().audio_tracks[0].name = name.into();
        assert_eq!(names(&p, &AafOptions::default()), [name]);
    }
}

#[test]
fn interleaved_audio_exposes_every_file_channel_and_round_trips_as_stereo() {
    let (p, s) = project();
    for bytes in [export(&p, s, &AafOptions::default()).unwrap().0, rich_export(false)] {
        let root = store::read(&bytes).unwrap();
        let content = root.strong(pid::ROOT_HEADER).unwrap().strong(pid::CONTENT).unwrap();
        let file = content
            .objs(pid::MOBS)
            .iter()
            .find(|m| {
                m.class == cls::SOURCE_MOB && m.strong(pid::ESSENCE_DESCRIPTION).is_some_and(|d| matches!(d.class, cls::PCM_DESCRIPTOR | cls::WAVE_DESCRIPTOR))
            })
            .unwrap();
        let slots = file.objs(pid::SLOTS);
        assert_eq!(slots.len(), 2, "a single file slot makes Studio Pro import stereo essence as mono");
        assert_eq!(slots.iter().map(|slot| slot.u32(pid::PHYSICAL_TRACK_NUMBER)).collect::<Vec<_>>(), [Some(1), Some(2)]);
        let (imported, _, _) = import(&bytes, &ImportOptions::default()).unwrap();
        let seq = imported.project.sequence(imported.sequences[0]).unwrap();
        assert_eq!(seq.audio_tracks[0].channels, filmcraft_project::AudioChannels::Stereo);
        let doc = super::read::read(&bytes, &mut Report::default()).unwrap();
        assert!(doc.sources.iter().filter(|s| s.kind == crate::comp::CKind::Sound).all(|s| s.channel.is_none() && s.channels == 2));
    }
}

#[test]
fn audio_channel_slot_expansion_rejects_hostile_metadata() {
    let (p, s) = project();
    let bytes = export(&p, s, &AafOptions::default()).unwrap().0;
    let original = super::read::read(&bytes, &mut Report::default()).unwrap();
    for (channels, channel) in [(65, None), (u32::MAX, None), (2, Some(u32::MAX))] {
        let mut doc = original.clone();
        let source = doc.sources.iter_mut().find(|s| s.kind == crate::comp::CKind::Sound).unwrap();
        source.file_channels = channels;
        source.channel = channel;
        assert!(write::write(&doc, filmcraft_cfb::Version::V4, &mut Report::default()).unwrap_err().to_string().contains("channels"));
    }
}

/// MS-01 §6.1 requires ObjClass in the class model even though the stored-format mapping
/// carries each instance's value in its storage CLSID rather than its properties stream.
#[test]
fn class_model_declares_required_object_class_and_its_reference_type() {
    let (p, s) = project();
    for small_sectors in [false, true] {
        let bytes = export(&p, s, &AafOptions { small_sectors, ..Default::default() }).unwrap().0;
        let root = store::read(&bytes).unwrap();
        let model = root.strong(pid::ROOT_META_DICTIONARY).unwrap();
        let base = model.objs(pid::META_CLASS_DEFINITIONS).iter().find(|c| c.auid(pid::META_IDENTIFICATION) == Some(ids::class(1))).unwrap();
        let prop = base
            .objs(pid::CLASS_PROPERTIES)
            .iter()
            .find(|p| p.u16(pid::PROPERTY_LOCAL_ID) == Some(0x0101))
            .expect("the required InterchangeObject.ObjClass definition is missing");
        assert_eq!(prop.u8(pid::PROPERTY_IS_OPTIONAL), Some(0));
        assert_eq!(prop.auid(pid::META_IDENTIFICATION), Some(ids::guid(0x06010104, 0x0101, 0, [6, 14, 43, 52, 1, 1, 1, 2])));
        let ty_id = ids::guid(0x05010100, 0, 0, [6, 14, 43, 52, 1, 4, 1, 1]);
        assert_eq!(prop.auid(pid::PROPERTY_TYPE), Some(ty_id));
        let ty = model.objs(pid::META_TYPE_DEFINITIONS).iter().find(|t| t.auid(pid::META_IDENTIFICATION) == Some(ty_id)).unwrap();
        assert_eq!(ty.class, ids::meta::TYPE_WEAK_REFERENCE);
        assert_eq!(weak_target(ty.get(pid::WEAK_REFERENCED_TYPE), path::META_CLASSES), ids::meta::CLASS_DEFINITION);
        let header = root.strong(pid::ROOT_HEADER).unwrap();
        assert_eq!(header.class, cls::HEADER);
        assert!(header.get(0x0101).is_none(), "ObjClass stays in the storage CLSID, not a duplicate physical property");
    }
}
