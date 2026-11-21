//! Post layout (X).

use super::{build_caption, build_caption_page};
use crate::caption::{TEXT_LIMIT, build_text_message, take_utf16};
use feedbot_core::{
    CaptionPage, Engagement, Extracted, MediaItem, MediaKind, MediaSource, Present,
};
use http::HeaderMap;
use teloxide::utils::html;
use url::Url;

fn sample(
    desc: Option<&str>,
    nsfw: bool,
    pages: usize,
    engagement: Option<Engagement>,
    with_author_url: bool,
) -> Extracted {
    let items = (0..pages)
        .map(|i| MediaItem {
            id: format!("p{i}"),
            kind: MediaKind::Photo,
            source: MediaSource::Direct {
                url: Url::parse("https://example.com/p.png").unwrap(),
                headers: HeaderMap::new(),
            },
            filename: None,
            cover: None,
        })
        .collect();
    Extracted {
        extractor_id: "x".into(),
        canonical_url: Url::parse("https://x.com/i/status/1").unwrap(),
        title: Some("ignored title".into()),
        author: Some("bob".into()),
        author_url: with_author_url.then(|| Url::parse("https://x.com/bob").unwrap()),
        description: desc.map(str::to_string),
        tags: Vec::new(),
        nsfw,
        ai: false,
        engagement,
        items,
    }
}

#[test]
fn caption_is_stats_plus_expandable_without_title_or_author_links() {
    let rendered = build_caption(&sample(
        Some("hello\nworld"),
        true,
        1,
        Some(Engagement {
            views: Some(12_345),
            likes: Some(320),
            shares: Some(18),
            comments: Some(41),
        }),
        true,
    ));
    assert!(rendered.caption.starts_with("#NSFW\n"));
    assert!(
        rendered
            .caption
            .contains("浏览 1.2万 · 点赞 320 · 分享 18 · 评论 41")
    );
    assert!(
        rendered
            .caption
            .contains("<blockquote expandable>hello\nworld</blockquote>")
    );
    assert!(!rendered.caption.contains("<a href="));
    assert!(!rendered.caption.contains(" / bob"));
    assert!(!rendered.caption.contains("ignored title"));
    assert!(rendered.extras.is_empty());
}

#[test]
fn text_only_post_keeps_the_body_visible_and_splits_at_the_message_limit() {
    let body = format!("<&>\n{}", "😀".repeat(TEXT_LIMIT / 2 + 1));
    let extracted = sample(Some(&body), false, 0, None, true);
    let rendered = build_text_message(Present::Post { origin: "原帖" }, &extracted);
    let (first, rest) = take_utf16(&body, TEXT_LIMIT);

    assert_eq!(rendered.caption, html::escape(first));
    assert_eq!(rendered.extras, [html::escape(rest)]);
}

#[test]
fn album_pages_use_index_over_total() {
    let extracted = sample(Some("hi"), false, 4, None, false);
    let first = build_caption_page(&extracted, CaptionPage { index: 0, total: 4 });
    assert!(first.caption.contains("1/4"));
    assert!(!first.caption.contains("2/4"));
    let second = build_caption_page(&extracted, CaptionPage { index: 1, total: 4 });
    assert!(second.caption.contains("2/4"));
    assert!(!second.caption.contains("1/4"));
}

#[test]
fn keyboard_has_author_and_post_or_post_only() {
    let both = super::media_keyboard(&sample(None, false, 1, None, true)).unwrap();
    let row = &both.inline_keyboard[0];
    assert_eq!(row.len(), 2);
    assert_eq!(row[0].text, "作者");
    assert_eq!(row[1].text, "原帖");

    let post_only = super::media_keyboard(&sample(None, false, 1, None, false)).unwrap();
    assert_eq!(post_only.inline_keyboard[0].len(), 1);
    assert_eq!(post_only.inline_keyboard[0][0].text, "原帖");
}
