// image_paste.rs — Clipboard image detection and text paste via subprocess.
//
// Supports three operations:
//   1. `read_clipboard_text()` — read text from the system clipboard
//   2. `read_clipboard_image()` — detect an image in the clipboard and save to a temp file
//   3. Helper structs for image attachments shown in the prompt
//
// All clipboard access uses platform CLI tools (no native Rust bindings needed):
//   macOS  : pbpaste / osascript
//   Linux  : xclip / wl-paste
//   Windows: PowerShell Get-Clipboard

use std::path::{Path, PathBuf};
use std::process::Command;

// ---------------------------------------------------------------------------
// Image attachment state
// ---------------------------------------------------------------------------

/// A pasted image attachment waiting to be included in the next message.
#[derive(Debug, Clone)]
pub struct PastedImage {
    /// Path to the temporary image file on disk (format sniffed from bytes).
    pub path: PathBuf,
    /// Display label shown in the prompt (e.g. "clipboard.png" or "image.png").
    pub label: String,
    /// Original dimensions, if known.
    pub dimensions: Option<(u32, u32)>,
}

// ---------------------------------------------------------------------------
// Clipboard text reading
// ---------------------------------------------------------------------------

/// Read text from the system clipboard. Returns `None` if the clipboard is
/// empty, unavailable, or contains non-text data.
pub fn read_clipboard_text() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        read_text_macos()
    }
    #[cfg(target_os = "windows")]
    {
        read_text_windows()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        read_text_linux()
    }
}

/// Read text from the primary selection when supported (Linux/X11/Wayland).
pub fn read_primary_text() -> Option<String> {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        None
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        read_primary_text_linux()
    }
}

