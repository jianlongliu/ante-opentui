//! Pasted images: opencode's inline `data:` URL → Ante's `@path` mention.
//!
//! opencode hands a pasted image over in the prompt body's `files`, as a
//! `data:` URL. Ante's protocol takes text and nothing else. What Ante *does*
//! resolve is an `@path` mention of an image file — its own TUI and its ACP
//! front end both deliver images that way — so the shim writes each pasted
//! image into Ante's paste cache and appends the mention to the text it sends.
//!
//! Ante drops an attachment whose base64 passes its cap instead of shrinking
//! it, which reads to the model as "no image", so anything oversized is
//! re-encoded here first. `MAX_STAGED_BYTES` is the byte budget the encoded
//! image has to fit in; 160 KB of bytes is ~213 KB of base64, under the ~240 KB
//! Ante accepts.

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use image::ImageEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, ImageFormat};
use serde_json::{Value, json};

/// Ante's paste cache: the directory its own TUI pastes into, and the one its
/// ACP front end writes to. A mention of a file here is read as an attachment
/// rather than as text, and Ante's own housekeeping owns the directory.
pub fn paste_cache() -> PathBuf {
    std::env::temp_dir().join("ante-paste-cache")
}

/// The encoded image has to fit this. Ante attaches a mentioned image only up
/// to its own cap and otherwise substitutes a "too large" notice.
const MAX_STAGED_BYTES: usize = 160 * 1024;
/// A pasted file beyond this is a mistake, not an attachment.
const MAX_RAW_BYTES: usize = 20 * 1024 * 1024;
/// Long edge to shrink to: enough detail for a vision model, cheap in tokens.
const MAX_EDGE: u32 = 1568;
/// Every path the shim stages starts with this, so a logged message can be told
/// apart from one the user typed.
const STAGE_PREFIX: &str = "antex-";

/// Stage a prompt's attachments. Answers the text to hand Ante — the user's own,
/// plus one `@path` mention per staged file — and the attachment list to echo
/// back, which is what the client renders (its optimistic copy carries no file
/// bodies: those are server-loaded in opencode).
pub fn stage(text: &str, files: &[Value]) -> (String, Vec<Value>) {
    let mut sent = text.to_string();
    let mut echo = Vec::new();
    for file in files {
        match stage_one(file) {
            Ok(Some((entry, mention))) => {
                if let Some(mention) = mention {
                    sent.push(' ');
                    sent.push_str(&mention);
                }
                echo.push(entry);
            }
            Ok(None) => {}
            Err(err) => crate::log_line(&format!("attachments: 附件被丢弃：{err}")),
        }
    }
    (sent, echo)
}

/// `Ok(None)` is "nothing to do with this one" — a URI we are not meant to
/// fetch, or a file type Ante has no way to read. `Err` is worth a log line.
fn stage_one(file: &Value) -> Result<Option<(Value, Option<String>)>, String> {
    let Some(uri) = file
        .get("uri")
        .and_then(Value::as_str)
        .filter(|uri| !uri.is_empty())
    else {
        return Ok(None);
    };
    let name = file.get("name").and_then(Value::as_str).unwrap_or("");
    if let Some(rest) = uri.strip_prefix("data:") {
        let (mime, bytes) = decode_data_url(rest, name)?;
        if bytes.len() > MAX_RAW_BYTES {
            return Err(format!("{name} 超过 {} MB", MAX_RAW_BYTES / 1024 / 1024));
        }
        return match kind(&mime) {
            // A pasted image travels as a mention: Ante turns that into an image
            // attachment, which is the only channel its protocol has.
            Kind::Image => {
                let Some(prepared) = prepare(bytes, &mime) else {
                    return Err(format!("{name} 无法解码为图片"));
                };
                let path = write_staged(&prepared, name)?;
                let entry = entry(&prepared.bytes, &prepared.mime, name, None);
                Ok(Some((entry, Some(mention(&path)))))
            }
            // Text and PDFs: Ante reads a mention of these as file content, so
            // the same mention is all that is needed.
            Kind::Text => {
                let ext = extension(&mime);
                let path = write_raw(&bytes, ext, name, "file")?;
                let entry = entry(&bytes, &mime, name, None);
                Ok(Some((entry, Some(mention(&path)))))
            }
            Kind::Other => Err(format!("{name} 的类型 {mime} Ante 读不了")),
        };
    }
    // `file://` attachments are files the client already points at in the text
    // (`@path`), so Ante reads them without help from us. The entry is still
    // echoed, because the client's transcript renders the server's copy.
    if let Some(path) = file_path(uri) {
        return stage_local(Path::new(&path), name);
    }
    Ok(None)
}

