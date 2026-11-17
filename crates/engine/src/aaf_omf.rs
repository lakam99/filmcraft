//! File ▸ Export ▸ AAF… / OMF… and File ▸ Import of AAF / OMF documents.
//!
//! The interchange crate writes the documents; this module prepares their audio essence the way
//! Premiere's AAF and OMF export dialogs offer it: embedded in the document or written as separate
//! WAV / AIFF files next to it, at a chosen sample rate and bit depth, trimmed to the used ranges
//! plus handles (or whole), with clip effects rendered in, broken out to mono; and for AAF an
//! optional video mixdown rendered to one file.
//!
//! `file.exportAaf {path, sequence?, mixdownVideo?, mixdownFormat?: "mp4"|"mov"|"mxf", breakoutToMono?, audio?: "embedded"|"separate"|
//! "linked", audioFormat?: "wav"|"aiff"|"mxf" (OP-Atom, AAF only), sampleRate?, bitDepth?, trimAudio?, handles? (frames),
//! renderAudioEffects?, smallSectors?}`
//! AAF defaults to `preserveMix: true`: render clip and mixer processing, retaining edits
//! for a separable stereo mixer; print the complete mix for buses, inserts or transitions.
//!
//! `file.exportOmf {path, sequence?, audio?: "embedded"|"separate", audioFormat?, sampleRate?,
//! bitDepth?, trimAudio? (default on), handles?, renderAudioEffects?, breakoutToMono?}`
//!
//! `file.importAaf {path, bin?}` (also `file.import` of `.aaf` / `.omf` files).

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use filmcraft_interchange::essence::{AudioEssence, AudioNeed, EssenceData, EssenceKey, MediaOptions, MixdownVideo, NeedOptions, NestNeeds, audio_needs};
use filmcraft_project::{ClipId, ItemId, Project, Sequence, Track, TrackItem, TrackKind};
use filmcraft_time::{Tick, TimeRange};

use crate::commands::{bad, bool_p, str_p, u64_p};
use crate::{EngineError, Result, Session};

fn check_cancel(s: &Session) -> Result<()> {
    if s.worker_progress.as_ref().is_some_and(|p| p.cancel.load(std::sync::atomic::Ordering::Acquire)) {
        return Err(EngineError::Other("cancelled".into()));
    }
    Ok(())
}

/// Where exported audio goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioMode {
    /// Inside the document.
    Embedded,
    /// Separate WAV / AIFF files next to the document.
    Separate,
    /// Original media files (AAF only), with rendered sidecars for retimed clips.
    Linked,
}

/// The audio settings of the AAF / OMF export dialogs.
#[derive(Clone, Debug)]
pub struct AudioPlan {
    pub mode: AudioMode,
    pub aiff: bool,
    /// Separate files as OP-Atom MXF (AAF only; Avid-style consolidated media).
    pub mxf: bool,
    pub sample_rate: u32,
    pub bits: u16,
    pub trim: bool,
    pub handles: Tick,
    pub render_effects: bool,
    /// Preserve the audible mixer output rather than only interchange clip metadata.
    pub preserve_mix: bool,
    pub breakout: bool,
    /// Nested sequences on the audio tracks. OMF: their sound is rendered into the export, as
    /// Premiere Pro does. AAF: they are written as compositions, so the media of the clips inside
    /// them is prepared like the rest.
    pub nests: NestNeeds,
}

fn plan_from(s: &Session, p: &Value, seq: ItemId, cmd: &str, omf: bool) -> Result<AudioPlan> {
    let q = s.project.sequence(seq).ok_or(EngineError::NoSequence)?;
    let mode = match str_p(p, "audio").unwrap_or("embedded") {
        "embedded" | "embed" | "encapsulate" => AudioMode::Embedded,
        "separate" | "separateAudio" => AudioMode::Separate,
        "linked" | "link" if !omf => AudioMode::Linked,
        other => return Err(bad(cmd, format!("unknown audio mode {other:?} (embedded, separate{})", if omf { "" } else { " or linked" }))),
    };
    let (aiff, mxf) = match str_p(p, "audioFormat").unwrap_or("wav") {
        "wav" | "bwf" | "broadcastWave" => (false, false),
        "aiff" | "aif" => (true, false),
        "mxf" | "opatom" if !omf => (false, true),
        other => return Err(bad(cmd, format!("unknown audio format {other:?} (wav, aiff{})", if omf { "" } else { " or mxf" }))),
    };
    let sample_rate = u64_p(p, "sampleRate").map(|r| r as u32).unwrap_or(q.settings.sample_rate.max(1));
    if !(8_000..=192_000).contains(&sample_rate) {
        return Err(bad(cmd, "sampleRate must be 8000–192000"));
    }
    let preserve_mix = !omf && bool_p(p, "preserveMix").unwrap_or(true);
    // A number, or the string the params list it as ("16|24"); anything else is refused rather
    // than read as 16 (#4019).
    let bits: u16 = match p.get("bitDepth").filter(|v| !v.is_null()) {
        None => {
            if preserve_mix {
                24
            } else {
                16
            }
        }
        Some(v) => match v
            .as_u64()
            .or_else(|| v.as_f64().filter(|f| f.fract() == 0.0 && *f >= 0.0).map(|f| f as u64))
            .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
        {
            Some(16) => 16,
            Some(24) => 24,
            _ => return Err(bad(cmd, format!("bitDepth must be 16 or 24, not {v}"))),
        },
    };
    let frames = u64_p(p, "handles").unwrap_or(if omf { 30 } else { 0 }) as i64;
    let breakout = bool_p(p, "breakoutToMono").unwrap_or(false);
    if preserve_mix && breakout {
        return Err(bad(cmd, "preserveMix requires interleaved audio to avoid the receiving editor's mono pan law; disable breakoutToMono or preserveMix"));
    }
    Ok(AudioPlan {
        mode,
        aiff,
        mxf,
        sample_rate,
        bits,
        trim: bool_p(p, "trimAudio").unwrap_or(omf),
        handles: q.settings.frame_rate.tick_of(frames),
        render_effects: bool_p(p, "renderAudioEffects").unwrap_or(false),
        preserve_mix,
        breakout,
        nests: if omf { NestNeeds::Render } else { NestNeeds::Inside },
    })
}

