//! Job pipeline: extract → prepare (download/process) → deliver (upload),
//! with retries, cancellation checks, and the Telegram `file_id` cache.

mod deliver;
mod prepare;
mod presence;

use crate::app::App;
use crate::queue::QueuedJob;
use deliver::{
    Outgoing, Post, Target, cache_entry, deliver, finish_delivery, send_page_link, send_text_post,
};
use feedbot_core::extract::{ExtractRequest, MediaKind, RequesterInfo};
use feedbot_core::job::MediaCacheEntry;
use feedbot_core::options::{OptionValue, OptionValues, split_option_tokens};
use feedbot_core::{ExtractError, JobState, MediaError, StoreError, load_saved_options};
use prepare::prepare_all;
use presence::Presence;
use std::sync::Arc;
use std::time::Duration;
use teloxide::RequestError;
use teloxide::types::{ChatAction, ChatId, MessageId, ReplyParameters};
use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, error, info, warn};

/// The job was cancelled (`/cancel`) or finished elsewhere; stop silently.
#[derive(Debug, thiserror::Error)]
#[error("任务已取消")]
struct JobCancelled;

/// Run at most `max_jobs` jobs at once. A slot is taken before a job is
/// received, so while all slots are busy jobs wait in the bounded channel and
/// `JobQueue::submit` reports a full queue instead of piling up tasks.
pub async fn run_worker_pool(
    app: Arc<App>,
    mut rx: mpsc::Receiver<QueuedJob>,
    max_jobs: usize,
    cancel: CancellationToken,
) {
    let slots = Arc::new(Semaphore::new(max_jobs.max(1)));
    loop {
        let permit = tokio::select! {
            _ = cancel.cancelled() => break,
            permit = slots.clone().acquire_owned() => match permit {
                Ok(permit) => permit,
                Err(_) => break,
            },
        };
        let job = tokio::select! {
            _ = cancel.cancelled() => break,
            job = rx.recv() => match job {
                Some(job) => job,
                None => break,
            },
        };
        let app = app.clone();
        let span = tracing::info_span!("job", job_id = %job.id);
        tokio::spawn(
            async move {
                let _permit = permit;
                if process_job(app, job).await.is_err() {
                    error!("failed to save final job state");
                }
            }
            .instrument(span),
        );
    }
}

async fn process_job(app: Arc<App>, job: QueuedJob) -> anyhow::Result<()> {
    let mut presence = Presence::new(app.bot.clone(), ChatId(job.chat_id), job.message_id);
    let max_attempts = app.pipeline.max_attempts.max(1);
    let mut attempt = 1;
    let err = loop {
        app.store.bump_attempt(job.id).await.ok();
        match run_once(&app, &job, &mut presence).await {
            Ok(()) => {
                app.store.set_state(job.id, JobState::Done, None).await?;
                info!(attempt, "job completed");
                presence.stop().await;
                return Ok(());
            }
            Err(e) if e.is::<JobCancelled>() => {
                info!("job cancelled");
                presence.stop().await;
                return Ok(());
            }
            Err(e) if is_retryable(&e) && attempt < max_attempts => {
                let delay = Duration::from_secs(2u64.pow(attempt));
                warn!(
                    attempt,
                    max_attempts,
                    delay_secs = delay.as_secs(),
                    cause = job_failure_kind(&e),
                    "retrying job"
                );
                presence.stop_refresh();
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
            Err(e) => break e,
        }
    };
    let failure = job_failure_message(&err, attempt);
    let still_running = app
        .store
        .set_state(job.id, JobState::Failed, Some(&failure))
        .await?;
    if still_running {
        presence.fail(&format!("失败：{failure}")).await;
    } else {
        presence.stop().await;
    }
    Ok(())
}

fn job_failure_message(error: &anyhow::Error, attempt: u32) -> String {
    let Some(RequestError::Network(source)) = error.downcast_ref::<RequestError>() else {
        warn!(attempt, cause = job_failure_kind(error), "job failed");
        return error.to_string();
    };
    let (cause, message) = if source.is_connect() {
        ("connect", "无法连接 Telegram Bot API")
    } else if source.is_timeout() {
        ("timeout", "请求 Telegram Bot API 超时")
    } else if source.is_body() {
        ("body", "向 Telegram Bot API 发送数据失败")
    } else {
        ("transport", "请求 Telegram Bot API 时发生网络错误")
    };
    let io_kind = error.chain().find_map(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .map(std::io::Error::kind)
    });
    warn!(
        attempt,
        cause,
        timed_out = source.is_timeout(),
        ?io_kind,
        "Telegram Bot API request failed"
    );
    message.into()
}

