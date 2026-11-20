use feedbot_core::{
    MediaError, MediaInfo, MediaProcessor, PhotoEncoding, PhotoFormat, PhotoInfo, TranscodeSpec,
    UgoiraFrame,
};
use serde::Deserialize;
use std::fs::File;
use std::io::{self, SeekFrom, Write, copy};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::process::Command;
use zip::ZipArchive;

/// Default wall-clock limit for one ffmpeg/ffprobe invocation.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

#[derive(Clone)]
pub struct FfmpegCli {
    ffmpeg: PathBuf,
    ffprobe: PathBuf,
    timeout: Duration,
}

impl FfmpegCli {
    pub fn new(ffmpeg: impl Into<PathBuf>, ffprobe: impl Into<PathBuf>) -> Self {
        Self {
            ffmpeg: ffmpeg.into(),
            ffprobe: ffprobe.into(),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        if !timeout.is_zero() {
            self.timeout = timeout;
        }
        self
    }

    async fn run(&self, bin: &Path, args: &[&str]) -> Result<String, MediaError> {
        let child = Command::new(bin)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output();
        let output = tokio::time::timeout(self.timeout, child)
            .await
            .map_err(|_| MediaError::Ffmpeg(format!("超时（{} 秒）", self.timeout.as_secs())))?
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    MediaError::MissingBinary
                } else {
                    MediaError::Ffmpeg(e.to_string())
                }
            })?;
        if !output.status.success() {
            return Err(MediaError::Ffmpeg(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

/// Concat demuxer list. The last frame is repeated because ffmpeg ignores the
/// duration of the final `file` entry.
pub(crate) fn build_concat_list(frames: &[UgoiraFrame]) -> String {
    let mut out = String::from("ffconcat version 1.0\n");
    for frame in frames {
        let name = concat_file_name(&frame.file);
        out.push_str("file ");
        out.push_str(&name);
        out.push('\n');
        out.push_str(&format!("duration {:.3}\n", frame.delay_ms as f64 / 1000.0));
    }
    if let Some(last) = frames.last() {
        out.push_str("file ");
        out.push_str(&concat_file_name(&last.file));
        out.push('\n');
    }
    out
}

fn concat_file_name(file: &str) -> String {
    Path::new(file)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(file)
        .to_string()
}

fn extract_zip_safe(zip_path: &Path, dest: &Path) -> Result<(), MediaError> {
    std::fs::create_dir_all(dest).map_err(|e| MediaError::Io(e.to_string()))?;
    let file = File::open(zip_path).map_err(|e| MediaError::Io(e.to_string()))?;
    let mut archive = ZipArchive::new(file).map_err(|e| MediaError::Io(e.to_string()))?;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| MediaError::Io(e.to_string()))?;
        if entry.is_dir() {
            continue;
        }
        let Some(rel) = entry.enclosed_name() else {
            continue;
        };
        if rel.components().count() != 1 {
            continue;
        }
        let out_path = dest.join(rel);
        let mut outfile = File::create(&out_path).map_err(|e| MediaError::Io(e.to_string()))?;
        copy(&mut entry, &mut outfile).map_err(|e| MediaError::Io(e.to_string()))?;
        outfile.flush().map_err(|e| MediaError::Io(e.to_string()))?;
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct ProbeJson {
    format: Option<ProbeFormat>,
    streams: Option<Vec<ProbeStream>>,
}

#[derive(Debug, Deserialize)]
struct ProbeFormat {
    format_name: Option<String>,
    duration: Option<String>,
    size: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProbeStream {
    codec_type: Option<String>,
    codec_name: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    pix_fmt: Option<String>,
    duration: Option<String>,
}

impl ProbeJson {
    fn photo_info(&self) -> Option<PhotoInfo> {
        let [stream] = self.streams.as_deref()? else {
            return None;
        };
        let format_name = self.format.as_ref()?.format_name.as_deref()?;
        let format = match (format_name, stream.codec_name.as_deref()?) {
            ("jpeg_pipe" | "image2", "mjpeg") => PhotoFormat::Jpeg,
            ("png_pipe" | "image2", "png") => PhotoFormat::Png,
            ("webp_pipe" | "image2" | "webp", "webp") => PhotoFormat::Webp,
            _ => return None,
        };
        let may_have_alpha = !matches!(format, PhotoFormat::Jpeg)
            && !matches!(
                stream.pix_fmt.as_deref(),
                Some("rgb24" | "rgb48be" | "gray" | "gray16be" | "monob" | "yuv420p" | "yuvj420p")
            );
        Some(PhotoInfo {
            width: stream.width?,
            height: stream.height?,
            format,
            may_have_alpha,
        })
    }
}

async fn png_is_animated(path: &Path) -> io::Result<bool> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut signature = [0; 8];
    file.read_exact(&mut signature).await?;
    if &signature != PNG_SIGNATURE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid PNG signature",
        ));
    }

    // FFprobe may report APNG as an ordinary PNG when the file ends in .png.
    // The animation control chunk must precede the first image data chunk.
    loop {
        let length = file.read_u32().await?;
        let mut kind = [0; 4];
        file.read_exact(&mut kind).await?;
        match &kind {
            b"acTL" => return Ok(true),
            b"IDAT" | b"IEND" => return Ok(false),
            _ => {
                file.seek(SeekFrom::Current(i64::from(length) + 4)).await?;
            }
        }
    }
}