fn quantize(planar: &[&Vec<f32>], bits: u16) -> Vec<u8> {
    let n = planar.first().map_or(0, |c| c.len());
    let bps = if bits >= 24 { 3 } else { 2 };
    let mut out = Vec::with_capacity(n * planar.len() * bps);
    for i in 0..n {
        for c in planar {
            let x = c[i].clamp(-1.0, 1.0);
            if bps == 3 {
                out.extend_from_slice(&((x * 8_388_607.0).round() as i32).to_le_bytes()[..3]);
            } else {
                out.extend_from_slice(&((x * 32_767.0).round() as i16).to_le_bytes());
            }
        }
    }
    out
}

const MAX_EXPORT_AUDIO_CHANNELS: usize = 64;
const MAX_EXPORT_AUDIO_SAMPLES: usize = 64 * 1024 * 1024;

/// Playback can fall back to silence after a decode failure. An interchange render must
/// instead fail, so a receiving editor never mistakes missing audio for a successful bake.
struct RenderAudioSource {
    inner: filmcraft_media::SharedSource,
    error: Arc<Mutex<Option<String>>>,
}

impl filmcraft_media::MediaSource for RenderAudioSource {
    fn info(&self) -> &filmcraft_media::MediaInfo {
        self.inner.info()
    }
    fn video_frame(&self, req: filmcraft_media::FrameRequest) -> filmcraft_media::Result<Arc<filmcraft_frame::VideoFrame>> {
        self.inner.video_frame(req)
    }
    fn audio(&self, start: i64, frames: usize, rate: u32) -> filmcraft_media::Result<filmcraft_frame::AudioBuffer> {
        self.audio_stream(0, start, frames, rate)
    }
    fn audio_stream(&self, stream: usize, start: i64, frames: usize, rate: u32) -> filmcraft_media::Result<filmcraft_frame::AudioBuffer> {
        let channels = self.info().audio_streams.get(stream).map_or(1, |a| a.channels.max(1) as usize);
        let result = if frames.saturating_mul(channels) > MAX_EXPORT_AUDIO_SAMPLES {
            Err(filmcraft_media::MediaError::Decode("audio render source request exceeds the sample budget".into()))
        } else {
            self.inner.audio_stream(stream, start, frames, rate)
        };
        if let Err(e) = &result {
            self.error.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(|| e.to_string());
        }
        result
    }
}

/// Decode a media range at `sample_rate` (planar).
fn decode(s: &Session, need: &AudioNeed, sample_rate: u32) -> Result<Vec<Vec<f32>>> {
    let src = s
        .media
        .full_res_source(&s.project, need.item, &*s.services)
        .ok_or_else(|| EngineError::Other(format!("media {} is offline", need.path.as_deref().unwrap_or("?"))))?;
    let start = need.start.to_units_floor(sample_rate as i64);
    let frames = (need.end.to_units_floor(sample_rate as i64) - start).max(0) as usize;
    let ch = (need.channels.max(1) as usize).min(MAX_EXPORT_AUDIO_CHANNELS);
    let total_samples = ch.checked_mul(frames).ok_or_else(|| EngineError::Other("audio export buffer size overflow".into()))?;
    if total_samples > MAX_EXPORT_AUDIO_SAMPLES {
        return Err(EngineError::Other(format!("audio export buffer size {total_samples} samples exceeds maximum allowed ({MAX_EXPORT_AUDIO_SAMPLES})")));
    }
    let mut out: Vec<Vec<f32>> = vec![Vec::with_capacity(frames); ch];
    let chunk = sample_rate as usize * 10;
    let mut done = 0usize;
    while done < frames {
        check_cancel(s)?;
        let n = chunk.min(frames - done);
        let b = src.audio_stream(need.audio_stream, start + done as i64, n, sample_rate).map_err(|e| EngineError::Other(e.to_string()))?;
        for (c, o) in out.iter_mut().enumerate() {
            match b.channels.get(c).or(b.channels.first()) {
                Some(x) => o.extend(x.iter().copied().chain(std::iter::repeat(0.0)).take(n)),
                None => o.extend(std::iter::repeat_n(0.0, n)),
            }
        }
        done += n;
    }
    Ok(out)
}

