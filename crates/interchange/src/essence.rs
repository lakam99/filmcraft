//! Media options shared by the AAF and OMF exporters, and the audio essence the caller supplies.
//!
//! This crate does no file I/O and decodes no media. An export that embeds audio, consolidates
//! (trims) it into new files, renders clip effects or mixes the video down works in three steps:
//!
//! 1. [`audio_needs`] lists the audio ranges the document will reference (with handles);
//! 2. the caller (the engine) decodes / renders each range and writes or keeps the PCM;
//! 3. the caller passes the results as [`AudioEssence`] in [`MediaOptions::essence`] (and a
//!    rendered video file as [`MediaOptions::mixdown_video`]) to [`crate::aaf::export`] /
//!    [`crate::omf::export`].

use std::collections::{BTreeMap, BTreeSet};

use filmcraft_project::{ClipId, ItemId, ItemKind, MediaRef, Project, TrackKind};
use filmcraft_time::Tick;
use serde::{Deserialize, Serialize};

/// What a piece of essence stands in for: a whole media item, or one clip's rendered audio.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum EssenceKey {
    Media(ItemId),
    MediaStream { item: ItemId, stream: usize },
    Clip(ClipId),
}

impl EssenceKey {
    pub fn media(item: ItemId, stream: usize) -> Self {
        if stream == 0 { Self::Media(item) } else { Self::MediaStream { item, stream } }
    }
}

/// One range of source audio an export references.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AudioNeed {
    pub key: EssenceKey,
    /// The media item (for a clip key: the clip's media).
    pub item: ItemId,
    /// Selected container stream; zero for legacy and sequence sources.
    #[serde(default)]
    pub audio_stream: usize,
    /// The media file, when the item is file media.
    pub path: Option<String>,
    /// Media time range to supply (handles included). Rendered clips use virtual
    /// timeline-rate media coordinates; decoded ranges are clamped to the source.
    pub start: Tick,
    pub end: Tick,
    /// Channels of the media's audio.
    pub channels: u32,
    pub sample_rate: u32,
}

/// Where the samples of an [`AudioEssence`] are.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum EssenceData {
    /// Interleaved little-endian signed PCM (`bits` per sample), embedded in the document.
    Embedded(Vec<u8>),
    /// A media file the caller wrote (consolidated / trimmed / rendered), referenced by path.
    File { path: String },
}

/// Audio essence supplied by the caller for one [`EssenceKey`] (and channel, when broken out to
/// mono).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AudioEssence {
    pub key: EssenceKey,
    /// `Some(c)`: this essence carries only source channel `c` (0-based), as one mono stream.
    pub channel: Option<u32>,
    /// Media time of the first sample (trimmed essence starts after the media's 0).
    pub start: Tick,
    /// Sample frames.
    pub frames: u64,
    pub sample_rate: u32,
    pub bits: u16,
    pub channels: u32,
    pub data: EssenceData,
    /// Clip effects (volume, gain, audio effects) are rendered into these samples: the document
    /// carries no gain for clips that use it.
    pub effects_rendered: bool,
    /// Speed and reverse playback have been rendered into timeline-rate PCM.
    #[serde(default)]
    pub timing_rendered: bool,
    /// Linear gain restoring headroom removed before integer PCM quantization.
    #[serde(default = "unity")]
    pub render_gain: f64,
}

fn unity() -> f64 {
    1.0
}

/// Whether audio needs rendering to preserve the clip's playback timing.
pub fn retimed(c: &filmcraft_project::TrackItem) -> bool {
    c.speed != 1.0 || c.reverse
}

/// A rendered video mixdown that replaces every video track with one clip.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MixdownVideo {
    pub path: String,
    /// Timeline range the file covers (its first frame is at `start`).
    pub start: Tick,
    pub duration: Tick,
    pub width: u32,
    pub height: u32,
}

/// Options shared by the AAF and OMF exporters.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MediaOptions {
    /// Split multichannel audio into mono tracks (one slot per channel; essence per channel).
    pub breakout_to_mono: bool,
    /// Leave the video tracks out (OMF is audio-only).
    pub audio_only: bool,
    /// Essence the caller prepared (see the module docs). Media without essence is referenced at
    /// its original path.
    pub essence: Vec<AudioEssence>,
    /// Replace the video tracks with this rendered file.
    pub mixdown_video: Option<MixdownVideo>,
    /// Replace the top-level sound tracks with the caller's complete mixer output.
    /// This preserves processing that cannot be represented portably as editable clips.
    #[serde(default)]
    pub mixdown_audio: Option<AudioEssence>,
}

impl MediaOptions {
    /// Essence for `key` (and `channel` when broken out).
    pub fn essence_for(&self, key: EssenceKey, channel: Option<u32>) -> Option<&AudioEssence> {
        self.essence.iter().find(|e| e.key == key && (e.channel == channel || e.channel.is_none()))
    }
}

/// What [`audio_needs`] lists for a nested sequence on an audio track.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum NestNeeds {
    /// Nothing.
    #[default]
    Skip,
    /// The nested sequence's own sound over the range the clip uses, as one [`EssenceKey::Clip`]
    /// range per clip (its `item` is the nested sequence) for the caller to mix. OMF: Premiere Pro
    /// renders the sound of a nested sequence into the document.
    Render,
    /// What the clips inside the nested sequence need, like those of the sequence itself. AAF: a
    /// nested sequence is written as a composition of its own.
    Inside,
}