async fn webp_is_animated(path: &Path) -> io::Result<bool> {
    // FFprobe can identify an animated WebP as webp_pipe. The VP8X flags byte
    // is the first byte after the four-byte chunk length.
    let mut header = [0; 21];
    tokio::fs::File::open(path)
        .await?
        .read_exact(&mut header)
        .await?;
    Ok(&header[12..16] == b"VP8X" && header[20] & 0b10 != 0)
}

#[async_trait::async_trait]
impl MediaProcessor for FfmpegCli {
    async fn probe(&self, path: &Path) -> Result<MediaInfo, MediaError> {
        let path_str = path.to_string_lossy();
        let json = self
            .run(
                &self.ffprobe,
                &[
                    "-v",
                    "quiet",
                    "-print_format",
                    "json",
                    "-show_format",
                    "-show_streams",
                    path_str.as_ref(),
                ],
            )
            .await?;
        let parsed: ProbeJson =
            serde_json::from_str(&json).map_err(|e| MediaError::Ffmpeg(e.to_string()))?;
        let mut info = MediaInfo {
            duration: parsed
                .format
                .as_ref()
                .and_then(|f| f.duration.as_ref())
                .and_then(|d| d.parse::<f64>().ok())
                .map(Duration::from_secs_f64),
            width: None,
            height: None,
            video_codec: None,
            audio_codec: None,
            size_bytes: parsed
                .format
                .as_ref()
                .and_then(|f| f.size.as_ref())
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
        };
        for stream in parsed.streams.unwrap_or_default() {
            match stream.codec_type.as_deref() {
                Some("video") => {
                    info.width = stream.width;
                    info.height = stream.height;
                    info.video_codec = stream.codec_name;
                    if info.duration.is_none() {
                        info.duration = stream
                            .duration
                            .as_ref()
                            .and_then(|d| d.parse::<f64>().ok())
                            .map(Duration::from_secs_f64);
                    }
                }
                Some("audio") => info.audio_codec = stream.codec_name,
                _ => {}
            }
        }
        Ok(info)
    }

    async fn probe_photo(&self, path: &Path) -> Result<Option<PhotoInfo>, MediaError> {
        let json = self
            .run(
                &self.ffprobe,
                &[
                    "-v",
                    "error",
                    "-select_streams",
                    "v",
                    "-show_entries",
                    "format=format_name:stream=codec_name,width,height,pix_fmt",
                    "-of",
                    "json",
                    &path.to_string_lossy(),
                ],
            )
            .await?;
        let probe: ProbeJson =
            serde_json::from_str(&json).map_err(|e| MediaError::Ffmpeg(e.to_string()))?;
        let Some(photo) = probe.photo_info() else {
            return Ok(None);
        };
        let animated = match photo.format {
            PhotoFormat::Jpeg => Ok(false),
            PhotoFormat::Png => png_is_animated(path).await,
            PhotoFormat::Webp => webp_is_animated(path).await,
        }
        .map_err(|e| MediaError::Io(e.to_string()))?;
        Ok((!animated).then_some(photo))
    }

