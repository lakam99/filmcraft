//! Source acquisition and decoding wall time inside one native frame request.
use filmcraft_media::{MediaSource, SharedSource};
use filmcraft_project::ItemId;
use filmcraft_render::SourceProvider;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;
use web_time::Instant;

#[derive(Clone, Default)]
pub(super) struct Clock(Arc<AtomicU64>);
impl Clock {
    pub(super) fn measure<T>(&self, work: impl FnOnce() -> T) -> T {
        let start = Instant::now();
        let result = work();
        let nanos = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
        let _ = self.0.try_update(Ordering::Relaxed, Ordering::Relaxed, |old| Some(old.saturating_add(nanos)));
        result
    }
    pub(super) fn elapsed(&self) -> Duration {
        Duration::from_nanos(self.0.load(Ordering::Relaxed))
    }
}
pub(super) struct Provider {
    pub inner: crate::media_pool::PoolProvider,
    pub clock: Clock,
}
impl SourceProvider for Provider {
    fn source(&self, item: ItemId) -> Option<SharedSource> {
        // Lazy opening can itself read media or wait for the host broker. It is
        // source work too, rather than evidence that grading is expensive.
        let source = self.clock.measure(|| self.inner.source(item))?;
        Some(Arc::new(Source { inner: source, clock: self.clock.clone() }))
    }
}
struct Source {
    inner: SharedSource,
    clock: Clock,
}
impl MediaSource for Source {
    fn info(&self) -> &filmcraft_media::MediaInfo {
        self.inner.info()
    }
    fn video_frame(&self, request: filmcraft_media::FrameRequest) -> filmcraft_media::Result<Arc<filmcraft_frame::VideoFrame>> {
        self.clock.measure(|| self.inner.video_frame(request))
    }
    fn video_frame_unturned(&self, request: filmcraft_media::FrameRequest) -> filmcraft_media::Result<(Arc<filmcraft_frame::VideoFrame>, u8)> {
        self.clock.measure(|| self.inner.video_frame_unturned(request))
    }
    fn audio(&self, start: i64, frames: usize, rate: u32) -> filmcraft_media::Result<filmcraft_frame::AudioBuffer> {
        self.inner.audio(start, frames, rate)
    }
    fn audio_stream(&self, stream: usize, start: i64, frames: usize, rate: u32) -> filmcraft_media::Result<filmcraft_frame::AudioBuffer> {
        self.inner.audio_stream(stream, start, frames, rate)
    }
}
