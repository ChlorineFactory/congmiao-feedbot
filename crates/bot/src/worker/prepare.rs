//! Turning extracted media items into local files ready for upload.

use crate::app::{App, MediaTools};
use feedbot_core::extract::{MediaItem, MediaKind, MediaSource};
use feedbot_core::{
    MediaError, MediaInfo, MediaProcessor, PhotoEncoding, PhotoFormat, PhotoInfo, TranscodeSpec,
    YtdlpSpec,
};
use http::HeaderMap;
use std::path::{Path, PathBuf};
use tokio::task::JoinSet;
use tracing::warn;

const TELEGRAM_PHOTO_MAX_BYTES: u64 = 10_000_000;
const TELEGRAM_PHOTO_MAX_EDGE_SUM: u64 = 10_000;
const TELEGRAM_PHOTO_MAX_ASPECT: u64 = 20;

pub(super) fn telegram_photo_max_bytes(tools: &MediaTools) -> u64 {
    TELEGRAM_PHOTO_MAX_BYTES
        .min(tools.max_photo_bytes)
        .min(tools.max_upload_bytes)
}

pub(super) async fn prepare_all(
    app: &App,
    items: Vec<MediaItem>,
    dir: &Path,
) -> anyhow::Result<(Vec<PreparedMedia>, Vec<String>)> {
    let total = items.len();
    let mut set = JoinSet::new();
    for (idx, item) in items.into_iter().enumerate() {
        let tools = app.pipeline.media.clone();
        let dir = dir.to_path_buf();
        set.spawn(async move {
            let _permit = tools.limit.clone().acquire_owned().await?;
            let outcome = prepare_one(idx, item, &tools, &dir).await;
            anyhow::Ok((idx, outcome))
        });
    }

    let mut prepared = Vec::new();
    let mut skipped = Vec::new();
    let mut first_error: Option<(usize, anyhow::Error)> = None;
    while let Some(joined) = set.join_next().await {
        let (idx, outcome) = joined.map_err(|e| anyhow::anyhow!("download task: {e}"))??;
        match outcome {
            Ok(PrepareOutcome::Ready(item)) => prepared.push(item),
            Ok(PrepareOutcome::Skip(reason)) => skipped.push((idx, reason)),
            Err(e) => {
                warn!(idx, error = %e, "media item failed");
                skipped.push((idx, format!("第 {} 项失败：{e}", idx + 1)));
                if first_error.as_ref().is_none_or(|(i, _)| idx < *i) {
                    first_error = Some((idx, e));
                }
            }
        }
    }
    // If nothing could be prepared, surface the error so the job is retried
    // (download errors) or reported, instead of sending an empty result.
    if prepared.is_empty()
        && total > 0
        && let Some((_, e)) = first_error
    {
        return Err(e);
    }
    prepared.sort_by_key(|p| p.idx);
    skipped.sort_by_key(|(idx, _)| *idx);
    Ok((prepared, skipped.into_iter().map(|(_, r)| r).collect()))
}

enum PrepareOutcome {
    Ready(PreparedMedia),
    Skip(String),
}

pub(super) struct PreparedMedia {
    pub(super) idx: usize,
    pub(super) kind: MediaKind,
    pub(super) path: PathBuf,
    pub(super) filename: Option<String>,
    pub(super) cacheable: bool,
    pub(super) probe: Option<MediaInfo>,
    pub(super) thumbnail: Option<PathBuf>,
}