/// A `file://` attachment: no mention to add, but the client still wants the
/// body echoed back so the transcript can draw it.
fn stage_local(path: &Path, name: &str) -> Result<Option<(Value, Option<String>)>, String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) if bytes.len() <= MAX_RAW_BYTES => bytes,
        Ok(_) => {
            return Err(format!(
                "{} 超过 {} MB",
                path.display(),
                MAX_RAW_BYTES / 1024 / 1024
            ));
        }
        Err(err) => return Err(format!("读不了 {}：{err}", path.display())),
    };
    let label = if name.is_empty() {
        &basename(path)
    } else {
        name
    };
    let mime = mime_for(path);
    let entry = if mime.starts_with("image/") && mime != "image/svg+xml" {
        let Some(prepared) = prepare(bytes, &mime) else {
            return Err(format!("{label} 无法解码为图片"));
        };
        entry(&prepared.bytes, &prepared.mime, label, Some(uri_of(path)))
    } else {
        entry(&bytes, &mime, label, Some(uri_of(path)))
    };
    Ok(Some((entry, None)))
}

/// The attachment object the client expects: `data`/`mime`/`source` are
/// required, `name` is what its chip renders.
fn entry(bytes: &[u8], mime: &str, name: &str, uri: Option<String>) -> Value {
    let source = match uri {
        Some(uri) => json!({ "type": "uri", "uri": uri }),
        None => json!({ "type": "inline" }),
    };
    let mut value = json!({ "data": encode(bytes), "mime": mime, "source": source });
    if !name.is_empty() {
        value["name"] = json!(name);
    }
    value
}

enum Kind {
    Image,
    Text,
    Other,
}

fn kind(mime: &str) -> Kind {
    if mime.starts_with("image/") && mime != "image/svg+xml" {
        Kind::Image
    } else if mime == "application/pdf"
        || mime.starts_with("text/")
        || matches!(
            mime,
            "image/svg+xml" | "application/json" | "application/xml"
        )
    {
        Kind::Text
    } else {
        Kind::Other
    }
}

struct Prepared {
    bytes: Vec<u8>,
    mime: &'static str,
    ext: &'static str,
}

/// An image, re-encoded if Ante would refuse it as-is. `None` means it could not
/// be decoded at all — a corrupt paste, which there is no point forwarding.
fn prepare(bytes: Vec<u8>, mime: &str) -> Option<Prepared> {
    if bytes.len() <= MAX_STAGED_BYTES
        && let Some((mime, ext)) = still_image(mime)
    {
        return Some(Prepared { bytes, mime, ext });
    }
    let decoded = image::load_from_memory(&bytes).ok()?;
    let prefer_png =
        matches!(mime, "image/png" | "image/gif" | "image/bmp") || decoded.color().has_alpha();
    shrink(decoded, prefer_png)
}

/// Formats Ante takes unchanged, with the extension to write them under.
fn still_image(mime: &str) -> Option<(&'static str, &'static str)> {
    Some(match mime {
        "image/png" => ("image/png", "png"),
        "image/jpeg" => ("image/jpeg", "jpg"),
        "image/gif" => ("image/gif", "gif"),
        "image/webp" => ("image/webp", "webp"),
        _ => return None,
    })
}

