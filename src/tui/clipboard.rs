//! Best-effort system-clipboard write for copied terminal selections.
//!
//! Tries the platform clipboard command first (`pbcopy` on macOS; `clip` on
//! Windows; `wl-copy`/`xclip`/`xsel` on Linux), falling back to an OSC 52 escape
//! sequence written to the controlling terminal. The fallback works over SSH
//! and inside multiplexers that pass OSC 52 through, but is only reached when no
//! clipboard command is available, so it rarely perturbs the alternate screen.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::contracts::FileSystem;

/// Copy `text` to the system clipboard (best effort; failures are silent).
pub fn copy(text: &str) {
    if text.is_empty() {
        return;
    }
    if try_command_clipboard(text) {
        return;
    }
    let _ = write_osc52(text);
}

/// What the TUI's paste key found on the system clipboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardPaste {
    /// An image, saved to this file in [`image_paste_dir`].
    Image(PathBuf),
    /// Text, to paste as a terminal would.
    Text(String),
}

/// Read the system clipboard for the TUI's paste key: an image, saved to a
/// temp file, and on Windows otherwise its text.
///
/// An image is how FlightDeck delivers a pasted picture to a hosted agent: the
/// agent runs in a PTY and cannot see the host clipboard's image flavour, but
/// it does understand a file-path reference (the same thing a terminal inserts
/// when you drag an image in). On Windows text is read too, because there the
/// paste key is the terminal's own paste key: a user frees Ctrl+V from Windows
/// Terminal so an image can reach FlightDeck at all, and then FlightDeck has to
/// paste text itself (Alt+V likewise). macOS and most Linux terminals paste
/// text with another chord, so there Ctrl+V with no image forwards the literal
/// key (readline's quoted-insert, vim's block select).
///
/// Best effort: any failure (empty clipboard, missing tool) yields `None` so
/// the caller can fall back to forwarding the key.
pub fn read_clipboard_for_paste() -> Option<ClipboardPaste> {
    // Containerized agents mount this dedicated directory read-only. Creating
    // it here also keeps local image paste self-contained when no container is
    // involved. Text paste does not need it, so a failure is not fatal.
    let paste_dir_ready = std::fs::create_dir_all(image_paste_dir()).is_ok();

    #[cfg(target_os = "windows")]
    {
        read_clipboard_windows(paste_dir_ready)
    }
    #[cfg(not(target_os = "windows"))]
    {
        paste_dir_ready
            .then(save_clipboard_image)
            .flatten()
            .map(ClipboardPaste::Image)
    }
}

/// If the system clipboard holds an image, write it to a temp file and return
/// its path; otherwise return `None`. Windows reads image and text in one
/// PowerShell run instead ([`read_clipboard_windows`]).
#[cfg(not(target_os = "windows"))]
fn save_clipboard_image() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        save_clipboard_image_macos()
    }
    #[cfg(not(target_os = "macos"))]
    {
        save_clipboard_image_linux()
    }
}

/// Directory on the host used exclusively for temporary pasted-image files.
///
/// Keeping these files in a dedicated directory (rather than directly under
/// the system temp directory) lets containerized agents read a pasted image
/// without granting access to unrelated host temporary files.
pub fn image_paste_dir() -> PathBuf {
    std::env::temp_dir().join("flightdeck-paste")
}

/// Map a pasted-image path from `host_dir` to its corresponding path under a
/// container bind mount. Returns `None` when the path is not inside the shared
/// host directory.
pub fn container_image_path(path: &Path, host_dir: &Path, container_dir: &Path) -> Option<PathBuf> {
    path.strip_prefix(host_dir)
        .ok()
        .map(|relative| container_dir.join(relative))
}

/// An image a front-end read off its own clipboard, to paste into a terminal
/// as a file path, as [`save_clipboard_image`] does for the TUI. The desktop
/// app reads its clipboard through GPUI rather than a platform command, and a
/// remote window sends the bytes to the host that runs the agent, because the
/// agent cannot read a clipboard on another machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PastedImage {
    /// The image format, as a file extension (`png`, `jpg`, …).
    pub format: String,
    /// The encoded image.
    pub bytes: Vec<u8>,
}