/// How [`audio_needs`] groups ranges.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NeedOptions {
    /// Extra media before and after every used range.
    pub handles: Tick,
    /// One range per clip ([`EssenceKey::Clip`]; for rendering clip effects) instead of one range
    /// per media item and container stream covering all its uses. Stream zero keeps
    /// the legacy [`EssenceKey::Media`] identity.
    pub per_clip: bool,
    /// Prepare retimed clips separately in virtual, timeline-rate media coordinates.
    #[serde(default)]
    pub bake_retiming: bool,
    /// Whole media instead of the used range (embedding / copying without trimming).
    pub whole_media: bool,
    /// Nested sequences on the audio tracks.
    #[serde(default)]
    pub nests: NestNeeds,
}

/// Nested sequences deeper than this are not followed.
const MAX_NESTING: usize = 16;

/// The audio ranges the audio tracks of `sequence` reference, in a stable order.
pub fn audio_needs(project: &Project, sequence: ItemId, opts: &NeedOptions) -> Vec<AudioNeed> {
    let mut by_item: BTreeMap<EssenceKey, AudioNeed> = BTreeMap::new();
    collect_needs(project, sequence, opts, &mut Vec::new(), &mut BTreeSet::new(), &mut by_item);
    by_item.into_values().collect()
}

fn collect_needs(
    project: &Project,
    sequence: ItemId,
    opts: &NeedOptions,
    open: &mut Vec<ItemId>,
    done: &mut BTreeSet<ItemId>,
    by_item: &mut BTreeMap<EssenceKey, AudioNeed>,
) {
    let Some(seq) = project.sequence(sequence) else { return };
    // a nest's needs do not depend on the clip that uses it: each sequence is listed once, so a
    // crafted project with many clips of the same nest on every level cannot multiply the work
    // (a sequence cannot be inside itself, but a damaged project may claim so)
    if open.contains(&sequence) || done.contains(&sequence) || open.len() >= MAX_NESTING {
        return;
    }
    open.push(sequence);
    for t in seq.tracks(TrackKind::Audio) {
        for c in &t.items {
            let item = crate::common::base_item(project, c.item);
            let render_timing = opts.bake_retiming && retimed(c);
            // Rendered audio advances in timeline time, regardless of the original speed.
            // Reference-only callers still need the full declared 100% playback span.
            let (lo, hi) = (c.source_in, c.source_in + c.duration.max(Tick::ZERO));
            // Virtual rendered coordinates can be negative: a reverse clip with source_in
            // zero still has real leading handles. Only decoded source ranges clamp at zero.
            let (mut start, mut end) = (if render_timing { lo - opts.handles } else { (lo - opts.handles).max(Tick::ZERO) }, hi + opts.handles);
            if opts.per_clip || render_timing {
                // A two-sided transition plays beyond the visible cut even with zero
                // requested handles. Its rendered source must cover that whole overlap.
                for tr in &t.transitions {
                    if tr.from == Some(c.id) || tr.to == Some(c.id) {
                        start = start.min(c.source_in + (tr.start - c.start));
                        end = end.max(c.source_in + (tr.end() - c.start));
                    }
                }
            }
            match project.item(item).map(|i| &i.kind) {
                Some(ItemKind::Media(m)) => {
                    let Some(a) = m.info.audio_streams.get(c.audio_stream) else { continue };
                    let path = match &m.media {
                        MediaRef::File { path } => Some(path.clone()),
                        MediaRef::Generator(_) => None,
                    };
                    let dur = m.info.duration;
                    if dur > Tick::ZERO && !render_timing {
                        end = end.min(dur);
                    }
                    if opts.whole_media && !render_timing {
                        start = Tick::ZERO;
                        end = dur.max(end);
                    }
                    let key = if opts.per_clip || render_timing { EssenceKey::Clip(c.id) } else { EssenceKey::media(item, c.audio_stream) };
                    let e = by_item.entry(key).or_insert_with(|| AudioNeed {
                        key,
                        item,
                        audio_stream: c.audio_stream,
                        path,
                        start,
                        end,
                        channels: a.channels,
                        sample_rate: a.sample_rate,
                    });
                    e.start = e.start.min(start);
                    e.end = e.end.max(end);
                }
                Some(ItemKind::Sequence(nested)) => match if render_timing { NestNeeds::Render } else { opts.nests } {
                    NestNeeds::Skip => {}
                    NestNeeds::Inside => collect_needs(project, item, opts, open, done, by_item),
                    NestNeeds::Render => {
                        // a clip longer than its sequence plays silence past the end: nothing to supply
                        if !render_timing {
                            end = end.min(nested.duration());
                        }
                        if end <= start {
                            continue;
                        }
                        let channels = match t.channels {
                            filmcraft_project::AudioChannels::Mono => 1,
                            filmcraft_project::AudioChannels::Surround51 => 6,
                            _ => 2,
                        };
                        let key = EssenceKey::Clip(c.id);
                        by_item.insert(
                            key,
                            AudioNeed { key, item, audio_stream: 0, path: None, start, end, channels, sample_rate: nested.settings.sample_rate.max(1) },
                        );
                    }
                },
                _ => {}
            }
        }
    }
    open.pop();
    done.insert(sequence);
}