#[cfg(target_os = "macos")]
fn read_text_macos() -> Option<String> {
    let out = Command::new("pbpaste").output().ok()?;
    if out.status.success() && !out.stdout.is_empty() {
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        None
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn read_text_linux() -> Option<String> {
    read_text_linux_selection(false)
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn read_primary_text_linux() -> Option<String> {
    read_text_linux_selection(true)
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn read_text_linux_selection(primary: bool) -> Option<String> {
    let commands: &[(&str, &[&str])] = if primary {
        &[
            ("wl-paste", &["--primary", "--no-newline"]),
            ("xclip", &["-selection", "primary", "-o"]),
            ("xsel", &["--primary", "--output"]),
        ]
    } else {
        &[
            ("wl-paste", &["--no-newline"]),
            ("xclip", &["-selection", "clipboard", "-o"]),
            ("xsel", &["--clipboard", "--output"]),
        ]
    };

    for (prog, args) in commands {
        if let Ok(out) = Command::new(prog).args(*args).output() {
            if out.status.success() && !out.stdout.is_empty() {
                return Some(String::from_utf8_lossy(&out.stdout).into_owned());
            }
        }
    }

    // tmux: when there is no system clipboard at all (SSH into a headless
    // box, no X/Wayland session), tmux still keeps its own copy buffer.
    // `tmux show-buffer` prints the default buffer — the terminal-world
    // equivalent of the copy buffer. Only the clipboard (not the primary
    // selection) has a tmux counterpart.
    if !primary {
        if let Ok(out) = Command::new("tmux").arg("show-buffer").output() {
            if out.status.success() && !out.stdout.is_empty() {
                return Some(String::from_utf8_lossy(&out.stdout).into_owned());
            }
        }
    }
    None
}

#[cfg(target_os = "windows")]
fn read_text_windows() -> Option<String> {
    let out = Command::new("powershell")
        .args(["-NoProfile", "-Command", "Get-Clipboard"])
        .output()
        .ok()?;
    if out.status.success() && !out.stdout.is_empty() {
        Some(
            String::from_utf8_lossy(&out.stdout)
                .trim_end_matches('\n')
                .to_string(),
        )
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Clipboard image reading
// ---------------------------------------------------------------------------

/// Check whether the clipboard currently holds an image. If it does, write
/// the PNG to a temp file and return a `PastedImage`.
pub fn read_clipboard_image() -> Option<PastedImage> {
    #[cfg(target_os = "macos")]
    {
        read_image_macos()
    }
    #[cfg(target_os = "windows")]
    {
        read_image_windows()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        read_image_linux()
    }
}

// ── macOS ──────────────────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
fn read_image_macos() -> Option<PastedImage> {
    // Check whether the clipboard contains an image type
    let check = Command::new("osascript")
        .args(["-e", "the clipboard as «class PNGf»"])
        .output()
        .ok()?;

    if !check.status.success() || check.stdout.is_empty() {
        return None;
    }

    // Write the PNG bytes to a temp file
    let tmp = make_temp_png()?;

    let script = format!(
        r#"set pngData to (the clipboard as «class PNGf»)
set fp to open for access POSIX file "{}" with write permission
write pngData to fp
close access fp"#,
        tmp.display()
    );

    let write_out = Command::new("osascript")
        .args(["-e", &script])
        .output()
        .ok()?;
    if write_out.status.success() && tmp.exists() && tmp.metadata().ok()?.len() > 0 {
        let dims = png_dimensions(&tmp);
        Some(PastedImage {
            label: "clipboard.png".to_string(),
            path: tmp,
            dimensions: dims,
        })
    } else {
        let _ = std::fs::remove_file(&tmp);
        None
    }
}

// ── Linux ──────────────────────────────────────────────────────────────────

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn read_image_linux() -> Option<PastedImage> {
    // xclip (X11): enumerate the offered clipboard targets, then fetch the
    // best image type the owner actually provides. Many applications
    // (browsers, chat clients) only offer `image/jpeg` or `image/webp`;
    // blindly requesting `image/png` fails on those, which is why pasting a
    // copied browser image used to report "No image in clipboard".
    if let Some(out) =
        run_clipboard_cmd("xclip", &["-selection", "clipboard", "-t", "TARGETS", "-o"])
    {
        let targets = String::from_utf8_lossy(&out.stdout);
        if let Some(mime) = pick_image_mime(&targets) {
            if let Some(out) =
                run_clipboard_cmd("xclip", &["-selection", "clipboard", "-t", &mime, "-o"])
            {
                if let Some(img) = save_clipboard_bytes(out.stdout) {
                    return Some(img);
                }
            }
        }
    }

    // wl-paste (Wayland): same negotiation against `--list-types`.
    if let Some(out) = run_clipboard_cmd("wl-paste", &["--list-types"]) {
        let types = String::from_utf8_lossy(&out.stdout);
        if let Some(mime) = pick_image_mime(&types) {
            if let Some(out) = run_clipboard_cmd("wl-paste", &["--type", &mime]) {
                if let Some(img) = save_clipboard_bytes(out.stdout) {
                    return Some(img);
                }
            }
        }
    }
    None
}

/// Run a clipboard helper and return its output only when it succeeded and
/// produced non-empty stdout.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn run_clipboard_cmd(prog: &str, args: &[&str]) -> Option<std::process::Output> {
    let out = Command::new(prog).args(args).output().ok()?;
    if out.status.success() && !out.stdout.is_empty() {
        Some(out)
    } else {
        None
    }
}

/// Persist raw clipboard image bytes to a temp file, sniffing the real format
/// so the extension and label match the actual payload.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn save_clipboard_bytes(data: Vec<u8>) -> Option<PastedImage> {
    if data.is_empty() {
        return None;
    }
    let media = sniff_media_type(&data);
    let ext = extension_for_media_type(media);
    let tmp = make_temp_image(ext)?;
    std::fs::write(&tmp, &data).ok()?;
    let dims = image_dimensions(&tmp);
    Some(PastedImage {
        label: format!("clipboard.{}", ext),
        path: tmp,
        dimensions: dims,
    })
}

// ── Windows ────────────────────────────────────────────────────────────────

#[cfg(target_os = "windows")]
fn read_image_windows() -> Option<PastedImage> {
    // Check whether clipboard has an image
    let check = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "if ((Get-Clipboard -Format Image) -ne $null) { 'yes' } else { 'no' }",
        ])
        .output()
        .ok()?;

    let answer = String::from_utf8_lossy(&check.stdout).trim().to_string();
    if answer != "yes" {
        return None;
    }

    let tmp = make_temp_png()?;
    let tmp_str = tmp.display().to_string();

    let script = format!(
        "$img = Get-Clipboard -Format Image; \
         $img.Save('{}', [System.Drawing.Imaging.ImageFormat]::Png)",
        tmp_str.replace('\'', "''")
    );

    let save = Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .output()
        .ok()?;

    if save.status.success() && tmp.exists() && tmp.metadata().ok()?.len() > 0 {
        let dims = png_dimensions(&tmp);
        Some(PastedImage {
            label: "clipboard.png".to_string(),
            path: tmp,
            dimensions: dims,
        })
    } else {
        let _ = std::fs::remove_file(&tmp);
        None
    }
}

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Clipboard text writing
// ---------------------------------------------------------------------------

/// Write text to the system clipboard. Returns `true` on success.
pub fn write_clipboard_text(text: &str) -> bool {
    #[cfg(target_os = "macos")]
    {
        write_text_macos_w(text)
    }
    #[cfg(target_os = "windows")]
    {
        write_text_windows_w(text)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        write_text_linux_w(text)
    }
}

#[cfg(target_os = "macos")]
fn write_text_macos_w(text: &str) -> bool {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = match Command::new("pbcopy").stdin(Stdio::piped()).spawn() {
        Ok(c) => c,
        Err(_) => return false,
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes());
    }
    child.wait().map(|s| s.success()).unwrap_or(false)
}

#[cfg(target_os = "windows")]
fn write_text_windows_w(text: &str) -> bool {
    use std::io::Write;
    use std::process::Stdio;
    // PowerShell Set-Clipboard reads from stdin via pipe
    let script =
        format!("[Console]::InputEncoding = [System.Text.Encoding]::UTF8; $input | Set-Clipboard");
    let mut child = match Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .stdin(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes());
    }
    child.wait().map(|s| s.success()).unwrap_or(false)
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn write_text_linux_w(text: &str) -> bool {
    let clipboard_ok = write_text_linux_selection(text, false);
    let primary_ok = write_text_linux_selection(text, true);
    clipboard_ok || primary_ok
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn write_text_linux_selection(text: &str, primary: bool) -> bool {
    use std::io::Write;
    use std::process::Stdio;

    let commands: &[(&str, &[&str])] = if primary {
        &[
            ("wl-copy", &["--primary"]),
            ("xclip", &["-selection", "primary"]),
            ("xsel", &["--primary", "--input"]),
        ]
    } else {
        &[
            ("wl-copy", &[]),
            ("xclip", &["-selection", "clipboard"]),
            ("xsel", &["--clipboard", "--input"]),
        ]
    };

    for (prog, args) in commands {
        if let Ok(mut child) = Command::new(prog).args(*args).stdin(Stdio::piped()).spawn() {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            if child.wait().map(|s| s.success()).unwrap_or(false) {
                return true;
            }
        }
    }
    false
}

/// macOS/Windows clipboard paths always rasterize to PNG; the Linux path
/// picks the extension from the sniffed format instead.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn make_temp_png() -> Option<PathBuf> {
    make_temp_image("png")
}

fn make_temp_image(ext: &str) -> Option<PathBuf> {
    let tmp_dir = std::env::temp_dir();
    let name = format!(
        "claude-paste-{}.{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
        ext
    );
    Some(tmp_dir.join(name))
}

/// Preferred clipboard image MIME types in order of preference.
///
/// Applications copy images to the clipboard in wildly different formats:
/// screenshots are usually PNG, but browsers and chat clients frequently
/// offer only JPEG or WebP. Asking for `image/png` unconditionally fails on
/// those, so we pick the best type the clipboard owner actually offers.
const IMAGE_MIME_PRIORITY: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/jpg",
    "image/webp",
    "image/tiff",
    "image/bmp",
    "image/gif",
];