fn job_failure_kind(error: &anyhow::Error) -> &'static str {
    if let Some(error) = error.downcast_ref::<ExtractError>() {
        return match error {
            ExtractError::NoExtractor => "no_extractor",
            ExtractError::AuthRequired(_) => "authentication_required",
            ExtractError::Network(_) => "extract_network",
            ExtractError::Failed(_) => "extract_failed",
            ExtractError::Options(_) => "extract_options",
            ExtractError::Unsupported => "unsupported_link",
        };
    }
    if let Some(error) = error.downcast_ref::<MediaError>() {
        return media_failure_kind(error);
    }
    if error.is::<RequestError>() {
        "telegram_request"
    } else if error.is::<StoreError>() {
        "store"
    } else {
        "unexpected"
    }
}

fn media_failure_kind(error: &MediaError) -> &'static str {
    match error {
        MediaError::MissingBinary => "missing_ffmpeg",
        MediaError::Ffmpeg(_) => "ffmpeg",
        MediaError::MissingYtdlp => "missing_yt_dlp",
        MediaError::Ytdlp(_) => "yt_dlp",
        MediaError::Download(_) => "download",
        MediaError::TooLarge(_) => "download_size_limit",
        MediaError::Io(_) => "media_io",
    }
}

fn is_retryable(err: &anyhow::Error) -> bool {
    if let Some(e) = err.downcast_ref::<ExtractError>() {
        return e.retryable();
    }
    matches!(
        err.downcast_ref::<MediaError>(),
        Some(MediaError::Download(_))
    )
}

/// Record progress; aborts the run when the job was cancelled meanwhile.
async fn advance(app: &App, job: &QueuedJob, state: JobState) -> anyhow::Result<()> {
    if app.store.set_state(job.id, state, None).await? {
        info!(stage = state.as_str(), "job stage started");
        Ok(())
    } else {
        Err(JobCancelled.into())
    }
}

