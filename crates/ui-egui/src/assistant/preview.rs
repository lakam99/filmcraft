//! Bounded, opaque AI previews. Decoder scale hints do not guarantee output dimensions.
use crate::frames::Rgba;
use std::io::Cursor;

const MAX_SIDE: u32 = 1280;
// Leave room for row filters and PNG headers under 4 MiB, even for noisy square frames.
const MAX_PIXELS: u32 = 1_000_000;
const MAX_PNG_BYTES: usize = 4 * 1024 * 1024;
const MAX_INPUT_BYTES: usize = 64 * 1024 * 1024;

pub(crate) struct Encoded {
    pub width: u32,
    pub height: u32,
    pub url: String,
}

fn png(frame: &Rgba) -> Result<(u32, u32, Vec<u8>), String> {
    if frame.w == 0 || frame.h == 0 || frame.w > 32_768 || frame.h > 32_768 {
        return Err("The preview has invalid dimensions".into());
    }
    let bytes =
        frame.w.checked_mul(frame.h).and_then(|n| n.checked_mul(4)).filter(|&n| n <= MAX_INPUT_BYTES).ok_or("The preview is too large to prepare safely")?;
    if frame.px.len() != bytes {
        return Err("Invalid preview pixel buffer".into());
    }
    let scale = (f64::from(MAX_SIDE) / frame.w.max(frame.h) as f64).min((f64::from(MAX_PIXELS) / (frame.w as f64 * frame.h as f64)).sqrt()).min(1.0);
    let width = ((frame.w as f64 * scale).floor() as u32).max(1);
    let height = ((frame.h as f64 * scale).floor() as u32).max(1);
    let mut pixels = frame.px.clone();
    // Cached RGB is already over black. Unpremultiplying or retaining its alpha
    // would change the appearance when an AI image viewer composites it again.
    for [_, _, _, alpha] in pixels.as_chunks_mut::<4>().0 {
        *alpha = 255;
    }
    let mut image = image::RgbaImage::from_raw(frame.w as u32, frame.h as u32, pixels).ok_or("Invalid preview pixel buffer")?;
    if image.width() != width || image.height() != height {
        image = image::imageops::resize(&image, width, height, image::imageops::FilterType::Triangle);
    }
    let mut output = Cursor::new(Vec::new());
    image.write_to(&mut output, image::ImageFormat::Png).map_err(|_| "Cannot encode the preview frame")?;
    let bytes = output.into_inner();
    if bytes.len() > MAX_PNG_BYTES {
        return Err("Preview PNG exceeds the AI image size limit".into());
    }
    Ok((width, height, bytes))
}

pub(super) fn encode(frame: &Rgba) -> Result<Encoded, String> {
    let (width, height, bytes) = png(frame)?;
    Ok(Encoded { width, height, url: format!("data:image/png;base64,{}", crate::control::base64(&bytes)) })
}

#[cfg(not(target_arch = "wasm32"))]
pub(super) use native::Pending;

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use super::{Encoded, encode};
    use crate::frames::Rgba;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, mpsc};

    #[derive(Default)]
    struct Slots(AtomicUsize);
    impl Slots {
        fn acquire(&self) -> Option<Permit<'_>> {
            // Program and Source may be requested together. A burst of tool calls
            // must not create an unbounded number of PNG workers or pixel copies.
            match self.0.try_update(Ordering::AcqRel, Ordering::Acquire, |used| if used < 2 { Some(used + 1) } else { None }) {
                Ok(_) => Some(Permit(self)),
                Err(_) => None,
            }
        }
    }
    struct Permit<'a>(&'a Slots);
    impl Drop for Permit<'_> {
        fn drop(&mut self) {
            self.0.0.fetch_sub(1, Ordering::Release);
        }
    }
    static SLOTS: Slots = Slots(AtomicUsize::new(0));

    pub(crate) struct Pending(mpsc::Receiver<Result<Encoded, String>>);
    impl Pending {
        pub(crate) fn start(frame: Arc<Rgba>) -> Result<Option<Self>, String> {
            let Some(permit) = SLOTS.acquire() else { return Ok(None) };
            let (send, receive) = mpsc::sync_channel(1);
            std::thread::Builder::new()
                .name("ai-preview-png".into())
                .spawn(move || {
                    let _permit = permit;
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| encode(&frame)))
                        .unwrap_or_else(|_| Err("AI preview encoding failed. Request the frame again.".into()));
                    let _ = send.send(result);
                })
                .map_err(|e| format!("Cannot start AI preview encoding: {e}"))?;
            Ok(Some(Self(receive)))
        }

        pub(crate) fn poll(&self) -> Result<Option<Encoded>, String> {
            match self.0.try_recv() {
                Ok(result) => result.map(Some),
                Err(mpsc::TryRecvError::Empty) => Ok(None),
                Err(mpsc::TryRecvError::Disconnected) => Err("AI preview encoding stopped before completing".into()),
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn encoding_slots_are_bounded_and_return_after_completion_or_panic() {
            let slots = Slots::default();
            let first = slots.acquire().unwrap();
            let second = slots.acquire().unwrap();
            assert!(slots.acquire().is_none());
            drop(first);
            let replacement = slots.acquire().unwrap();
            assert!(slots.acquire().is_none());
            drop((second, replacement));
            let result = std::panic::catch_unwind(|| {
                let _permit = slots.acquire().unwrap();
                panic!("synthetic encoding failure");
            });
            assert!(result.is_err());
            assert_eq!(slots.0.load(Ordering::Acquire), 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opaque_previews_preserve_cached_rgb_and_leave_the_cached_frame_unchanged() {
        let frame = Rgba { w: 2, h: 1, px: vec![64, 32, 16, 128, 12, 34, 56, 64] };
        let (width, height, bytes) = png(&frame).unwrap();
        let decoded = image::load_from_memory(&bytes).unwrap().into_rgba8();
        assert_eq!((width, height), (2, 1));
        assert_eq!(decoded.into_raw(), [64, 32, 16, 255, 12, 34, 56, 255]);
        assert_eq!(frame.px[3], 128);
        assert_eq!(frame.px[7], 64);
    }

    #[test]
    fn noisy_square_previews_fit_the_pixel_and_png_budgets() {
        let mut pixels = vec![0u8; 1600 * 1600 * 4];
        let mut random = 1u32;
        for pixel in pixels.as_chunks_mut::<4>().0 {
            for channel in &mut pixel[..3] {
                random = random.wrapping_mul(1664525).wrapping_add(1013904223);
                *channel = (random >> 24) as u8;
            }
            pixel[3] = 255;
        }
        let (width, height, bytes) = png(&Rgba { w: 1600, h: 1600, px: pixels }).unwrap();
        assert_eq!((width, height), (1000, 1000));
        assert!(bytes.len() <= MAX_PNG_BYTES);
        assert_eq!(image::load_from_memory(&bytes).unwrap().width(), 1000);
    }

    #[test]
    fn invalid_or_oversized_buffers_are_rejected_before_copying_pixels() {
        for (w, h, pixels) in [(0, 4, vec![]), (4, 0, vec![]), (2, 2, vec![0; 4]), (usize::MAX, 2, vec![]), (32_768, 32_768, vec![])] {
            let result = std::panic::catch_unwind(|| encode(&Rgba { w, h, px: pixels }));
            assert!(result.is_ok());
            assert!(result.unwrap().is_err());
        }
    }
}