/// The largest pasted image saved, in bytes. Well above a Retina screenshot,
/// and small enough that one travels in a single web-protocol frame (axum's
/// 16 MiB frame limit, after base64).
pub const MAX_PASTED_IMAGE_BYTES: usize = 10 * 1024 * 1024;

/// The extension a pasted image is saved under: its `format` when that is a
/// raster format agents read, else `None`. An allowlist, because a remote
/// viewer chooses `format` and it becomes part of a file name.
pub fn pasted_image_extension(format: &str) -> Option<&'static str> {
    match format.to_ascii_lowercase().as_str() {
        "png" => Some("png"),
        "jpg" | "jpeg" => Some("jpg"),
        "gif" => Some("gif"),
        "webp" => Some("webp"),
        "bmp" => Some("bmp"),
        "tif" | "tiff" => Some("tiff"),
        _ => None,
    }
}

/// Save `image` in [`image_paste_dir`] and return its path. Refused as
/// [`check_pasted_image`] refuses, or when the save fails.
pub fn save_pasted_image(fs: &dyn FileSystem, image: &PastedImage) -> Result<PathBuf, String> {
    let ext = check_pasted_image(image)?;
    let path = unique_image_path(ext);
    fs.create_dir_all(&image_paste_dir())
        .and_then(|()| fs.write_bytes(&path, &image.bytes))
        .map_err(|e| format!("The pasted image could not be saved: {e}"))?;
    Ok(path)
}

/// The extension `image` is saved under, or the reason a user reads why it
/// cannot be pasted: it is empty, larger than [`MAX_PASTED_IMAGE_BYTES`] or
/// not an image format agents read. A remote window checks before it sends.
pub fn check_pasted_image(image: &PastedImage) -> Result<&'static str, String> {
    let Some(ext) = pasted_image_extension(&image.format) else {
        return Err(format!(
            "A pasted {} image cannot be attached; copy it as PNG or JPEG.",
            image.format
        ));
    };
    if image.bytes.is_empty() {
        return Err("The pasted image is empty.".to_string());
    }
    if image.bytes.len() > MAX_PASTED_IMAGE_BYTES {
        return Err(format!(
            "The pasted image is {} MB; images up to {} MB can be pasted.",
            image.bytes.len().div_ceil(1024 * 1024),
            MAX_PASTED_IMAGE_BYTES / (1024 * 1024)
        ));
    }
    Ok(ext)
}

/// What to type into an agent for an image saved at `path`: the path as the
/// agent sees it (inside a container, the paste directory's mount), quoted
/// when it could be word-split, and a trailing space so typing can go on.
/// What a terminal inserts when an image is dragged in, which agents such as
/// Claude Code recognise and attach.
pub fn pasted_image_text(path: &Path, containerized: bool) -> String {
    let path = if containerized {
        container_image_path(
            path,
            &image_paste_dir(),
            Path::new(crate::runtime::container::IMAGE_PASTE_DIR),
        )
        .unwrap_or_else(|| path.to_path_buf())
    } else {
        path.to_path_buf()
    };
    let raw = path.to_string_lossy();
    let mut text = if raw.contains(char::is_whitespace) {
        format!("'{}'", raw.replace('\'', "'\\''"))
    } else {
        raw.into_owned()
    };
    text.push(' ');
    text
}

/// A unique path under the system temp dir for a pasted image, namespaced by pid
/// and a per-process counter so concurrent pastes never collide.
fn unique_image_path(ext: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    image_paste_dir().join(format!("flightdeck-paste-{pid}-{n}.{ext}"))
}

/// `true` if `path` now refers to a non-empty file.
fn wrote_non_empty(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.len() > 0)
        .unwrap_or(false)
}

