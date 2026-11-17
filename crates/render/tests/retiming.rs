//! Audio timing must survive the different buffer sizes used by playback and interchange.

use std::sync::Arc;

use filmcraft_media::generators::GeneratorSource;
use filmcraft_media::{Generator, MediaSource};
use filmcraft_project::{ItemKind, Label, MediaClip, MediaRef, Project, SequenceSettings, TrackKind};
use filmcraft_render::{SourceMap, audio::track_input};
use filmcraft_time::{FrameRate, Tick, TimeRange};

#[test]
fn varispeed_audio_is_independent_of_read_partitioning() {
    for (speed, reverse) in [(0.85, false), (1.25, false), (0.85, true), (1.5, true), (f64::MAX, false), (f64::MAX, true)] {
        let source = GeneratorSource::new(Generator::Tone { hz: 997.0, db: -6.0 }, 1, 1, FrameRate::FPS_24, Tick::from_units(4, 1));
        let mut p = Project::new("partitioned retiming");
        let item = p.add_item(
            "tone",
            Label::Iris,
            ItemKind::Media(Box::new(MediaClip {
                media: MediaRef::Generator(source.generator.clone()),
                info: source.info().clone(),
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
        let seq = p.new_sequence("mix", SequenceSettings::default(), 0, 1, None);
        let mut clip = p
            .make_track_item(
                item,
                TrackKind::Audio,
                Tick::from_units(130, 48_000),
                TimeRange::new(Tick::from_units(24_003, 48_000), Tick::from_units(48_000, 48_000)),
                FrameRate::FPS_24,
            )
            .unwrap();
        clip.speed = speed;
        clip.reverse = reverse;
        clip.maintain_pitch = false;
        p.sequence_mut(seq).unwrap().audio_tracks[0].items.push(clip);
        let mut sources = SourceMap::default();
        sources.0.insert(item, Arc::new(source));
        let track = &p.sequence(seq).unwrap().audio_tracks[0];
        let whole = track_input(&p, track, 130, 4800, 48_000, &sources);
        let mut pieces = vec![Vec::new(), Vec::new()];
        for offset in (0..4800).step_by(257) {
            let b = track_input(&p, track, 130 + offset as i64, 257.min(4800 - offset), 48_000, &sources);
            for (dst, src) in pieces.iter_mut().zip(b.channels) {
                dst.extend(src);
            }
        }
        let error = whole.channels.iter().zip(pieces).flat_map(|(a, b)| a.iter().zip(b).map(|(a, b)| (a - b).abs())).fold(0.0f32, f32::max);
        assert!(error < 2e-6, "{speed}x, reversed={reverse}: max sample error {error}");
    }
}