/// Render in the original timeline coordinate system: gain automation, DSP blocks and
/// pitch-preserving grains must have the same phase and clip boundaries as playback.
fn render_clip(s: &Session, seq: ItemId, need: &AudioNeed, sample_rate: u32, preserve_mix: bool) -> Result<Vec<Vec<f32>>> {
    let EssenceKey::Clip(clip_id) = need.key else { return decode(s, need, sample_rate) };
    let (sequence, track, c) = audio_clip(&s.project, seq, clip_id).ok_or_else(|| EngineError::Other("clip not found".into()))?;
    if c.frame_hold.is_some() {
        return Err(EngineError::Other("frame holds are supported on video clips only".into()));
    }
    if c.duration <= Tick::ZERO {
        return Err(EngineError::Other("audio render needs a positive clip duration".into()));
    }
    let sr = i64::from(sample_rate);
    let head = (c.source_in - need.start).to_units_floor(sr).max(0);
    let tail = (need.end - c.source_in - c.duration).to_units_floor(sr).max(0);
    let body_start = c.start.to_units_floor(sr);
    let body_end = c.end().to_units_floor(sr);
    let first = body_start.checked_sub(head).ok_or_else(|| EngineError::Other("audio render start overflow".into()))?;
    let end = body_end.checked_add(tail).ok_or_else(|| EngineError::Other("audio render end overflow".into()))?;
    // AAF stores rounded sample units; a sub-sample edit can declare one more sample
    // than playback's floor(end)-floor(start). Supply that sample rather than a short file.
    let requested = ((i128::from((need.end - need.start).0) * i128::from(sample_rate) + i128::from(filmcraft_time::TICKS_PER_SECOND / 2))
        / i128::from(filmcraft_time::TICKS_PER_SECOND))
    .max(0);
    let frames =
        usize::try_from(i128::from(end.saturating_sub(first)).max(requested)).map_err(|_| EngineError::Other("invalid audio render duration".into()))?;
    let channels = if track.channels == filmcraft_project::AudioChannels::Surround51 { 6 } else { 2 };
    if !c.speed.is_finite()
        || c.speed.abs() < 1e-6
        || frames.saturating_mul(channels) > MAX_EXPORT_AUDIO_SAMPLES
        || frames as f64 * c.speed.abs().max(1.0) * f64::from(need.channels.max(1)) > MAX_EXPORT_AUDIO_SAMPLES as f64
    {
        return Err(EngineError::Other(
            "audio render exceeds the supported sample budget or has an invalid speed; split the clip into shorter sections".into(),
        ));
    }
    if s.project.sequence(need.item).is_none() && s.media.full_res_source(&s.project, need.item, &*s.services).is_none() {
        return Err(EngineError::Other(format!("media {} is offline", need.path.as_deref().unwrap_or("?"))));
    }
    let provider = s.media.full_res_provider(s.project.clone(), s.services.clone()).recording();
    let error: Arc<Mutex<Option<String>>> = Arc::default();
    let checked = |item| {
        filmcraft_render::SourceProvider::source(&provider, item)
            .map(|inner| Arc::new(RenderAudioSource { inner, error: error.clone() }) as filmcraft_media::SharedSource)
    };
    // Keep source requests bounded too, including faster-than-realtime playback and
    // nested mixes. Absolute resampling/DSP grids make these chunks sample-equivalent.
    let chunk_frames = (16_384.0 / c.speed.abs().max(1.0)).floor().clamp(1.0, 4096.0) as usize;
    let mut isolated = track.clone();
    isolated.transitions.clear(); // the interchange composition carries the transitions
    let mut mixer = sequence.clone();
    mixer.settings.sample_rate = sample_rate;
    for t in &mut mixer.audio_tracks {
        t.items.clear();
        t.transitions.clear();
    }
    let mixer_track = mixer.audio_tracks.iter().position(|t| t.id == track.id).ok_or_else(|| EngineError::Other("audio track not found".into()))?;
    let extended = filmcraft_render::audio::with_handles(c, Tick::from_units(first, sr), Tick::from_units(end, sr));
    let mut out = vec![Vec::with_capacity(frames); channels];
    // Extending a pitch-preserving clip changes its edge grains. Keep the original clip for
    // the visible body, and extend only the editable handles, so consolidation cannot alter it.
    for (start, stop, item) in [(first, body_start, &extended), (body_start, body_end, c), (body_end, end, &extended)] {
        if stop <= start {
            continue;
        }
        isolated.items = vec![item.clone()];
        if let Some(t) = mixer.audio_tracks.get_mut(mixer_track) {
            t.items = isolated.items.clone();
        }
        for at in (start..stop).step_by(chunk_frames) {
            check_cancel(s)?;
            let n = (stop - at).min(chunk_frames as i64) as usize;
            let b = if preserve_mix {
                filmcraft_render::mixer::mix_graph(&s.project, &mixer, at, n, &checked, None)
            } else {
                filmcraft_render::audio::track_input(&s.project, &isolated, at, n, sample_rate, &checked)
            };
            for (dst, src) in out.iter_mut().zip(b.channels) {
                dst.extend(src);
            }
        }
    }
    // Playback recomputes each transition using that transition's own extended clip.
    // Preserve its edge grains too; the receiving AAF mixer applies the fade separately.
    for tr in &track.transitions {
        if tr.from != Some(c.id) && tr.to != Some(c.id) {
            continue;
        }
        let start = tr.start.to_units_floor(sr).max(first);
        let stop = tr.end().to_units_floor(sr).min(end);
        if stop <= start {
            continue;
        }
        isolated.items = vec![filmcraft_render::audio::with_handles(c, tr.start, tr.end())];
        for at in (start..stop).step_by(chunk_frames) {
            check_cancel(s)?;
            let n = (stop - at).min(chunk_frames as i64) as usize;
            let b = filmcraft_render::audio::track_input(&s.project, &isolated, at, n, sample_rate, &checked);
            let offset = (at - first) as usize;
            for (dst, src) in out.iter_mut().zip(b.channels) {
                if let Some(dst) = dst.get_mut(offset..offset + src.len()) {
                    dst.copy_from_slice(&src);
                }
            }
        }
    }
    if !provider.offline_used().is_empty() {
        return Err(EngineError::Other("audio render contains offline media; relink it before exporting".into()));
    }
    if let Some(error) = error.lock().unwrap_or_else(|e| e.into_inner()).take() {
        return Err(EngineError::Other(format!("audio render failed: {error}")));
    }
    for channel in &mut out {
        channel.resize(frames, 0.0);
    }
    Ok(out)
}