/// macOS: read the clipboard image via `osascript`. Screenshots and most copied
/// images expose a `PNGf` flavour; we prefer it and fall back to `TIFF`.
#[cfg(target_os = "macos")]
fn save_clipboard_image_macos() -> Option<PathBuf> {
    // Inspect the available flavours first so we only spend the write attempt
    // when an image is actually present.
    let info = Command::new("osascript")
        .args(["-e", "clipboard info"])
        .output()
        .ok()?;
    let info = String::from_utf8_lossy(&info.stdout);
    let has_png = info.contains("PNGf");
    let has_tiff = info.contains("TIFF");
    if !has_png && !has_tiff {
        return None;
    }

    // Prefer PNG; coercion to PNGf succeeds for most image flavours on macOS.
    if has_png || has_tiff {
        let path = unique_image_path("png");
        if osascript_write_clipboard(&path, "«class PNGf»") && wrote_non_empty(&path) {
            return Some(path);
        }
        let _ = std::fs::remove_file(&path);
    }
    // Fall back to a raw TIFF dump when PNG coercion failed.
    if has_tiff {
        let path = unique_image_path("tiff");
        if osascript_write_clipboard(&path, "«class TIFF»") && wrote_non_empty(&path) {
            return Some(path);
        }
        let _ = std::fs::remove_file(&path);
    }
    None
}

/// Run an AppleScript that writes the clipboard, coerced to `class`, to `path`.
/// Returns whether the script exited successfully.
#[cfg(target_os = "macos")]
fn osascript_write_clipboard(path: &Path, class: &str) -> bool {
    // AppleScript string literals escape `\` and `"`; temp paths contain neither
    // in practice, but guard anyway.
    let p = path
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    let script = format!(
        "set p to POSIX file \"{p}\"\n\
         set fh to open for access p with write permission\n\
         try\n\
         \tset eof fh to 0\n\
         \twrite (the clipboard as {class}) to fh\n\
         \tclose access fh\n\
         on error errMsg number errNum\n\
         \tclose access fh\n\
         \terror errMsg number errNum\n\
         end try"
    );
    Command::new("osascript")
        .arg("-e")
        .arg(script)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Windows: one PowerShell run reads the clipboard. An image is saved as a PNG
/// and reported as `image`; otherwise its text is reported as `text:` and the
/// UTF-8 bytes in base64, because PowerShell writes stdout in the console code
/// page, which would mangle non-ASCII text. One run rather than two keeps the
/// paste to a single PowerShell start-up. An empty clipboard exits non-zero.
#[cfg(target_os = "windows")]
fn read_clipboard_windows(paste_dir_ready: bool) -> Option<ClipboardPaste> {
    let path = unique_image_path("png");
    // PowerShell single-quoted literals only need `'` doubled; temp paths never
    // contain quotes in practice, but guard anyway.
    let p = path.to_string_lossy().replace('\'', "''");
    let check_image = if paste_dir_ready { "$true" } else { "$false" };
    let script = format!(
        "Add-Type -AssemblyName System.Windows.Forms, System.Drawing; \
         $img = $null; \
         if ({check_image}) {{ $img = [System.Windows.Forms.Clipboard]::GetImage() }}; \
         if ($img -ne $null) {{ \
           try {{ $img.Save('{p}', [System.Drawing.Imaging.ImageFormat]::Png); 'image'; exit 0 }} \
           catch {{ }} }}; \
         $t = [System.Windows.Forms.Clipboard]::GetText(); \
         if ([string]::IsNullOrEmpty($t)) {{ exit 1 }}; \
         'text:' + [Convert]::ToBase64String([System.Text.Encoding]::UTF8.GetBytes($t)); exit 0"
    );
    let output = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let paste = match output {
        Ok(out) if out.status.success() => {
            parse_windows_clipboard_output(&String::from_utf8_lossy(&out.stdout), &path)
        }
        _ => None,
    };
    if !matches!(paste, Some(ClipboardPaste::Image(_))) {
        let _ = std::fs::remove_file(&path);
    }
    paste
}

/// Turn [`read_clipboard_windows`]'s script output into a paste: `image` names
/// the PNG saved at `image_path` (refused if the file is missing or empty),
/// `text:<base64>` carries the clipboard text. Anything else is `None`.
#[cfg(any(target_os = "windows", test))]
fn parse_windows_clipboard_output(stdout: &str, image_path: &Path) -> Option<ClipboardPaste> {
    use base64::Engine;

    let line = stdout.trim();
    if line == "image" {
        return wrote_non_empty(image_path)
            .then(|| ClipboardPaste::Image(image_path.to_path_buf()));
    }
    let b64 = line.strip_prefix("text:")?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    (!text.is_empty()).then_some(ClipboardPaste::Text(text))
}

/// Linux: try Wayland (`wl-paste`) then X11 (`xclip`) to pull a PNG off the
/// clipboard.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn save_clipboard_image_linux() -> Option<PathBuf> {
    use std::fs::File;

    // (command, args producing PNG bytes on stdout)
    let candidates: &[(&str, &[&str])] = &[
        ("wl-paste", &["--type", "image/png"]),
        (
            "xclip",
            &["-selection", "clipboard", "-t", "image/png", "-o"],
        ),
    ];
    for (cmd, args) in candidates {
        let path = unique_image_path("png");
        let Ok(file) = File::create(&path) else {
            continue;
        };
        let spawned = Command::new(cmd)
            .args(*args)
            .stdout(Stdio::from(file))
            .stderr(Stdio::null())
            .status();
        if matches!(spawned, Ok(s) if s.success()) && wrote_non_empty(&path) {
            return Some(path);
        }
        let _ = std::fs::remove_file(&path);
    }
    None
}

