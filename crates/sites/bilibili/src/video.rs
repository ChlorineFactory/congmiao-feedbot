//! Videos and bangumi episodes: view metadata and play streams.

use super::api::media_headers;
use super::links::parse_https_url;
use super::{BilibiliCdn, BilibiliExtractor};
use feedbot_core::extract::{
    DownloadSource, Engagement, ExtractCtx, ExtractError, Extracted, MediaItem, MediaKind,
    MediaSource,
};
use feedbot_core::options::OptionValue;
use http::{HeaderMap, HeaderValue, header};
use serde_json::Value;
use url::Url;

use feedbot_site_common::{json_str, json_u64};

pub(super) async fn extracted_from_view(
    this: &BilibiliExtractor,
    ctx: &ExtractCtx,
    data: Value,
    cookie: Option<&str>,
    options: &feedbot_core::options::OptionValues,
) -> Result<Extracted, ExtractError> {
    let cid = json_u64(&data, "/cid")
        .ok_or_else(|| ExtractError::Failed("视频详情缺少 cid".into()))?
        .to_string();
    let bvid = json_str(&data, "/bvid")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ExtractError::Failed("视频详情缺少 bvid".into()))?
        .to_string();
    let height = quality_height(options);
    let qn = qn_for(height);
    let play = this
        .api_json(
            ctx,
            &this.api_base,
            "/x/player/playurl",
            &[
                ("bvid".into(), bvid.clone()),
                ("cid".into(), cid),
                ("qn".into(), qn.into()),
                ("fnval".into(), "4048".into()),
                ("fourk".into(), "1".into()),
            ],
            cookie,
        )
        .await?;
    video_from_view_play(
        &bvid,
        &data,
        play.get("data").unwrap_or(&Value::Null),
        height,
        this.max_upload_bytes,
        cookie,
        this.cdn,
    )
}

pub(super) fn quality_height(options: &feedbot_core::options::OptionValues) -> u32 {
    match options.get("q") {
        Some(OptionValue::Enum(q)) => q.parse().unwrap_or(720),
        _ => 720,
    }
}

pub(super) fn qn_for(height: u32) -> &'static str {
    match height {
        1080.. => "80",
        720..=1079 => "64",
        480..=719 => "32",
        _ => "16",
    }
}

pub(crate) fn video_from_view_play(
    bvid: &str,
    data: &Value,
    play: &Value,
    target_height: u32,
    max_bytes: Option<u64>,
    cookie: Option<&str>,
    cdn: BilibiliCdn,
) -> Result<Extracted, ExtractError> {
    let mut headers = media_headers(cookie)?;
    headers.insert(header::RANGE, HeaderValue::from_static("bytes=0-"));
    let source = play_source(play, target_height, max_bytes, cdn, headers)
        .ok_or_else(|| ExtractError::Failed("没有可用的视频地址".into()))?;
    let cover = json_str(data, "/pic").and_then(parse_https_url);
    let owner_mid = json_u64(data, "/owner/mid");
    Ok(Extracted {
        extractor_id: "bilibili".into(),
        canonical_url: Url::parse(&format!("https://www.bilibili.com/video/{bvid}"))
            .map_err(|e| ExtractError::Failed(e.to_string()))?,
        title: json_str(data, "/title").map(str::to_string),
        author: json_str(data, "/owner/name").map(str::to_string),
        author_url: owner_mid
            .and_then(|n| Url::parse(&format!("https://space.bilibili.com/{n}")).ok()),
        description: json_str(data, "/desc")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        tags: Vec::new(),
        nsfw: false,
        ai: false,
        engagement: Engagement::packed(
            json_u64(data, "/stat/view"),
            json_u64(data, "/stat/like"),
            json_u64(data, "/stat/share"),
            json_u64(data, "/stat/reply"),
        ),
        items: vec![MediaItem {
            id: bvid.into(),
            kind: MediaKind::Video,
            source,
            filename: Some(format!("bilibili-{bvid}.mp4")),
            cover,
        }],
    })
}

/// `max_bytes`: prefer the best stream whose estimated size (bandwidth ×
/// duration) fits, so the result needs no transcoding before upload.
pub(super) fn play_source(
    play: &Value,
    target_height: u32,
    max_bytes: Option<u64>,
    cdn: BilibiliCdn,
    headers: HeaderMap,
) -> Option<MediaSource> {
    if let Some(stream) = play
        .pointer("/durl/0")
        .and_then(|stream| stream_urls(stream, &["url"], cdn))
    {
        return Some(MediaSource::Streams {
            stream,
            audio: None,
            headers,
        });
    }
    let videos = play.pointer("/dash/video").and_then(Value::as_array)?;
    let audio_bandwidth = json_u64(play, "/dash/audio/0/bandwidth").unwrap_or(0);
    let duration = json_u64(play, "/dash/duration")
        .or_else(|| json_u64(play, "/timelength").map(|ms| ms / 1000));
    let fits = |stream: &Value| match (max_bytes, duration, json_u64(stream, "/bandwidth")) {
        // 10 % headroom for container overhead and bitrate peaks.
        (Some(max), Some(secs), Some(bandwidth)) => {
            (bandwidth + audio_bandwidth).saturating_mul(secs) / 8 <= max / 10 * 9
        }
        _ => false,
    };
    let selected = pick_video_stream(videos, target_height, fits)?;
    let stream = stream_urls(selected, &["baseUrl", "base_url"], cdn)?;
    let audio = play
        .pointer("/dash/audio/0")
        .and_then(|stream| stream_urls(stream, &["baseUrl", "base_url"], cdn));
    Some(MediaSource::Streams {
        stream,
        audio,
        headers,
    })
}

fn stream_urls(
    stream: &Value,
    primary_fields: &[&str],
    cdn: BilibiliCdn,
) -> Option<DownloadSource> {
    let primary = primary_fields.iter().find_map(|&field| {
        stream
            .get(field)
            .and_then(Value::as_str)
            .and_then(parse_https_url)
    });
    let backups = ["backupUrl", "backup_url"]
        .into_iter()
        .filter_map(|field| stream.get(field).and_then(Value::as_array))
        .flatten()
        .filter_map(|value| value.as_str().and_then(parse_https_url));
    cdn.order(primary, backups)
}

/// Highest AVC stream within `target` height that `fits` the upload limit;
/// otherwise the highest within `target`, transcoded later if too large.
pub(super) fn pick_video_stream(
    streams: &[Value],
    target: u32,
    fits: impl Fn(&Value) -> bool,
) -> Option<&Value> {
    let target = u64::from(target);
    let within = |s: &&Value| json_u64(s, "/height").unwrap_or(u64::MAX) <= target;
    let avc = |s: &&Value| {
        json_u64(s, "/codecid") == Some(7)
            || json_str(s, "/codecs").is_some_and(|c| c.starts_with("avc"))
    };
    let height = |s: &&Value| json_u64(s, "/height").unwrap_or_default();
    streams
        .iter()
        .filter(within)
        .filter(avc)
        .filter(|s| fits(s))
        .max_by_key(height)
        .or_else(|| streams.iter().filter(within).filter(avc).max_by_key(height))
        .or_else(|| {
            streams
                .iter()
                .filter(within)
                .max_by_key(|s| json_u64(s, "/height").unwrap_or_default())
        })
        .or_else(|| {
            streams
                .iter()
                .filter(avc)
                .min_by_key(|s| json_u64(s, "/height").unwrap_or(u64::MAX))
        })
        .or_else(|| {
            streams
                .iter()
                .min_by_key(|s| json_u64(s, "/height").unwrap_or(u64::MAX))
        })
}