/// Power-of-two scaling keeps hot floating-point audio intact in integer PCM. The AAF gain
/// operation restores this scale in the receiving mixer instead of clipping the baked samples.
fn preserve_headroom(planar: &mut [Vec<f32>]) -> Result<f64> {
    let mut peak = 1.0f32;
    for &sample in planar.iter().flatten() {
        if !sample.is_finite() {
            return Err(EngineError::Other("audio render produced a non-finite sample".into()));
        }
        peak = peak.max(sample.abs());
    }
    let gain = 2.0f64.powf(f64::from(peak).log2().ceil());
    if gain > 16384.0 {
        return Err(EngineError::Other("audio render exceeds the representable AAF/OMF gain; reduce the clip level before exporting".into()));
    }
    if gain > 1.0 {
        for sample in planar.iter_mut().flatten() {
            *sample = (f64::from(*sample) / gain) as f32;
        }
    }
    Ok(gain)
}

/// The audio clip `clip` with its track and sequence: in `seq`, or in a sequence nested in it
/// (clip ids are unique in a project).
fn audio_clip(p: &Project, seq: ItemId, clip: ClipId) -> Option<(&Sequence, &Track, &TrackItem)> {
    let first = p.sequence(seq).into_iter();
    let others = p.sequences().filter(|i| i.id != seq).filter_map(|i| p.sequence(i.id));
    for q in first.chain(others) {
        if let Some((t, c)) = q.tracks(TrackKind::Audio).iter().find_map(|t| t.items.iter().find(|c| c.id == clip).map(|c| (t, c))) {
            return Some((q, t, c));
        }
    }
    None
}

fn safe_name(s: &str) -> String {
    let n: String = s.chars().map(|c| if c.is_alphanumeric() || " -_.".contains(c) { c } else { '_' }).collect();
    let n = n.trim().trim_matches('.').to_string();
    if n.is_empty() { "audio".into() } else { n }
}

fn ensure_dir(s: &Session, dir: &str) {
    if !s.services.export_in_memory() && !dir.is_empty() {
        let _ = std::fs::create_dir_all(dir);
    }
}

fn check_linked_streams(s: &Session, seq: ItemId, plan: &AudioPlan) -> Result<()> {
    if plan.mode == AudioMode::Linked && !plan.trim && !plan.render_effects && !plan.preserve_mix {
        let needs = audio_needs(&s.project, seq, &NeedOptions { nests: plan.nests, bake_retiming: true, ..Default::default() });
        if needs.iter().any(|need| need.audio_stream > 0 && !matches!(need.key, EssenceKey::Clip(_))) {
            return Err(EngineError::Other("linked AAF media cannot select container audio streams; choose embedded or separate audio".into()));
        }
    }
    Ok(())
}

