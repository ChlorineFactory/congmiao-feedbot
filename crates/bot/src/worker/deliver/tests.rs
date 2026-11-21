use super::*;

fn extracted() -> Extracted {
    Extracted {
        extractor_id: "test".into(),
        canonical_url: Url::parse("https://example.com/post").unwrap(),
        title: Some("Album title".into()),
        author: None,
        author_url: None,
        description: Some("Album description".into()),
        tags: Vec::new(),
        nsfw: false,
        ai: false,
        engagement: None,
        items: Vec::new(),
    }
}

fn outgoing(kind: MediaKind) -> Outgoing {
    Outgoing {
        kind,
        file: InputFile::file_id(FileId("test-file".into())),
        probe: None,
        thumbnail: None,
    }
}

#[test]
fn photo_video_caption_goes_on_first_attachment_of_each_group() {
    let extracted = extracted();
    let post = Post {
        present: Present::Gallery,
        extracted: &extracted,
    };
    let items = [
        MediaKind::Photo,
        MediaKind::Video,
        MediaKind::Video,
        MediaKind::Photo,
    ]
    .map(outgoing);

    let first_group = album_media(post, &items, &[0, 1], true);
    let [InputMedia::Photo(photo), InputMedia::Video(video)] = first_group.as_slice() else {
        panic!("expected photo followed by video");
    };
    assert_eq!(photo.caption, caption_for_index(post, 0, 4));
    assert!(video.caption.is_none());
    assert!(photo.has_spoiler && video.has_spoiler);

    let second_group = album_media(post, &items, &[2, 3], true);
    let [InputMedia::Video(video), InputMedia::Photo(photo)] = second_group.as_slice() else {
        panic!("expected video followed by photo");
    };
    assert_eq!(video.caption, caption_for_index(post, 2, 4));
    assert!(photo.caption.is_none());
    assert!(video.has_spoiler && photo.has_spoiler);
}

#[test]
fn audio_and_document_groups_keep_each_items_caption() {
    let extracted = extracted();
    let post = Post {
        present: Present::Gallery,
        extracted: &extracted,
    };
    let items = [
        MediaKind::Audio,
        MediaKind::Audio,
        MediaKind::Document,
        MediaKind::Document,
    ]
    .map(outgoing);

    let audio_group = album_media(post, &items, &[0, 1], false);
    let [InputMedia::Audio(first), InputMedia::Audio(second)] = audio_group.as_slice() else {
        panic!("expected two audio attachments");
    };
    assert_eq!(first.caption, caption_for_index(post, 0, 4));
    assert_eq!(second.caption, caption_for_index(post, 1, 4));

    let document_group = album_media(post, &items, &[2, 3], false);
    let [InputMedia::Document(first), InputMedia::Document(second)] = document_group.as_slice()
    else {
        panic!("expected two document attachments");
    };
    assert_eq!(first.caption, caption_for_index(post, 2, 4));
    assert_eq!(second.caption, caption_for_index(post, 3, 4));
}