    async fn encode_photo(
        &self,
        input: &Path,
        output: &Path,
        long_edge: u32,
        short_edge: u32,
        encoding: PhotoEncoding,
    ) -> Result<(), MediaError> {
        let input = input.to_string_lossy();
        let output = output.to_string_lossy();
        // FFmpeg applies display rotation before the filter, so size the long
        // and short edges using the resulting orientation.
        let scale = format!(
            "scale='if(gte(iw,ih),{long_edge},{short_edge})':\
             'if(gte(iw,ih),{short_edge},{long_edge})':flags=lanczos"
        );
        let mut args = vec![
            "-v",
            "error",
            "-i",
            &input,
            "-map",
            "0:v:0",
            "-frames:v",
            "1",
            "-vf",
            &scale,
        ];
        let quantizer;
        match encoding {
            PhotoEncoding::Jpeg { quality } => {
                quantizer = (2 + (100 - u32::from(quality.min(100))) * 29 / 100).to_string();
                args.extend(["-c:v", "mjpeg", "-pix_fmt", "yuvj444p", "-q:v", &quantizer]);
            }
            PhotoEncoding::Png => {
                args.extend(["-c:v", "png", "-pix_fmt", "rgba", "-compression_level", "9"]);
            }
        }
        args.extend(["-update", "1", "-y", &output]);
        self.run(&self.ffmpeg, &args).await?;
        Ok(())
    }

    async fn remux(&self, inputs: &[PathBuf], output: &Path) -> Result<(), MediaError> {
        if inputs.is_empty() {
            return Err(MediaError::Ffmpeg("remux 没有输入文件".into()));
        }
        let out = output.to_string_lossy();
        let mut args: Vec<String> = Vec::new();
        for input in inputs {
            args.push("-i".into());
            args.push(input.to_string_lossy().into_owned());
        }
        args.extend([
            "-c".into(),
            "copy".into(),
            "-movflags".into(),
            "+faststart".into(),
            "-y".into(),
            out.into_owned(),
        ]);
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.run(&self.ffmpeg, &arg_refs).await?;
        Ok(())
    }

    async fn thumbnail(&self, input: &Path, output: &Path) -> Result<(), MediaError> {
        let inp = input.to_string_lossy();
        let out = output.to_string_lossy();
        self.run(
            &self.ffmpeg,
            &[
                "-ss",
                "0",
                "-i",
                inp.as_ref(),
                "-vframes",
                "1",
                "-vf",
                "scale='min(320,iw)':-2",
                "-q:v",
                "3",
                "-y",
                out.as_ref(),
            ],
        )
        .await?;
        Ok(())
    }

    async fn transcode(
        &self,
        input: &Path,
        output: &Path,
        spec: &TranscodeSpec,
    ) -> Result<(), MediaError> {
        let inp = input.to_string_lossy();
        let out = output.to_string_lossy();
        let vf = format!("scale=-2:'min({},ih)'", spec.max_height);
        self.run(
            &self.ffmpeg,
            &[
                "-i",
                inp.as_ref(),
                "-c:v",
                "libx264",
                "-preset",
                "veryfast",
                "-b:v",
                spec.video_bitrate,
                "-c:a",
                "aac",
                "-b:a",
                spec.audio_bitrate,
                "-vf",
                &vf,
                "-movflags",
                "+faststart",
                "-y",
                out.as_ref(),
            ],
        )
        .await?;
        Ok(())
    }

    async fn ugoira_to_mp4(
        &self,
        zip: &Path,
        frames: &[UgoiraFrame],
        work_dir: &Path,
        output: &Path,
    ) -> Result<(), MediaError> {
        if frames.is_empty() {
            return Err(MediaError::Ffmpeg("ugoira 没有帧".into()));
        }
        let zip = zip.to_path_buf();
        let work = work_dir.to_path_buf();
        tokio::task::spawn_blocking(move || extract_zip_safe(&zip, &work))
            .await
            .map_err(|e| MediaError::Io(e.to_string()))??;

        let list_path = work_dir.join("list.ffconcat");
        tokio::fs::write(&list_path, build_concat_list(frames))
            .await
            .map_err(|e| MediaError::Io(e.to_string()))?;

        let list = list_path.to_string_lossy().into_owned();
        let out = output.to_string_lossy().into_owned();
        match encode_ugoira(self, &list, &out, true).await {
            Err(MediaError::Ffmpeg(msg))
                if msg.to_ascii_lowercase().contains("unrecognized option") =>
            {
                encode_ugoira(self, &list, &out, false).await
            }
            other => other,
        }
    }
}