/// Pick the best image MIME type from a whitespace/newline-separated list of
/// offered clipboard types (xclip `TARGETS` or `wl-paste --list-types`).
/// MIME parameters (`;charset=…`) are ignored when matching. Falls back to
/// any `image/*` type when none of the preferred ones is offered.
pub fn pick_image_mime(offered: &str) -> Option<String> {
    let offered_types: Vec<&str> = offered
        .split(|c: char| c.is_whitespace())
        .map(|t| t.split(';').next().unwrap_or(t))
        .filter(|t| !t.is_empty())
        .collect();
    for preferred in IMAGE_MIME_PRIORITY {
        if offered_types
            .iter()
            .any(|t| t.eq_ignore_ascii_case(preferred))
        {
            return Some((*preferred).to_string());
        }
    }
    offered_types
        .iter()
        .find(|t| t.starts_with("image/"))
        .map(|t| t.to_string())
}

/// Sniff the MIME type of an image from its magic bytes.
/// Falls back to `image/png` when the format is unknown.
pub fn sniff_media_type(data: &[u8]) -> &'static str {
    if data.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
        "image/png"
    } else if data.len() >= 3 && data[0] == 0xFF && data[1] == 0xD8 && data[2] == 0xFF {
        "image/jpeg"
    } else if data.len() >= 12 && data.starts_with(b"RIFF") && data[8..12] == *b"WEBP" {
        "image/webp"
    } else if data.starts_with(b"GIF8") {
        "image/gif"
    } else if data.starts_with(b"BM") {
        "image/bmp"
    } else if data.len() >= 4
        && (data.starts_with(b"II\x2a\x00") || data.starts_with(b"MM\x00\x2a"))
    {
        "image/tiff"
    } else {
        "image/png"
    }
}