async fn run_once(app: &App, job: &QueuedJob, presence: &mut Presence) -> anyhow::Result<()> {
    advance(app, job, JobState::Extracting).await?;
    presence.set_action(ChatAction::Typing).await;

    let extract_permit = app.pipeline.extract_limit.acquire().await?;
    let url = app.registry.canonicalize(&job.url);
    let Some(extractor) = app.registry.match_url(&url) else {
        return Err(ExtractError::NoExtractor.into());
    };
    let (tokens, leftover) = split_option_tokens(&job.raw_args);
    if !leftover.is_empty() {
        warn!("ignored unrecognized option fragments");
    }
    let saved =
        load_saved_options(app.store.as_ref(), extractor.id(), job.user_id, job.chat_id).await?;
    let options = extractor.resolve_options(&saved, &tokens)?;
    let credential = app.store.resolve(extractor.id(), job.user_id).await?;
    let source_key = OptionValues::cache_key(url.as_str(), extractor.options(), &options);
    let req = ExtractRequest {
        url,
        options: options.clone(),
        credential,
        requester: RequesterInfo {
            user_id: job.user_id,
            chat_id: job.chat_id,
        },
    };
    // Extraction always runs, even on a cache hit: it doubles as the access
    // check (login, deletion) and refreshes caption metadata.
    debug!(platform = extractor.id(), "requesting metadata");
    let mut extracted = extractor.extract(&app.pipeline.extract_ctx, &req).await?;
    drop(extract_permit);

    let spoiler = media_spoiler(&options, extracted.nsfw);
    let present = extractor.present();
    let total_items = extracted.items.len();
    info!(
        platform = extractor.id(),
        items = total_items,
        "extraction completed"
    );
    let target = Target {
        bot: &app.bot,
        chat: ChatId(job.chat_id),
        reply: reply_params(job),
    };

    if total_items == 0 {
        advance(app, job, JobState::Uploading).await?;
        return send_text_post(
            &target,
            Post {
                present,
                extracted: &extracted,
            },
        )
        .await;
    }

    if let Some(page) = app
        .pipeline
        .overflow
        .publish_page(&extracted, &source_key)
        .await
    {
        info!(items = total_items, "published as Telegraph page");
        advance(app, job, JobState::Uploading).await?;
        send_page_link(
            &target,
            Post {
                present,
                extracted: &extracted,
            },
            &page,
            spoiler,
        )
        .await?;
        return Ok(());
    }

    // Older photo file IDs can bypass the current preparation and upload limits.
    let source_key = if extracted
        .items
        .iter()
        .any(|item| item.kind == MediaKind::Photo)
    {
        let max_photo_bytes = prepare::telegram_photo_max_bytes(&app.pipeline.media);
        let max_upload_bytes = app.pipeline.media.max_upload_bytes;
        format!("{source_key}#telegram-photo-v3-{max_photo_bytes}-{max_upload_bytes}")
    } else {
        source_key
    };
    let hits = app.store.get_cache_all(&source_key).await?;
    if hits.len() == total_items {
        info!(items = hits.len(), "using cached Telegram files");
        advance(app, job, JobState::Uploading).await?;
        presence.set_action(upload_action(hits[0].kind)).await;
        let outgoing: Vec<Outgoing> = hits.iter().map(Outgoing::cached).collect();
        let (group_count, _) = deliver(
            app,
            &target,
            Post {
                present,
                extracted: &extracted,
            },
            &outgoing,
            spoiler,
        )
        .await?;
        finish_delivery(
            app,
            &target,
            Post {
                present,
                extracted: &extracted,
            },
            outgoing.len(),
            total_items,
            group_count,
            &[],
        )
        .await?;
        return Ok(());
    }

    advance(app, job, JobState::Downloading).await?;
    let dir = tempfile::tempdir().map_err(|e| MediaError::Io(e.to_string()))?;
    let items = std::mem::take(&mut extracted.items);
    let (prepared, skipped) = prepare_all(app, items, dir.path()).await?;
    info!(
        prepared = prepared.len(),
        skipped = skipped.len(),
        "media preparation completed"
    );

    advance(app, job, JobState::Uploading).await?;
    presence
        .set_action(
            prepared
                .first()
                .map(|item| upload_action(item.kind))
                .unwrap_or(ChatAction::Typing),
        )
        .await;

    let outgoing: Vec<Outgoing> = prepared.iter().map(Outgoing::local).collect();
    let (group_count, sent) = deliver(
        app,
        &target,
        Post {
            present,
            extracted: &extracted,
        },
        &outgoing,
        spoiler,
    )
    .await?;
    // Cache complete sets only. A temporary preparation failure should be retried on the next request.
    if skipped.is_empty() && prepared.iter().all(|item| item.cacheable) {
        let entries: Vec<MediaCacheEntry> = sent
            .iter()
            .filter_map(|(pos, msg)| cache_entry(&source_key, *pos, outgoing[*pos].kind, msg))
            .collect();
        if entries.len() == outgoing.len() && app.store.put_many(&entries).await.is_err() {
            warn!("failed to store media cache");
        }
    }
    finish_delivery(
        app,
        &target,
        Post {
            present,
            extracted: &extracted,
        },
        outgoing.len(),
        total_items,
        group_count,
        &skipped,
    )
    .await?;
    drop(dir);
    Ok(())
}

fn reply_params(job: &QueuedJob) -> Option<ReplyParameters> {
    (job.message_id != 0).then(|| ReplyParameters::new(MessageId(job.message_id)))
}

fn media_spoiler(options: &OptionValues, nsfw: bool) -> bool {
    match options.get("sp") {
        Some(OptionValue::Flag(flag)) => *flag,
        _ => nsfw,
    }
}

fn upload_action(kind: MediaKind) -> ChatAction {
    match kind {
        MediaKind::Photo => ChatAction::UploadPhoto,
        MediaKind::Video | MediaKind::Animation => ChatAction::UploadVideo,
        MediaKind::Audio | MediaKind::Document => ChatAction::UploadDocument,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r18_spoilers_by_default() {
        let empty = OptionValues::new();
        assert!(media_spoiler(&empty, true));
        assert!(!media_spoiler(&empty, false));
    }

    #[test]
    fn plus_sp_spoilers_sfw() {
        let mut v = OptionValues::new();
        v.insert("sp", OptionValue::Flag(true));
        assert!(media_spoiler(&v, false));
        assert!(media_spoiler(&v, true));
    }

    #[test]
    fn minus_sp_disables_r18() {
        let mut v = OptionValues::new();
        v.insert("sp", OptionValue::Flag(false));
        assert!(!media_spoiler(&v, true));
        assert!(!media_spoiler(&v, false));
    }
}
