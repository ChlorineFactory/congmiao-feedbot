use super::*;
use feedbot_core::UgoiraFrame;
use std::sync::Mutex;

const MAX_TEST_PHOTO_BYTES: u64 = 100;

struct FakePhotoProcessor {
    original: PhotoInfo,
    encoded_sizes: &'static [usize],
    attempts: Mutex<Vec<(u32, u32, PhotoEncoding)>>,
}

impl FakePhotoProcessor {
    fn new(original: PhotoInfo, encoded_sizes: &'static [usize]) -> Self {
        Self {
            original,
            encoded_sizes,
            attempts: Mutex::new(Vec::new()),
        }
    }

    async fn prepare(&self, dir: &Path, size: u64) -> Result<Option<(PathBuf, u64)>, MediaError> {
        let original = dir.join("original");
        tokio::fs::write(&original, vec![0; size as usize])
            .await
            .unwrap();
        prepare_photo(
            &original,
            size,
            &dir.join("prepared"),
            self,
            MAX_TEST_PHOTO_BYTES,
        )
        .await
    }
}

#[async_trait::async_trait]
impl MediaProcessor for FakePhotoProcessor {
    async fn probe_photo(&self, path: &Path) -> Result<Option<PhotoInfo>, MediaError> {
        if path.ends_with("original") {
            return Ok(Some(self.original));
        }
        let &(width, height, encoding) = self.attempts.lock().unwrap().last().unwrap();
        let format = match encoding {
            PhotoEncoding::Jpeg { .. } => PhotoFormat::Jpeg,
            PhotoEncoding::Png => PhotoFormat::Png,
        };
        Ok(Some(PhotoInfo {
            width,
            height,
            format,
            may_have_alpha: matches!(format, PhotoFormat::Png) && self.original.may_have_alpha,
        }))
    }

    async fn encode_photo(
        &self,
        _input: &Path,
        output: &Path,
        long_edge: u32,
        short_edge: u32,
        encoding: PhotoEncoding,
    ) -> Result<(), MediaError> {
        let size = {
            let mut attempts = self.attempts.lock().unwrap();
            let size = self.encoded_sizes.get(attempts.len()).copied();
            attempts.push((long_edge, short_edge, encoding));
            size
        };
        let Some(size) = size else {
            return Err(MediaError::Ffmpeg("test encoding failed".into()));
        };
        tokio::fs::write(output, vec![0; size]).await.unwrap();
        Ok(())
    }

    async fn probe(&self, _path: &Path) -> Result<MediaInfo, MediaError> {
        unreachable!()
    }

    async fn remux(&self, _inputs: &[PathBuf], _output: &Path) -> Result<(), MediaError> {
        unreachable!()
    }

    async fn thumbnail(&self, _input: &Path, _output: &Path) -> Result<(), MediaError> {
        unreachable!()
    }

    async fn transcode(
        &self,
        _input: &Path,
        _output: &Path,
        _spec: &TranscodeSpec,
    ) -> Result<(), MediaError> {
        unreachable!()
    }

    async fn ugoira_to_mp4(
        &self,
        _zip: &Path,
        _frames: &[UgoiraFrame],
        _work_dir: &Path,
        _output: &Path,
    ) -> Result<(), MediaError> {
        unreachable!()
    }
}

fn jpeg(width: u32, height: u32) -> PhotoInfo {
    PhotoInfo {
        width,
        height,
        format: PhotoFormat::Jpeg,
        may_have_alpha: false,
    }
}

#[tokio::test]
async fn compliant_photo_at_both_limits_is_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let media = FakePhotoProcessor::new(jpeg(6000, 4000), &[]);
    assert_eq!(
        media
            .prepare(dir.path(), MAX_TEST_PHOTO_BYTES)
            .await
            .unwrap(),
        Some((dir.path().join("original"), MAX_TEST_PHOTO_BYTES))
    );
    assert!(media.attempts.lock().unwrap().is_empty());
}

#[tokio::test]
async fn oversized_dimensions_are_fixed_before_encoding_even_if_bytes_fit() {
    let dir = tempfile::tempdir().unwrap();
    let media = FakePhotoProcessor::new(jpeg(16_000, 800), &[98]);
    assert_eq!(
        media.prepare(dir.path(), 90).await.unwrap(),
        Some((dir.path().join("prepared").join("photo.jpg"), 98))
    );
    // Rounding the short edge down here would exceed Telegram's 20:1 limit.
    assert!(matches!(
        media.attempts.lock().unwrap().as_slice(),
        [(9523, 477, PhotoEncoding::Jpeg { quality: 100 })]
    ));
}

#[tokio::test]
async fn oversized_jpeg_lowers_quality_before_reducing_dimensions() {
    let dir = tempfile::tempdir().unwrap();
    let media = FakePhotoProcessor::new(jpeg(3000, 2000), &[140, 110, 101, 100]);
    assert_eq!(
        media.prepare(dir.path(), 150).await.unwrap(),
        Some((dir.path().join("prepared").join("photo.jpg"), 100))
    );
    assert!(matches!(
        media.attempts.lock().unwrap().as_slice(),
        [
            (3000, 2000, PhotoEncoding::Jpeg { quality: 100 }),
            (3000, 2000, PhotoEncoding::Jpeg { quality: 88 }),
            (3000, 2000, PhotoEncoding::Jpeg { quality: 72 }),
            (2250, 1500, PhotoEncoding::Jpeg { quality: 88 }),
        ]
    ));
}

#[tokio::test]
async fn panorama_above_aspect_limit_falls_back_without_encoding() {
    let dir = tempfile::tempdir().unwrap();
    let media = FakePhotoProcessor::new(jpeg(8401, 420), &[]);
    assert_eq!(media.prepare(dir.path(), 90).await.unwrap(), None);
    assert!(media.attempts.lock().unwrap().is_empty());
}

#[tokio::test]
async fn oversized_transparent_png_keeps_png_encoding_when_resized() {
    let dir = tempfile::tempdir().unwrap();
    let media = FakePhotoProcessor::new(
        PhotoInfo {
            width: 1800,
            height: 900,
            format: PhotoFormat::Png,
            may_have_alpha: true,
        },
        &[110, 95],
    );
    assert_eq!(
        media.prepare(dir.path(), 150).await.unwrap(),
        Some((dir.path().join("prepared").join("photo.png"), 95))
    );
    assert!(matches!(
        media.attempts.lock().unwrap().as_slice(),
        [
            (1800, 900, PhotoEncoding::Png),
            (1350, 675, PhotoEncoding::Png),
        ]
    ));
}

#[tokio::test]
async fn encoding_error_preserves_original_for_document_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let media = FakePhotoProcessor::new(jpeg(3000, 2000), &[]);
    assert!(matches!(
        media.prepare(dir.path(), 150).await,
        Err(MediaError::Ffmpeg(_))
    ));
    assert_eq!(
        tokio::fs::read(dir.path().join("original")).await.unwrap(),
        vec![0; 150]
    );
}