fn extension_for_media_type(media: &str) -> &'static str {
    match media {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "image/bmp" => "bmp",
        "image/tiff" => "tiff",
        _ => "png",
    }
}

/// macOS/Windows clipboard paths always produce PNG.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn png_dimensions(path: &Path) -> Option<(u32, u32)> {
    let data = std::fs::read(path).ok()?;
    if data.len() < 24 {
        return None;
    }
    // PNG signature: 8 bytes; IHDR: 4 len + 4 type + 4 w + 4 h
    if &data[0..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let w = u32::from_be_bytes([data[16], data[17], data[18], data[19]]);
    let h = u32::from_be_bytes([data[20], data[21], data[22], data[23]]);
    Some((w, h))
}

/// Read image dimensions from common image headers (PNG + JPEG).
/// Returns `None` for unknown or malformed formats.
pub fn image_dimensions(path: &Path) -> Option<(u32, u32)> {
    let data = std::fs::read(path).ok()?;
    if data.starts_with(&[0x89, 0x50, 0x4E, 0x47]) && data.len() >= 24 {
        // PNG signature: 8 bytes; IHDR: 4 len + 4 type + 4 w + 4 h
        let w = u32::from_be_bytes([data[16], data[17], data[18], data[19]]);
        let h = u32::from_be_bytes([data[20], data[21], data[22], data[23]]);
        return Some((w, h));
    }
    if data.len() >= 2 && data[0] == 0xFF && data[1] == 0xD8 {
        return jpeg_dimensions(&data);
    }
    None
}

/// Parse JPEG dimensions from an SOF marker (SOF0–SOF15, excluding the
/// non-image markers DHT/C8/DAC/CC). Returns `None` for malformed streams.
/// All SOF variants (baseline, progressive, lossless) carry width/height in
/// the same header layout.
fn jpeg_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2; // skip SOI marker
    while i + 4 <= data.len() {
        if data[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = data[i + 1];
        // Fill byte, stuffed byte, standalone markers without a length field.
        if marker == 0x00 || marker == 0xFF {
            i += 1;
            continue;
        }
        if marker == 0xD8 || marker == 0xD9 || (0xD0..=0xD7).contains(&marker) {
            i += 2;
            continue;
        }
        let seg_len = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
        if seg_len < 2 {
            return None;
        }
        // SOF markers carry the frame header: precision(1) height(2) width(2).
        if matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            if seg_len < 5 || i + 2 + seg_len > data.len() {
                return None;
            }
            let h = u16::from_be_bytes([data[i + 5], data[i + 6]]) as u32;
            let w = u16::from_be_bytes([data[i + 7], data[i + 8]]) as u32;
            return Some((w, h));
        }
        i += 2 + seg_len;
    }
    None
}