/// Prepare the audio essence of an export; returns the essence and the files written.
pub fn prepare_audio(s: &Session, seq: ItemId, plan: &AudioPlan, media_dir: &str) -> Result<(Vec<AudioEssence>, Vec<String>)> {
    check_linked_streams(s, seq, plan)?;
    let mut needs = audio_needs(
        &s.project,
        seq,
        &NeedOptions {
            handles: plan.handles,
            per_clip: plan.render_effects || plan.preserve_mix,
            whole_media: !plan.trim && !plan.render_effects && !plan.preserve_mix,
            nests: if plan.preserve_mix { NestNeeds::Render } else { plan.nests },
            bake_retiming: true,
        },
    );
    let mut essence = Vec::new();
    let mut files = Vec::new();
    let mut used = std::collections::HashSet::new();
    let total = needs.len() as u64;
    for (index, need) in needs.iter_mut().enumerate() {
        check_cancel(s)?;
        if let Some(progress) = &s.worker_progress {
            use std::sync::atomic::Ordering;
            progress.total.store(total.max(1), Ordering::Relaxed);
            progress.done.store(index as u64, Ordering::Relaxed);
            *progress.status.lock().unwrap_or_else(|e| e.into_inner()) = format!("Preparing audio {} of {total}", index + 1);
        }
        let nest = s.project.sequence(need.item).is_some();
        let timing = match need.key {
            EssenceKey::Clip(id) => audio_clip(&s.project, seq, id).is_some_and(|(_, _, c)| filmcraft_interchange::essence::retimed(c)),
            _ => false,
        };
        if plan.mode == AudioMode::Linked && !plan.trim && !plan.render_effects && !plan.preserve_mix && !timing && !nest {
            continue;
        }
        let rendered = plan.render_effects || plan.preserve_mix || nest || timing;
        if rendered
            && let EssenceKey::Clip(id) = need.key
            && let Some((_, _, c)) = audio_clip(&s.project, seq, id)
        {
            let sr = i64::from(plan.sample_rate);
            let head = (c.source_in - need.start).to_units_floor(sr).max(0);
            let tail = (need.end - c.source_in - c.duration).to_units_floor(sr).max(0);
            need.start = c.source_in - Tick::from_units(head, sr);
            need.end = c.source_in + c.duration + Tick::from_units(tail, sr);
        }
        let mut planar = if rendered { render_clip(s, seq, need, plan.sample_rate, plan.preserve_mix)? } else { decode(s, need, plan.sample_rate)? };
        let render_gain = if rendered { preserve_headroom(&mut planar)? } else { 1.0 };
        let frames = planar.first().map_or(0, Vec::len) as u64;
        let groups: Vec<(Option<u32>, Vec<&Vec<f32>>)> =
            if plan.breakout { planar.iter().enumerate().map(|(c, x)| (Some(c as u32), vec![x])).collect() } else { vec![(None, planar.iter().collect())] };
        let base = match need.key {
            EssenceKey::Clip(c) => {
                let name = audio_clip(&s.project, seq, c).map(|(_, _, ti)| ti.name.clone()).unwrap_or_default();
                format!("{} {}", safe_name(&name), c.0)
            }
            EssenceKey::Media(_) | EssenceKey::MediaStream { .. } => safe_name(
                need.path
                    .as_deref()
                    .map(|p| std::path::Path::new(p).file_stem().map(|x| x.to_string_lossy().into_owned()).unwrap_or_default())
                    .as_deref()
                    .unwrap_or("audio"),
            ),
        };
        for (ch, chans) in groups {
            let pcm = quantize(&chans, plan.bits);
            let n = chans.len() as u32;
            let data = match plan.mode {
                AudioMode::Embedded => EssenceData::Embedded(pcm),
                AudioMode::Separate | AudioMode::Linked => {
                    let ext = if plan.mxf {
                        "mxf"
                    } else if plan.aiff {
                        "aif"
                    } else {
                        "wav"
                    };
                    let stem = match ch {
                        Some(c) => format!("{base}_A{}", c + 1),
                        None => base.clone(),
                    };
                    let mut name = format!("{stem}.{ext}");
                    let mut k = 1;
                    while !used.insert(name.to_ascii_lowercase()) {
                        k += 1;
                        name = format!("{stem} ({k}).{ext}");
                    }
                    let path = if media_dir.is_empty() { name.clone() } else { format!("{media_dir}/{name}") };
                    let bytes = if plan.mxf {
                        let bps = plan.bits.div_ceil(8) as usize;
                        let samples: Vec<i32> = pcm
                            .chunks_exact(bps)
                            .map(|b| if bps == 3 { i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8 } else { i16::from_le_bytes([b[0], b[1]]) as i32 })
                            .collect();
                        let opts = filmcraft_mxf::OpAtomPcm {
                            sample_rate: plan.sample_rate,
                            bits: plan.bits as u32,
                            channels: n,
                            edit_rate: None,
                            ids: filmcraft_mxf::PackageIds::from_seed(&path, &name),
                            timecode: None,
                        };
                        filmcraft_mxf::write_opatom_pcm(&opts, &samples).map_err(|e| EngineError::Other(format!("{path}: {e}")))?
                    } else if plan.aiff {
                        filmcraft_interchange::wav::aiff_file(&pcm, n as u16, plan.sample_rate, plan.bits)
                    } else {
                        filmcraft_interchange::wav::wav_file(&pcm, n as u16, plan.sample_rate, plan.bits)
                    };
                    s.services.write_file(&path, &bytes).map_err(|e| EngineError::Other(format!("{path}: {e}")))?;
                    files.push(path.clone());
                    EssenceData::File { path }
                }
            };
            essence.push(AudioEssence {
                key: need.key,
                channel: ch,
                start: need.start,
                frames,
                sample_rate: plan.sample_rate,
                bits: plan.bits,
                channels: n,
                data,
                effects_rendered: rendered,
                timing_rendered: timing,
                render_gain,
            });
        }
    }
    Ok((essence, files))
}