async fn prepare_one(
    idx: usize,
    item: MediaItem,
    tools: &MediaTools,
    dir: &Path,
) -> anyhow::Result<PrepareOutcome> {
    let MediaTools {
        downloader,
        processor: media,
        ytdlp,
        max_upload_bytes,
        ..
    } = tools;
    let mut thumb_url = item.cover.clone();
    let (mut path, mut filename, mut kind) = match item.source {
        MediaSource::Direct { url, headers } => {
            let path = dir.join(safe_name(item.filename.as_deref(), idx));
            downloader.to_file(&url, &headers, &path).await?;
            (path, item.filename, item.kind)
        }
        MediaSource::Separated {
            video,
            audio,
            headers,
        } => {
            let vpath = dir.join(format!("{idx}_v.bin"));
            let apath = dir.join(format!("{idx}_a.bin"));
            let out = dir.join(format!("{idx}_out.mp4"));
            tokio::try_join!(
                downloader.to_file(&video, &headers, &vpath),
                downloader.to_file(&audio, &headers, &apath),
            )?;
            media.remux(&[vpath, apath], &out).await?;
            (out, Some("video.mp4".into()), MediaKind::Video)
        }
        MediaSource::UgoiraZip {
            zip,
            headers,
            frames,
        } => {
            let zip_path = dir.join(format!("{idx}_ugoira.zip"));
            let work = dir.join(format!("{idx}_ugoira"));
            tokio::fs::create_dir_all(&work)
                .await
                .map_err(|e| MediaError::Io(e.to_string()))?;
            let out = dir.join(format!("{idx}_ugoira.mp4"));
            downloader.to_file(&zip, &headers, &zip_path).await?;
            media.ugoira_to_mp4(&zip_path, &frames, &work, &out).await?;
            (out, Some("ugoira.mp4".into()), MediaKind::Animation)
        }
        MediaSource::YtDlp {
            url,
            audio_only,
            max_height,
            thumbnail,
            cookies,
        } => {
            let (ext, kind) = if audio_only {
                ("m4a", MediaKind::Audio)
            } else {
                ("mp4", MediaKind::Video)
            };
            let dest = dir.join(format!("{idx}.{ext}"));
            ytdlp
                .download(
                    &url,
                    &dest,
                    &YtdlpSpec {
                        audio_only,
                        max_height,
                        cookies,
                        max_bytes: Some(*max_upload_bytes),
                    },
                )
                .await?;
            if thumbnail.is_some() {
                thumb_url = thumbnail;
            }
            let name = if audio_only { "audio.m4a" } else { "video.mp4" };
            (dest, Some(name.into()), kind)
        }
    };

    let mut thumbnail = None;
    if let Some(url) = thumb_url {
        let cover = dir.join(format!("{idx}_cover.jpg"));
        if downloader
            .to_file(&url, &HeaderMap::new(), &cover)
            .await
            .is_ok()
        {
            thumbnail = Some(cover);
        }
    }

    let mut size = file_size(&path).await?;
    let mut cacheable = true;
    if kind == MediaKind::Photo {
        let work_dir = dir.join(format!("photo-{idx}"));
        let max_bytes = telegram_photo_max_bytes(tools);
        match prepare_photo(&path, size, &work_dir, media.as_ref(), max_bytes).await {
            Ok(Some((photo_path, photo_size))) => {
                if photo_path != path {
                    filename = photo_path
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned());
                    path = photo_path;
                }
                size = photo_size;
            }
            Ok(None) => kind = MediaKind::Document,
            Err(error) => {
                warn!(idx, %error, "photo preparation failed; keeping original");
                kind = MediaKind::Document;
                cacheable = false;
            }
        }
    }

    if matches!(kind, MediaKind::Video | MediaKind::Animation) && size > *max_upload_bytes {
        let tc = dir.join(format!("{idx}_tc.mp4"));
        let spec = TranscodeSpec {
            max_bytes: max_upload_bytes.saturating_sub(1024 * 1024).max(1),
            ..TranscodeSpec::default()
        };
        if let Err(e) = media.transcode(&path, &tc, &spec).await {
            return Ok(PrepareOutcome::Skip(format!(
                "第 {} 项转码失败：{e}",
                idx + 1
            )));
        }
        path = tc;
        filename = Some("video.mp4".into());
        kind = MediaKind::Video;
        size = file_size(&path).await.unwrap_or(size);
    }

    if size > *max_upload_bytes {
        return Ok(PrepareOutcome::Skip(format!(
            "第 {} 项超过上传上限（{size} 字节）",
            idx + 1
        )));
    }

    let probe = if matches!(kind, MediaKind::Video | MediaKind::Animation) {
        media.probe(&path).await.ok()
    } else {
        None
    };
    if thumbnail.is_none() && kind == MediaKind::Video {
        let thumb = dir.join(format!("{idx}_thumb.jpg"));
        if media.thumbnail(&path, &thumb).await.is_ok() {
            thumbnail = Some(thumb);
        }
    }

    Ok(PrepareOutcome::Ready(PreparedMedia {
        idx,
        kind,
        path,
        filename,
        cacheable,
        probe,
        thumbnail,
    }))
}