/// Build a base64 `ImageSource` from an image file on disk, sniffing the real
/// media type from the file bytes rather than trusting the extension (a
/// `.png`-named file may hold JPEG data and vice versa). Returns `None` when
/// the file cannot be read.
pub fn image_source_from_file(path: &Path) -> Option<clawde_core::types::ImageSource> {
    let data = std::fs::read(path).ok()?;
    if data.is_empty() {
        return None;
    }
    let media_type = sniff_media_type(&data).to_string();
    let data = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &data);
    Some(clawde_core::types::ImageSource {
        source_type: "base64".to_string(),
        media_type: Some(media_type),
        data: Some(data),
        url: None,
    })
}

/// Check whether a file path points to a supported image format.
/// Used to detect `@path/to/image.png` references in the prompt so images
/// can be auto-attached without clipboard tools (critical for SSH users).
pub fn is_image_path(path: &std::path::Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("png")
            | Some("jpg")
            | Some("jpeg")
            | Some("gif")
            | Some("webp")
            | Some("bmp")
            | Some("tiff")
            | Some("tif")
            | Some("avif")
            | Some("heif")
            | Some("heic")
    )
}

/// Read a file and base64-encode it for the Anthropic API.
pub fn encode_image_base64(path: &Path) -> Option<String> {
    let data = std::fs::read(path).ok()?;
    Some(base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        &data,
    ))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pasted_image_clone() {
        let img = PastedImage {
            path: PathBuf::from("/tmp/test.png"),
            label: "test.png".to_string(),
            dimensions: Some((800, 600)),
        };
        let cloned = img.clone();
        assert_eq!(cloned.label, "test.png");
        assert_eq!(cloned.dimensions, Some((800, 600)));
    }

    #[test]
    fn make_temp_image_produces_unique_names() {
        // Just check it returns a path under tmp with the requested suffix
        let p = make_temp_image("png").unwrap();
        assert!(p.to_string_lossy().contains("claude-paste-"));
        assert!(p.to_string_lossy().ends_with(".png"));
        let j = make_temp_image("jpg").unwrap();
        assert!(j.to_string_lossy().ends_with(".jpg"));
    }

    #[test]
    fn image_dimensions_reads_png_ihdr() {
        // Minimal valid PNG IHDR: 8-byte sig + 4-byte length + "IHDR" + 4-byte w + 4-byte h + ...
        let mut data = vec![0u8; 24];
        data[0..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        // IHDR chunk: length=13
        data[8..12].copy_from_slice(&13u32.to_be_bytes());
        data[12..16].copy_from_slice(b"IHDR");
        // width = 100
        data[16..20].copy_from_slice(&100u32.to_be_bytes());
        // height = 200
        data[20..24].copy_from_slice(&200u32.to_be_bytes());
        let tmp = make_temp_image("png").unwrap();
        std::fs::write(&tmp, &data).unwrap();
        let dims = image_dimensions(&tmp);
        assert_eq!(dims, Some((100, 200)));
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn encode_image_base64_missing_file_returns_none() {
        let p = PathBuf::from("/nonexistent/file.png");
        assert!(encode_image_base64(&p).is_none());
    }

    #[test]
    fn pick_image_mime_prefers_png() {
        // Screenshot-style clipboard: PNG offered alongside text.
        assert_eq!(
            pick_image_mime("TARGETS\nimage/png\ntext/plain").as_deref(),
            Some("image/png")
        );
    }

    #[test]
    fn pick_image_mime_falls_back_to_jpeg() {
        // Browser "Copy image": JPEG only. This is the case the old code
        // got wrong (it asked for image/png unconditionally and failed).
        assert_eq!(
            pick_image_mime("TARGETS\nimage/jpeg\ntext/html\ntext/plain").as_deref(),
            Some("image/jpeg")
        );
    }

    #[test]
    fn pick_image_mime_prefers_png_over_jpeg_when_both_offered() {
        assert_eq!(
            pick_image_mime("image/jpeg\nimage/png\nimage/webp").as_deref(),
            Some("image/png")
        );
    }

    #[test]
    fn pick_image_mime_ignores_parameters_and_case() {
        assert_eq!(
            pick_image_mime("text/plain;charset=utf-8\nIMAGE/PNG").as_deref(),
            Some("image/png")
        );
    }

    #[test]
    fn pick_image_mime_any_image_type_fallback() {
        // Unknown image type still matches rather than giving up.
        assert_eq!(
            pick_image_mime("image/x-pict\ntext/plain").as_deref(),
            Some("image/x-pict")
        );
    }

    #[test]
    fn pick_image_mime_none_for_text_only() {
        assert_eq!(pick_image_mime("TARGETS\ntext/plain"), None);
        assert_eq!(pick_image_mime(""), None);
    }

    #[test]
    fn sniff_media_type_detects_formats() {
        let png = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        assert_eq!(sniff_media_type(&png), "image/png");
        assert_eq!(sniff_media_type(&[0xFF, 0xD8, 0xFF, 0xE0]), "image/jpeg");
        let mut webp = Vec::from(&b"RIFF\x00\x00\x00\x00WEBP"[..]);
        assert_eq!(sniff_media_type(&webp), "image/webp");
        webp[8..12].copy_from_slice(b"XXXX");
        assert_eq!(sniff_media_type(&webp), "image/png"); // not a real webp
        assert_eq!(sniff_media_type(b"GIF89a"), "image/gif");
        assert_eq!(sniff_media_type(b"BM\x00\x00"), "image/bmp");
        assert_eq!(sniff_media_type(b"II\x2a\x00\x08"), "image/tiff");
        assert_eq!(sniff_media_type(b"\x00\x01\x02\x03"), "image/png"); // unknown → png default
    }

    #[test]
    fn image_dimensions_reads_jpeg_sof() {
        // Minimal JPEG: SOI + SOF0 (8-bit, 2x1) + EOI.
        // SOF0 segment: FF C0 00 0B 08 00 02 00 01 01 01 11 00
        let mut data = Vec::new();
        data.extend_from_slice(&[0xFF, 0xD8]); // SOI
        data.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x00, 0x02, 0x00, 0x01]);
        data.extend_from_slice(&[0x01, 0x01, 0x11, 0x00]); // component descriptors
        data.extend_from_slice(&[0xFF, 0xD9]); // EOI
        let tmp = make_temp_image("jpg").unwrap();
        std::fs::write(&tmp, &data).unwrap();
        assert_eq!(image_dimensions(&tmp), Some((1, 2)));
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn image_dimensions_unknown_format_returns_none() {
        let tmp = make_temp_image("png").unwrap();
        std::fs::write(&tmp, b"definitely not an image").unwrap();
        assert_eq!(image_dimensions(&tmp), None);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn image_source_from_file_sniffs_real_media_type() {
        // A .png-named file holding JPEG bytes must be reported as JPEG.
        let tmp = make_temp_image("png").unwrap();
        std::fs::write(&tmp, [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10]).unwrap();
        let src = image_source_from_file(&tmp).unwrap();
        assert_eq!(src.media_type.as_deref(), Some("image/jpeg"));
        assert_eq!(src.source_type, "base64");
        assert!(src.data.is_some());
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn image_source_from_file_missing_returns_none() {
        assert!(image_source_from_file(Path::new("/nonexistent/x.png")).is_none());
    }

    #[test]
    fn encode_image_base64_roundtrip() {
        let tmp = make_temp_image("png").unwrap();
        std::fs::write(&tmp, b"hello world").unwrap();
        let b64 = encode_image_base64(&tmp).unwrap();
        let decoded =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &b64).unwrap();
        assert_eq!(decoded, b"hello world");
        let _ = std::fs::remove_file(&tmp);
    }
}