fn mixdown_format(p: &Value) -> Result<(filmcraft_export::Format, &'static str)> {
    use filmcraft_export::Format;
    let invalid = || bad("file.exportAaf", "`mixdownFormat` must be mp4 (H.264), mov (DNxHR), or mxf (DNxHR OP-Atom)");
    match p.get("mixdownFormat") {
        None | Some(Value::Null) => Ok((Format::H264, "mp4")),
        Some(Value::String(value)) => match value.to_ascii_lowercase().as_str() {
            "mp4" => Ok((Format::H264, "mp4")),
            "mov" => Ok((Format::DnxHr, "mov")),
            "mxf" => Ok((Format::MxfOpAtom, "mxf")),
            _ => Err(invalid()),
        },
        _ => Err(invalid()),
    }
}

/// Render the video of `seq` to one file for an AAF video mixdown.
fn mixdown(s: &Session, seq: ItemId, path: &str, format: filmcraft_export::Format) -> Result<MixdownVideo> {
    let q = s.project.sequence(seq).ok_or(EngineError::NoSequence)?;
    let dur = q.duration();
    if dur <= Tick::ZERO {
        return Err(EngineError::Other("the sequence is empty".into()));
    }
    let mut settings = filmcraft_export::ExportSettings {
        format,
        path: path.to_string(),
        range: Some(TimeRange::new(Tick::ZERO, dur)),
        include_audio: false,
        part_of_batch: false,
        dnx_profile: "hq".into(),
        // DAW picture references need ordinary 8-bit AVC, not HDR/High 10 or a DNxHR
        // decoder absent from common native video players (including Studio Pro on Windows).
        h264_profile: filmcraft_export::H264Profile::High,
        sdr: format == filmcraft_export::Format::H264,
        hardware_encoding: filmcraft_export::HardwareEncoding::Auto,
        ..Default::default()
    };
    if s.services.export_in_memory() {
        let services = s.services.clone();
        settings.sink = Some(filmcraft_export::OutputSink(Arc::new(move |p: &str, d: Vec<u8>| services.write_file(p, &d))));
    }
    if !s.worker_gpu {
        settings.gpu_rendering = filmcraft_export::GpuRendering::Off;
    }
    let project = s.project.clone();
    let provider = s.media.full_res_provider(project.clone(), s.services.clone());
    let progress = s.worker_progress.clone().unwrap_or_default();
    filmcraft_export::export(&project, seq, &settings, &provider, &progress).map_err(|e| EngineError::Other(format!("video mixdown: {e}")))?;
    Ok(MixdownVideo { path: path.to_string(), start: Tick::ZERO, duration: dur, width: q.settings.width, height: q.settings.height })
}

fn report_json(r: &filmcraft_interchange::Report) -> Vec<String> {
    r.entries.iter().map(|e| if e.count > 1 { format!("{} (×{})", e.message, e.count) } else { e.message.clone() }).collect()
}

fn split(path: &str) -> (String, String) {
    let p = std::path::Path::new(path);
    let dir = p.parent().map(|d| d.to_string_lossy().to_string()).unwrap_or_default();
    let stem = p.file_stem().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "Export".into());
    (dir, stem)
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() { name.to_string() } else { format!("{dir}/{name}") }
}

/// Independent clip prints commute with a linear stereo mixer. Inserts, bus routing,
/// overlaps and transitions do not: summing separate processed clips would change the sound.
pub fn needs_mixdown(q: &Sequence) -> bool {
    q.settings.audio_master != filmcraft_project::AudioChannels::Stereo
        || !q.submix_tracks.is_empty()
        || q.master_effects.iter().any(|e| e.enabled)
        || q.audio_tracks.iter().any(|t| {
            t.channels == filmcraft_project::AudioChannels::Surround51
                || t.effects.iter().any(|e| e.enabled)
                || !t.mixer.sends.is_empty()
                || t.mixer.output.is_some()
                || !t.transitions.is_empty()
                || t.items.windows(2).any(|c| c.first().zip(c.get(1)).is_some_and(|(a, b)| a.end() > b.start))
        })
}

/// Printed samples start on playback's floor-based sample grid. AAF otherwise rounds
/// sub-sample event boundaries, moving an already-rendered waveform by one sample.
/// The editor's project stays unchanged, and baked pan is never applied a second time.
fn printed_clip_project(s: &Session, seq: ItemId, sample_rate: u32) -> Result<Project> {
    let mut p = (*s.project).clone();
    let q = p.sequence_mut(seq).ok_or(EngineError::NoSequence)?;
    q.settings.sample_rate = sample_rate;
    q.master_volume_db = 0.0;
    q.master_effects.clear();
    q.master_mixer = Default::default();
    for t in &mut q.audio_tracks {
        t.channels = filmcraft_project::AudioChannels::Stereo;
        t.volume_db = 0.0;
        t.pan = 0.0;
        t.muted = false;
        t.solo = false;
        t.mixer = Default::default();
        for c in &mut t.items {
            let end = c.end().to_units_floor(i64::from(sample_rate));
            let start = c.start.to_units_floor(i64::from(sample_rate));
            c.start = Tick::from_units(start, i64::from(sample_rate));
            c.duration = Tick::from_units(end.saturating_sub(start), i64::from(sample_rate));
        }
    }
    Ok(p)
}