/// Pipe `text` into the first available platform clipboard command. On
/// Windows this returns PowerShell's actual success/failure so the OSC 52
/// fallback still runs when `Set-Clipboard` fails; elsewhere it returns
/// `true` once a command was spawned (regardless of whether it ultimately
/// succeeded), so the OSC 52 fallback is skipped when a native tool exists.
fn try_command_clipboard(text: &str) -> bool {
    #[cfg(target_os = "windows")]
    {
        try_windows_clipboard(text)
    }

    #[cfg(not(target_os = "windows"))]
    {
        let candidates: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
            &[("pbcopy", &[])]
        } else {
            &[
                ("wl-copy", &[]),
                ("xclip", &["-selection", "clipboard"]),
                ("xsel", &["-ib"]),
            ]
        };

        for (cmd, args) in candidates {
            let spawned = Command::new(cmd)
                .args(*args)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            if let Ok(mut child) = spawned {
                if let Some(mut stdin) = child.stdin.take() {
                    let _ = stdin.write_all(text.as_bytes());
                    // `stdin` drops here, closing the pipe so the command sees EOF.
                }
                let _ = child.wait();
                return true;
            }
        }
        false
    }
}

/// Windows: set the clipboard via PowerShell's `Set-Clipboard` instead of
/// piping into `clip`. `clip` decodes piped stdin using the process's
/// OEM/ANSI code page rather than UTF-8, so any non-ASCII text gets mangled
/// on the way in; base64-encoding the UTF-8 bytes and decoding them inside
/// PowerShell avoids that entirely (and sidesteps `-Command` quoting issues,
/// since the base64 alphabet never contains a `'`).
#[cfg(target_os = "windows")]
fn try_windows_clipboard(text: &str) -> bool {
    let b64 = base64_encode(text.as_bytes());
    let script = format!(
        "Set-Clipboard -Value ([System.Text.Encoding]::UTF8.GetString([System.Convert]::FromBase64String('{b64}')))"
    );
    Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Write an OSC 52 clipboard-set sequence to stdout.
fn write_osc52(text: &str) -> std::io::Result<()> {
    let seq = format!("\x1b]52;c;{}\x07", base64_encode(text.as_bytes()));
    let mut out = std::io::stdout();
    out.write_all(seq.as_bytes())?;
    out.flush()
}

/// Minimal standard-alphabet base64 encoder (no padding-free shortcuts, no deps).
fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((n >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{
        base64_encode, container_image_path, image_paste_dir, parse_windows_clipboard_output,
        pasted_image_extension, pasted_image_text, save_pasted_image, ClipboardPaste, PastedImage,
        MAX_PASTED_IMAGE_BYTES,
    };
    use crate::testing::FakeFs;
    use std::path::Path;

    fn png(bytes: Vec<u8>) -> PastedImage {
        PastedImage {
            format: "png".to_string(),
            bytes,
        }
    }

    #[test]
    fn windows_clipboard_text_survives_non_ascii() {
        let text = "héllo → 世界\r\nline two";
        let out = format!("text:{}\r\n", base64_encode(text.as_bytes()));
        assert_eq!(
            parse_windows_clipboard_output(&out, Path::new("unused.png")),
            Some(ClipboardPaste::Text(text.to_string()))
        );
    }

    #[test]
    fn windows_clipboard_image_needs_a_written_file() {
        let dir = std::env::temp_dir().join(format!("fd-clip-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let written = dir.join("written.png");
        std::fs::write(&written, [1u8, 2, 3]).unwrap();
        let missing = dir.join("missing.png");

        assert_eq!(
            parse_windows_clipboard_output("image\r\n", &written),
            Some(ClipboardPaste::Image(written.clone()))
        );
        assert_eq!(parse_windows_clipboard_output("image", &missing), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn windows_clipboard_rejects_empty_or_garbled_output() {
        let p = Path::new("unused.png");
        assert_eq!(parse_windows_clipboard_output("", p), None);
        assert_eq!(parse_windows_clipboard_output("text:", p), None);
        assert_eq!(parse_windows_clipboard_output("text:not base64!", p), None);
        assert_eq!(parse_windows_clipboard_output("something else", p), None);
    }

    #[test]
    fn a_pasted_image_is_saved_in_the_paste_directory() {
        let fs = FakeFs::new();
        let path = save_pasted_image(&fs, &png(vec![1, 2, 3])).unwrap();
        assert!(path.starts_with(image_paste_dir()));
        assert_eq!(path.extension().unwrap(), "png");
        assert_eq!(fs.file_bytes(&path), Some(vec![1, 2, 3]));
        let other = save_pasted_image(&fs, &png(vec![4])).unwrap();
        assert_ne!(path, other, "every paste gets its own file");
    }

    #[test]
    fn only_image_formats_within_the_size_limit_are_saved() {
        let fs = FakeFs::new();
        let refused = |image: PastedImage| save_pasted_image(&fs, &image).unwrap_err();
        assert!(refused(PastedImage {
            format: "../../etc/passwd".to_string(),
            bytes: vec![1],
        })
        .contains("cannot be attached"));
        assert!(refused(png(Vec::new())).contains("empty"));
        assert!(refused(png(vec![0; MAX_PASTED_IMAGE_BYTES + 1])).contains("up to 10 MB"));
        assert!(fs.writes().is_empty(), "a refused image writes nothing");
        assert_eq!(pasted_image_extension("JPEG"), Some("jpg"));
        assert_eq!(pasted_image_extension("svg"), None);
    }

    #[test]
    fn a_pasted_image_types_its_path_quoted_when_needed() {
        let dir = image_paste_dir();
        let plain = dir.join("flightdeck-paste-1-0.png");
        assert_eq!(
            pasted_image_text(&plain, false),
            format!("{} ", plain.display())
        );
        let spaced = Path::new("/tmp/my shot's.png");
        assert_eq!(
            pasted_image_text(spaced, false),
            "'/tmp/my shot'\\''s.png' "
        );
        // In a container, the paste directory's mount.
        let mapped = pasted_image_text(&plain, true);
        assert!(
            mapped.starts_with(crate::runtime::container::IMAGE_PASTE_DIR),
            "{mapped}"
        );
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn maps_pasted_image_to_its_container_mount() {
        assert_eq!(
            container_image_path(
                Path::new("/tmp/flightdeck-paste/flightdeck-paste-42-0.png"),
                Path::new("/tmp/flightdeck-paste"),
                Path::new("/tmp/flightdeck-paste"),
            ),
            Some(Path::new("/tmp/flightdeck-paste/flightdeck-paste-42-0.png").to_path_buf())
        );
        assert_eq!(
            container_image_path(
                Path::new("/tmp/unrelated.png"),
                Path::new("/tmp/flightdeck-paste"),
                Path::new("/tmp/flightdeck-paste"),
            ),
            None
        );
    }
}