async fn encode_ugoira(
    cli: &FfmpegCli,
    list: &str,
    output: &str,
    fps_mode: bool,
) -> Result<(), MediaError> {
    let timing = if fps_mode { "-fps_mode" } else { "-vsync" };
    cli.run(
        &cli.ffmpeg,
        &[
            "-f",
            "concat",
            "-safe",
            "0",
            "-i",
            list,
            timing,
            "vfr",
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-pix_fmt",
            "yuv420p",
            "-vf",
            "scale=trunc(iw/2)*2:trunc(ih/2)*2",
            "-an",
            "-movflags",
            "+faststart",
            "-y",
            output,
        ],
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn photo_from_probe(format: &str, codec: &str, pixel_format: &str) -> Option<PhotoInfo> {
        let probe: ProbeJson = serde_json::from_value(serde_json::json!({
            "format": {"format_name": format},
            "streams": [{
                "codec_name": codec, "width": 9000, "height": 6000, "pix_fmt": pixel_format
            }]
        }))
        .unwrap();
        probe.photo_info()
    }

    #[test]
    fn photo_probe_distinguishes_opaque_and_potentially_transparent_images() {
        let jpeg = photo_from_probe("jpeg_pipe", "mjpeg", "yuvj444p").unwrap();
        assert!(matches!(jpeg.format, PhotoFormat::Jpeg));
        assert_eq!((jpeg.width, jpeg.height), (9000, 6000));
        assert!(!jpeg.may_have_alpha);

        let png = photo_from_probe("png_pipe", "png", "rgb24").unwrap();
        assert!(matches!(png.format, PhotoFormat::Png));
        assert!(!png.may_have_alpha);
        for pixel_format in ["rgba", "pal8"] {
            assert!(
                photo_from_probe("png_pipe", "png", pixel_format)
                    .unwrap()
                    .may_have_alpha
            );
        }
    }

    #[test]
    fn photo_probe_rejects_animated_png_and_mjpeg_video() {
        assert!(photo_from_probe("apng", "apng", "rgba").is_none());
        assert!(photo_from_probe("avi", "mjpeg", "yuvj420p").is_none());
    }

    #[tokio::test]
    async fn png_animation_is_detected_by_chunk_type() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.png");
        let mut header = PNG_SIGNATURE.to_vec();
        header.extend(b"\0\0\0\x0dIHDR");
        header.extend([0; 17]); // IHDR payload and CRC.
        header.extend(b"\0\0\0\x04tEXtacTL\0\0\0\0");

        let mut still = header.clone();
        still.extend(b"\0\0\0\0IDAT");
        tokio::fs::write(&path, still).await.unwrap();
        assert!(!png_is_animated(&path).await.unwrap());

        header.extend(b"\0\0\0\x08acTL");
        tokio::fs::write(&path, header).await.unwrap();
        assert!(png_is_animated(&path).await.unwrap());
    }

    #[tokio::test]
    async fn only_the_webp_animation_flag_marks_the_extended_image_as_animated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.webp");
        let mut header = *b"RIFF\x16\x00\x00\x00WEBPVP8X\x0a\x00\x00\x00\x10";
        tokio::fs::write(&path, header).await.unwrap();
        assert!(!webp_is_animated(&path).await.unwrap());

        header[20] |= 0b10;
        tokio::fs::write(&path, header).await.unwrap();
        assert!(webp_is_animated(&path).await.unwrap());
    }

    #[test]
    fn concat_list_repeats_last_frame_and_converts_ms() {
        let frames = [
            UgoiraFrame {
                file: "000000.jpg".into(),
                delay_ms: 70,
            },
            UgoiraFrame {
                file: "subdir/000001.jpg".into(),
                delay_ms: 100,
            },
        ];
        let list = build_concat_list(&frames);
        assert_eq!(
            list,
            "ffconcat version 1.0\n\
             file 000000.jpg\n\
             duration 0.070\n\
             file 000001.jpg\n\
             duration 0.100\n\
             file 000001.jpg\n"
        );
    }

    #[test]
    fn concat_list_single_frame() {
        let frames = [UgoiraFrame {
            file: "000000.jpg".into(),
            delay_ms: 50,
        }];
        let list = build_concat_list(&frames);
        assert_eq!(
            list,
            "ffconcat version 1.0\n\
             file 000000.jpg\n\
             duration 0.050\n\
             file 000000.jpg\n"
        );
    }
}