/// A bounded complete mixer print. Processing shared by several tracks must run once
/// after their sum, rather than once per stem (nonlinear master effects especially).
fn prepare_mixdown_audio(s: &Session, seq: ItemId, plan: &AudioPlan, media_dir: &str) -> Result<(AudioEssence, Vec<String>)> {
    if plan.mxf {
        return Err(EngineError::Other("a complete audio mix print requires WAV or AIFF; choose one of those formats".into()));
    }
    let mut q = s.project.sequence(seq).ok_or(EngineError::NoSequence)?.clone();
    q.settings.sample_rate = plan.sample_rate;
    let frames =
        usize::try_from(q.duration().to_units_floor(i64::from(plan.sample_rate))).map_err(|_| EngineError::Other("invalid audio mix duration".into()))?;
    let channels = filmcraft_render::mixer::width_of(q.settings.audio_master);
    if frames == 0 || frames.saturating_mul(channels) > MAX_EXPORT_AUDIO_SAMPLES {
        return Err(EngineError::Other("audio mix exceeds the render sample budget or is empty; export a shorter sequence".into()));
    }
    let provider = s.media.full_res_provider(s.project.clone(), s.services.clone()).recording();
    let error: Arc<Mutex<Option<String>>> = Arc::default();
    let checked = |item| {
        filmcraft_render::SourceProvider::source(&provider, item)
            .map(|inner| Arc::new(RenderAudioSource { inner, error: error.clone() }) as filmcraft_media::SharedSource)
    };
    let mut out = vec![Vec::with_capacity(frames); channels];
    if let Some(progress) = &s.worker_progress {
        progress.total.store(frames as u64, std::sync::atomic::Ordering::Relaxed);
        progress.done.store(0, std::sync::atomic::Ordering::Relaxed);
        *progress.status.lock().unwrap_or_else(|e| e.into_inner()) = "Rendering audio mix".into();
    }
    for at in (0..frames).step_by(4096) {
        check_cancel(s)?;
        let n = (frames - at).min(4096);
        let b = filmcraft_render::mixer::mix_graph(&s.project, &q, at as i64, n, &checked, None);
        for (dst, src) in out.iter_mut().zip(b.channels) {
            dst.extend(src);
        }
        if let Some(progress) = &s.worker_progress {
            progress.done.store((at + n) as u64, std::sync::atomic::Ordering::Relaxed);
        }
    }
    if !provider.offline_used().is_empty() {
        return Err(EngineError::Other("audio mix contains offline media; relink it before exporting".into()));
    }
    if let Some(error) = error.lock().unwrap_or_else(|e| e.into_inner()).take() {
        return Err(EngineError::Other(format!("audio mix render failed: {error}")));
    }
    let render_gain = preserve_headroom(&mut out)?;
    let pcm = quantize(&out.iter().collect::<Vec<_>>(), plan.bits);
    let mut files = Vec::new();
    let data = if plan.mode == AudioMode::Embedded {
        EssenceData::Embedded(pcm)
    } else {
        let path = join(media_dir, if plan.aiff { "FilmCraft Mix.aif" } else { "FilmCraft Mix.wav" });
        let bytes = if plan.aiff {
            filmcraft_interchange::wav::aiff_file(&pcm, channels as u16, plan.sample_rate, plan.bits)
        } else {
            filmcraft_interchange::wav::wav_file(&pcm, channels as u16, plan.sample_rate, plan.bits)
        };
        s.services.write_file(&path, &bytes).map_err(|e| EngineError::Other(format!("{path}: {e}")))?;
        files.push(path.clone());
        EssenceData::File { path }
    };
    Ok((
        AudioEssence {
            key: EssenceKey::Media(seq),
            channel: None,
            start: Tick::ZERO,
            frames: frames as u64,
            sample_rate: plan.sample_rate,
            bits: plan.bits,
            channels: channels as u32,
            data,
            effects_rendered: true,
            timing_rendered: true,
            render_gain,
        },
        files,
    ))
}

/// `file.exportAaf`
pub fn export_aaf(s: &mut Session, p: &Value) -> Result<Value> {
    #[cfg(not(target_arch = "wasm32"))]
    if crate::backend::enabled(s) {
        let seq = crate::export_tools::sequence_param(s, p, "file.exportAaf")?;
        plan_from(s, p, seq, "file.exportAaf", false)?;
        if bool_p(p, "mixdownVideo").unwrap_or(false) {
            mixdown_format(p)?;
        }
        return crate::backend::start_interchange(s, false, p);
    }
    export_aaf_at(s, p, None)
}