/// Down the ladder until the encoded image fits the budget: fewer pixels first,
/// then a lower JPEG quality. PNG is tried first for sources that were PNG —
/// screenshots and text keep their edges that way — and JPEG for photographs.
fn shrink(img: DynamicImage, prefer_png: bool) -> Option<Prepared> {
    let (width, height) = (img.width(), img.height());
    let long_edge = width.max(height);
    let mut sizes: Vec<(u32, u32)> = Vec::new();
    if long_edge <= MAX_EDGE {
        sizes.push((width, height));
    }
    for edge in [MAX_EDGE, 1280, 1024, 800, 640, 512] {
        if edge >= long_edge {
            continue;
        }
        let scale = edge as f32 / long_edge as f32;
        let size = (
            ((width as f32 * scale).round() as u32).max(1),
            ((height as f32 * scale).round() as u32).max(1),
        );
        if !sizes.contains(&size) {
            sizes.push(size);
        }
    }
    let attempts: &[(bool, u8)] = if prefer_png {
        &[(false, 0), (true, 88), (true, 76), (true, 64), (true, 52)]
    } else {
        &[(true, 88), (true, 76), (true, 64), (true, 52), (false, 0)]
    };
    for (target_w, target_h) in sizes {
        let frame = if (target_w, target_h) == (width, height) {
            img.clone()
        } else {
            img.resize(target_w, target_h, FilterType::Lanczos3)
        };
        for &(as_jpeg, quality) in attempts {
            let encoded = if as_jpeg {
                encode_jpeg(&frame, quality)
            } else {
                encode_png(&frame)
            };
            let Some(bytes) = encoded else { continue };
            if bytes.len() <= MAX_STAGED_BYTES {
                return Some(if as_jpeg {
                    Prepared {
                        bytes,
                        mime: "image/jpeg",
                        ext: "jpg",
                    }
                } else {
                    Prepared {
                        bytes,
                        mime: "image/png",
                        ext: "png",
                    }
                });
            }
        }
    }
    None
}

fn encode_jpeg(img: &DynamicImage, quality: u8) -> Option<Vec<u8>> {
    let rgb = img.to_rgb8();
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality)
        .write_image(
            rgb.as_raw(),
            rgb.width(),
            rgb.height(),
            image::ExtendedColorType::Rgb8,
        )
        .ok()?;
    Some(out)
}

fn encode_png(img: &DynamicImage) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    img.write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
        .ok()?;
    Some(out)
}

/// Write a staged file into the paste cache. The stamp leads — it is what makes
/// the name unique, and putting it first keeps the original name (which may
/// itself end in digits) readable on replay.
fn write_staged(prepared: &Prepared, name: &str) -> Result<PathBuf, String> {
    write_raw(&prepared.bytes, prepared.ext, name, "image")
}

fn write_raw(bytes: &[u8], ext: &str, name: &str, fallback: &str) -> Result<PathBuf, String> {
    let stem = sanitize(name, fallback);
    let path = paste_cache().join(format!("{STAGE_PREFIX}{}-{stem}.{ext}", stamp()));
    write_file(&path, bytes)
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<PathBuf, String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|err| format!("建不了 {}：{err}", dir.display()))?;
    }
    std::fs::write(path, bytes).map_err(|err| format!("写不了 {}：{err}", path.display()))?;
    Ok(path.to_path_buf())
}

/// Ante reads a mention up to the next unescaped whitespace, so a path with a
/// space in it has to escape them.
fn mention(path: &Path) -> String {
    let text = path.to_string_lossy();
    let mut token = String::from("@");
    for ch in text.chars() {
        if ch.is_whitespace() {
            token.push('\\');
        }
        token.push(ch);
    }
    token
}

fn sanitize(name: &str, fallback: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('.').trim_matches('_');
    let stem = trimmed.split('.').next().unwrap_or("").trim_matches('_');
    if stem.is_empty() {
        fallback.to_string()
    } else {
        stem.chars().take(40).collect()
    }
}

fn stamp() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    format!("{nanos}-{}", std::process::id())
}