async fn prepare_photo(
    original: &Path,
    size: u64,
    work_dir: &Path,
    media: &dyn MediaProcessor,
    max_bytes: u64,
) -> Result<Option<(PathBuf, u64)>, MediaError> {
    let Some(photo) = media.probe_photo(original).await? else {
        return Ok(None);
    };
    let (long, short) = photo_edges(photo);
    if short == 0 || long > TELEGRAM_PHOTO_MAX_ASPECT * short {
        return Ok(None);
    }

    if long + short <= TELEGRAM_PHOTO_MAX_EDGE_SUM
        && size <= max_bytes
        && matches!(photo.format, PhotoFormat::Jpeg | PhotoFormat::Png)
    {
        return Ok(Some((original.to_path_buf(), size)));
    }

    let mut edge_sum = (long + short).min(TELEGRAM_PHOTO_MAX_EDGE_SUM);
    let mut attempts = Vec::new();
    if photo.may_have_alpha || matches!(photo.format, PhotoFormat::Png) {
        attempts.push((edge_sum, PhotoEncoding::Png));
    }
    if !photo.may_have_alpha {
        for quality in [100, 88, 72] {
            attempts.push((edge_sum, PhotoEncoding::Jpeg { quality }));
        }
    }
    let resize_encoding = if photo.may_have_alpha {
        PhotoEncoding::Png
    } else {
        PhotoEncoding::Jpeg { quality: 88 }
    };
    for _ in 0..4 {
        edge_sum = edge_sum * 3 / 4;
        if edge_sum < 2 {
            break;
        }
        attempts.push((edge_sum, resize_encoding));
    }

    tokio::fs::create_dir(work_dir)
        .await
        .map_err(|e| MediaError::Io(e.to_string()))?;
    for (target_sum, encoding) in attempts {
        let (long_edge, short_edge) = fit_photo_edges(long, short, target_sum);
        let output = work_dir.join(match encoding {
            PhotoEncoding::Jpeg { .. } => "photo.jpg",
            PhotoEncoding::Png => "photo.png",
        });
        media
            .encode_photo(original, &output, long_edge, short_edge, encoding)
            .await?;
        let size = file_size(&output).await?;
        if size > 0
            && size <= max_bytes
            && let Some(encoded) = media.probe_photo(&output).await?
        {
            let (long, short) = photo_edges(encoded);
            if short > 0
                && long + short <= TELEGRAM_PHOTO_MAX_EDGE_SUM
                && long <= TELEGRAM_PHOTO_MAX_ASPECT * short
            {
                return Ok(Some((output, size)));
            }
        }
    }
    Ok(None)
}

fn photo_edges(photo: PhotoInfo) -> (u64, u64) {
    (
        u64::from(photo.width.max(photo.height)),
        u64::from(photo.width.min(photo.height)),
    )
}

fn fit_photo_edges(long: u64, short: u64, target_sum: u64) -> (u32, u32) {
    let sum = long + short;
    let long_edge = (long * target_sum / sum).max(1);
    // Rounding the short edge down can push a valid panorama past 20:1.
    let short_edge = (short * target_sum / sum).max(long_edge.div_ceil(TELEGRAM_PHOTO_MAX_ASPECT));
    (long_edge as u32, short_edge as u32)
}

async fn file_size(path: &Path) -> Result<u64, MediaError> {
    tokio::fs::metadata(path)
        .await
        .map(|m| m.len())
        .map_err(|e| MediaError::Io(e.to_string()))
}

fn safe_name(name: Option<&str>, idx: usize) -> String {
    name.and_then(|n| Path::new(n).file_name())
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .map(|s| format!("{idx}_{s}"))
        .unwrap_or_else(|| format!("item_{idx}"))
}

#[cfg(test)]
mod tests;