pub(crate) fn export_aaf_at(s: &mut Session, p: &Value, media_override: Option<&str>) -> Result<Value> {
    let cmd = "file.exportAaf";
    let path = str_p(p, "path").ok_or_else(|| bad(cmd, "need `path`"))?.to_string();
    let seq = crate::export_tools::sequence_param(s, p, cmd)?;
    let plan = plan_from(s, p, seq, cmd, false)?;
    let video_format = bool_p(p, "mixdownVideo").unwrap_or(false).then(|| mixdown_format(p)).transpose()?;
    check_linked_streams(s, seq, &plan)?;
    let (dir, stem) = split(&path);
    let media_dir = media_override.map(str::to_string).unwrap_or_else(|| join(&dir, &format!("{stem} Media")));
    let mut files = Vec::new();
    if plan.mode != AudioMode::Embedded || bool_p(p, "mixdownVideo").unwrap_or(false) {
        ensure_dir(s, &media_dir);
    }
    let full_mix = plan.preserve_mix && needs_mixdown(s.project.sequence(seq).ok_or(EngineError::NoSequence)?);
    let (essence, mixdown_audio, written) = if full_mix {
        let (mix, files) = prepare_mixdown_audio(s, seq, &plan, &media_dir)?;
        (Vec::new(), Some(mix), files)
    } else {
        let (essence, files) = prepare_audio(s, seq, &plan, &media_dir)?;
        (essence, None, files)
    };
    files.extend(written);
    let mixdown_video = if let Some((format, ext)) = video_format {
        let m = mixdown(s, seq, &join(&media_dir, &format!("{} Video Mixdown.{ext}", safe_name(&stem))), format)?;
        files.push(m.path.clone());
        Some(m)
    } else {
        None
    };
    let opts = filmcraft_interchange::aaf::AafOptions {
        name: s.project.item(seq).map(|i| i.name.clone()),
        media: MediaOptions { breakout_to_mono: plan.breakout, audio_only: false, essence, mixdown_video, mixdown_audio },
        small_sectors: bool_p(p, "smallSectors").unwrap_or(false),
    };
    let printed = if plan.preserve_mix && !full_mix { Some(printed_clip_project(s, seq, plan.sample_rate)?) } else { None };
    let project = printed.as_ref().unwrap_or(&s.project);
    let (bytes, mut report) = filmcraft_interchange::aaf::export(project, seq, &opts).map_err(|e| EngineError::Other(e.to_string()))?;
    if printed.is_some() {
        report.info("clip effects, track levels, mute, solo, pan and master level were rendered; editable audio events are preserved");
    } else if !plan.preserve_mix {
        report.warn("mix matching is disabled: track, bus and master mixer processing is not preserved");
    }
    check_cancel(s)?;
    s.services.write_file(&path, &bytes).map_err(|e| EngineError::Other(format!("{path}: {e}")))?;
    Ok(json!({"path": path, "bytes": bytes.len(), "mediaFiles": files, "report": report_json(&report),
        "preserveMix": plan.preserve_mix, "audioLayout": if full_mix { "mixdown" } else { "clips" }}))
}

/// `file.exportOmf`
pub fn export_omf(s: &mut Session, p: &Value) -> Result<Value> {
    #[cfg(not(target_arch = "wasm32"))]
    if crate::backend::enabled(s) {
        let seq = crate::export_tools::sequence_param(s, p, "file.exportOmf")?;
        plan_from(s, p, seq, "file.exportOmf", true)?;
        return crate::backend::start_interchange(s, true, p);
    }
    export_omf_at(s, p, None)
}

pub(crate) fn export_omf_at(s: &mut Session, p: &Value, media_override: Option<&str>) -> Result<Value> {
    let cmd = "file.exportOmf";
    let path = str_p(p, "path").ok_or_else(|| bad(cmd, "need `path`"))?.to_string();
    let seq = crate::export_tools::sequence_param(s, p, cmd)?;
    let plan = plan_from(s, p, seq, cmd, true)?;
    let (dir, stem) = split(&path);
    let media_dir = media_override.map(str::to_string).unwrap_or_else(|| join(&dir, &format!("{stem} Audio Files")));
    if plan.mode == AudioMode::Separate {
        ensure_dir(s, &media_dir);
    }
    let (essence, files) = prepare_audio(s, seq, &plan, &media_dir)?;
    let opts = filmcraft_interchange::omf::OmfOptions {
        name: str_p(p, "title").map(str::to_string).or_else(|| s.project.item(seq).map(|i| i.name.clone())),
        media: MediaOptions { breakout_to_mono: plan.breakout, audio_only: true, essence, mixdown_video: None, mixdown_audio: None },
    };
    let (bytes, report) = filmcraft_interchange::omf::export(&s.project, seq, &opts).map_err(|e| EngineError::Other(e.to_string()))?;
    if bytes.len() as u64 > i32::MAX as u64 {
        return Err(EngineError::Other("the OMF file would exceed 2 GB; use separate audio files or trim the audio".into()));
    }
    s.services.write_file(&path, &bytes).map_err(|e| EngineError::Other(format!("{path}: {e}")))?;
    Ok(json!({"path": path, "bytes": bytes.len(), "mediaFiles": files, "report": report_json(&report)}))
}

/// `file.importAaf` (and OMF): import a document as sequences.
pub fn import_document(s: &mut Session, p: &Value, cmd: &str) -> Result<Value> {
    let path = str_p(p, "path").ok_or_else(|| bad(cmd, "need `path`"))?.to_string();
    let bytes = s.services.read_file(&path).map_err(|e| EngineError::Other(format!("{path}: {e}")))?;
    let format = crate::interchange::detect(&path, &bytes).filter(|f| matches!(f, filmcraft_interchange::Format::Aaf | filmcraft_interchange::Format::Omf));
    let format = format.ok_or_else(|| bad(cmd, format!("{path} is not an AAF or OMF file")))?;
    let bin = u64_p(p, "bin").map(filmcraft_project::BinId);
    crate::interchange::import(s, &path, &bytes, format, bin)
}