fn encode(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// `data:` payload → its mime and bytes. Only the base64 form is produced by the
/// client, but the URL-encoded form costs nothing to accept.
fn decode_data_url(rest: &str, label: &str) -> Result<(String, Vec<u8>), String> {
    let (meta, payload) = rest
        .split_once(',')
        .ok_or_else(|| format!("{label} 的 data URL 没有正文"))?;
    let mime = meta.split(';').next().unwrap_or("").to_string();
    if meta
        .split(';')
        .any(|part| part.eq_ignore_ascii_case("base64"))
    {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(payload.trim())
            .map_err(|err| format!("{label} 的 base64 解不开：{err}"))?;
        return Ok((mime, bytes));
    }
    Ok((mime, percent_decode(payload).into_bytes()))
}

/// The local path a `file://` URI names, percent-decoded.
fn file_path(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    (authority.is_empty() || authority == "localhost").then(|| percent_decode(path))
}

fn percent_decode(encoded: &str) -> String {
    let bytes = encoded.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let escaped = (bytes[i] == b'%' && i + 2 < bytes.len())
            .then(|| std::str::from_utf8(&bytes[i + 1..i + 3]).ok())
            .flatten()
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match escaped {
            Some(byte) => {
                decoded.push(byte);
                i += 3;
            }
            None => {
                decoded.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn uri_of(path: &Path) -> String {
    format!("file://{}", path.display())
}

fn basename(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn extension(mime: &str) -> &'static str {
    match mime {
        "application/pdf" => "pdf",
        "application/json" => "json",
        "application/xml" => "xml",
        "image/svg+xml" => "svg",
        _ => "txt",
    }
}

fn mime_for(path: &Path) -> String {
    let ext = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        "pdf" => "application/pdf",
        "json" => "application/json",
        "xml" => "application/xml",
        "svg" => "image/svg+xml",
        "md" => "text/markdown",
        "txt" | "log" | "rs" | "ts" | "tsx" | "js" | "py" | "sh" | "toml" | "yaml" | "yml" => {
            "text/plain"
        }
        _ => "application/octet-stream",
    }
    .to_string()
}

/// Ante appends what it loaded for a mention — the mentioned folder's listing —
/// to the message it echoes back. The transcript shows the message the user
/// wrote, not that expansion.
pub fn strip_expansion(echoed: &str) -> &str {
    match echoed.find("\n\n<folder-structure>") {
        Some(at) => &echoed[..at],
        None => echoed,
    }
}

/// A message as Ante recorded it → what the transcript should show: the staged
/// mentions turn back into attachments (when their file is still in the cache)
/// and drop out of the text; the expansion never had a place in it.
pub fn from_log(recorded: &str) -> (String, Vec<Value>) {
    let text = strip_expansion(recorded);
    let bytes = text.as_bytes();
    let mut shown = String::with_capacity(text.len());
    let mut files = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_whitespace() {
            shown.push(bytes[i] as char);
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let token = &text[start..i];
        match restore(token) {
            Some(entry) => files.push(entry),
            None => shown.push_str(token),
        }
    }
    (shown.trim_end().to_string(), files)
}

/// One token, if it is a mention the shim staged and the file is still there.
fn restore(token: &str) -> Option<Value> {
    let path = PathBuf::from(token.strip_prefix('@')?.replace("\\ ", " "));
    if !basename(&path).starts_with(STAGE_PREFIX) {
        return None;
    }
    let bytes = std::fs::read(&path).ok()?;
    let mime = mime_for(&path);
    let mut entry = entry(&bytes, &mime, &original_name(&path), None);
    entry["mime"] = json!(mime);
    Some(entry)
}

/// `antex-1759253000123456789-1234-clipboard.png` → `clipboard.png`: the stamp
/// leads the name, so the original — digits and all — is what follows it.
fn original_name(path: &Path) -> String {
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("");
    let rest = stem.strip_prefix(STAGE_PREFIX).unwrap_or(stem);
    let slug = rest
        .splitn(3, '-')
        .nth(2)
        .filter(|slug| !slug.is_empty())
        .unwrap_or("attachment");
    match path.extension().and_then(|ext| ext.to_str()) {
        Some(ext) => format!("{slug}.{ext}"),
        None => slug.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut img = image::RgbImage::new(width, height);
        for (x, y, pixel) in img.enumerate_pixels_mut() {
            // Noise, so the encoding has nothing to hide behind.
            *pixel = image::Rgb([(x % 251) as u8, (y % 241) as u8, ((x * y) % 239) as u8]);
        }
        encode_png(&DynamicImage::ImageRgb8(img)).expect("encode png")
    }

    fn data_url(mime: &str, bytes: &[u8]) -> Value {
        json!({ "uri": format!("data:{mime};base64,{}", encode(bytes)), "name": "clipboard" })
    }

    #[test]
    fn an_image_becomes_a_mention_and_an_entry() {
        let (sent, echo) = stage("看这张图", &[data_url("image/png", &png(64, 48))]);
        let token = sent.split_whitespace().last().expect("mention");
        assert!(token.starts_with("@/"), "{token}");
        let path = PathBuf::from(token.trim_start_matches('@'));
        assert!(path.exists(), "{}", path.display());
        let staged = basename(&path);
        assert!(
            staged.starts_with(STAGE_PREFIX) && staged.ends_with("-clipboard.png"),
            "{staged}"
        );
        assert_eq!(echo.len(), 1);
        assert_eq!(echo[0]["mime"], "image/png");
        assert_eq!(echo[0]["name"], "clipboard");
        assert_eq!(echo[0]["source"], json!({ "type": "inline" }));
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(echo[0]["data"].as_str().expect("data"))
            .expect("base64");
        assert_eq!(decoded, std::fs::read(&path).expect("read staged"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_oversized_image_is_shrunk_under_the_budget() {
        let big = png(3000, 2000);
        assert!(big.len() > MAX_STAGED_BYTES);
        let prepared = prepare(big, "image/png").expect("prepared");
        assert!(
            prepared.bytes.len() <= MAX_STAGED_BYTES,
            "{}",
            prepared.bytes.len()
        );
        let decoded = image::load_from_memory(&prepared.bytes).expect("decodes");
        assert!(decoded.width().max(decoded.height()) <= MAX_EDGE);
    }

    #[test]
    fn a_path_with_spaces_is_escaped_in_the_mention() {
        assert_eq!(mention(Path::new("/tmp/a b/c.png")), "@/tmp/a\\ b/c.png");
    }

    #[test]
    fn a_file_uri_is_echoed_but_not_mentioned() {
        let path = paste_cache().join("antex-test-local.png");
        std::fs::create_dir_all(paste_cache()).expect("cache dir");
        std::fs::write(&path, png(32, 32)).expect("write");
        let file = json!({ "uri": uri_of(&path) });
        let (sent, echo) = stage("看图", std::slice::from_ref(&file));
        assert_eq!(sent, "看图");
        assert_eq!(echo.len(), 1);
        assert_eq!(echo[0]["source"]["type"], "uri");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_unknown_type_is_dropped() {
        let file = json!({ "uri": "data:application/zip;base64,AAAA" });
        let (sent, echo) = stage("附件", std::slice::from_ref(&file));
        assert_eq!(sent, "附件");
        assert!(echo.is_empty());
    }

    #[test]
    fn a_logged_message_comes_back_as_text_plus_attachment() {
        let mut file = data_url("image/png", &png(64, 48));
        file["name"] = json!("ANTEX-42");
        let (sent, _) = stage("看这张图", std::slice::from_ref(&file));
        let token = sent.split_whitespace().last().expect("mention").to_string();
        let recorded = format!("{sent}\n\n<folder-structure>\n- /tmp/\n  - x.png\n\n");
        let (shown, files) = from_log(&recorded);
        assert_eq!(shown, "看这张图");
        assert_eq!(files.len(), 1);
        // The stamp leads the staged name, so a name that ends in digits survives.
        assert_eq!(files[0]["name"], "ANTEX-42.png");
        assert_eq!(files[0]["mime"], "image/png");
        let path = PathBuf::from(token.trim_start_matches('@'));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_mention_the_user_typed_is_left_alone() {
        let (shown, files) = from_log("看看 @src/main.rs 里的这段");
        assert_eq!(shown, "看看 @src/main.rs 里的这段");
        assert!(files.is_empty());
    }

    #[test]
    fn the_expansion_is_stripped_for_matching() {
        let sent = "看图 @/tmp/ante-paste-cache/antex-clipboard-1-2.png";
        let echoed = format!("{sent}\n\n<folder-structure>\n- /tmp/\n\n");
        assert_eq!(strip_expansion(&echoed), sent);
    }
}
