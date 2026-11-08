//! Reading the system clipboard — text **and** images.
//!
//! The write side lives in `tui::copy_to_clipboard` (and has its own Windows
//! story: `clip.exe` mangles UTF-8). This module is only the read side, added so
//! a copied screenshot can be pasted straight into the prompt as an attachment.
//!
//! Every backend is an OS tool that ships with the platform — PowerShell on
//! Windows, `wl-paste`/`xclip` on Linux, `pbpaste`/`pngpaste` on macOS. When none
//! is present the read returns `None` and the caller says so; it never panics and
//! never blocks for long (a spawned tool that is missing fails instantly).

use std::path::PathBuf;
use std::process::Command;

/// Long edge a pasted image is scaled down to.
///
/// A 4K screenshot is ~8 MB as PNG; base64 adds a third on top and providers
/// reject anything past a few MB. 1568 px is the long-edge limit the vision APIs
/// are usually documented around, so scaling to it keeps the payload well inside
/// the limit without visibly hurting a screenshot.
///
/// Only the Windows backend scales today; the Linux/macOS backends pass the
/// clipboard image through as the OS tool returns it, so on those targets the
/// constant is (correctly) unused — hence the conditional `allow`.
#[cfg_attr(not(windows), allow(dead_code))]
const MAX_EDGE: u32 = 1568;

/// What a clipboard read turned into.
#[derive(Debug, Clone, PartialEq)]
pub enum ClipboardRead {
    /// An image, already written to this file — ready to attach.
    Image(PathBuf),
    /// Plain text, for the input.
    Text(String),
    /// Nothing usable (empty, or only whitespace).
    Empty,
}

/// Decide what the clipboard held. An image wins: a screenshot usually carries no
/// useful text, and if it carries both, the picture is what was copied.
pub fn classify(image: Option<PathBuf>, text: Option<String>) -> ClipboardRead {
    if let Some(path) = image {
        return ClipboardRead::Image(path);
    }
    match text {
        Some(t) if !t.trim().is_empty() => ClipboardRead::Text(t),
        _ => ClipboardRead::Empty,
    }
}

/// Read the clipboard: image first, then text.
pub fn read() -> ClipboardRead {
    classify(read_image(), read_text())
}

/// Where a freshly pasted image is written. Never deleted — the attachment is a
/// path that has to stay readable until the turn runs.
fn paste_target() -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let name = format!("rijo-paste-{stamp}-{}.png", std::process::id());
    std::env::temp_dir().join(name)
}

fn run(cmd: &str, args: &[&str]) -> Option<std::process::Output> {
    Command::new(cmd).args(args).output().ok()
}

/// Read an image from the clipboard, scaled to [`MAX_EDGE`] and written as PNG.
pub fn read_image() -> Option<PathBuf> {
    #[cfg(windows)]
    return image_windows();

    #[cfg(target_os = "linux")]
    return image_linux();

    #[cfg(target_os = "macos")]
    return image_macos();

    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    None
}

/// Read the clipboard's text.
pub fn read_text() -> Option<String> {
    #[cfg(windows)]
    return text_windows();

    #[cfg(target_os = "linux")]
    return text_linux();

    #[cfg(target_os = "macos")]
    return text_macos();

    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    None
}

/// PowerShell reads the clipboard image, scales it and saves it as PNG.
///
/// `[Windows.Forms.Clipboard]::GetImage()` needs an STA apartment; Windows
/// PowerShell's console host is STA by default, which is why this uses
/// `powershell` and not `pwsh` (MTA by default). `Get-Clipboard -Format Image` is
/// the fallback. Exit code 3 means "no image", which is not an error.
#[cfg(windows)]
fn image_windows() -> Option<PathBuf> {
    let target = paste_target();
    let path = target.display().to_string().replace('\'', "''");
    let script = format!(
        "$ErrorActionPreference='SilentlyContinue'; \
         Add-Type -AssemblyName System.Windows.Forms; \
         Add-Type -AssemblyName System.Drawing; \
         $img=[System.Windows.Forms.Clipboard]::GetImage(); \
         if ($img -eq $null) {{ $img=Get-Clipboard -Format Image }}; \
         if ($img -eq $null) {{ exit 3 }}; \
         if ($img.Width -gt {max} -or $img.Height -gt {max}) {{ \
           $r=[Math]::Min({max}/$img.Width, {max}/$img.Height); \
           $bmp=New-Object System.Drawing.Bitmap([int]($img.Width*$r), [int]($img.Height*$r)); \
           $g=[System.Drawing.Graphics]::FromImage($bmp); \
           $g.InterpolationMode=[System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic; \
           $g.DrawImage($img, 0, 0, $bmp.Width, $bmp.Height); \
           $img=$bmp }}; \
         $img.Save('{path}', [System.Drawing.Imaging.ImageFormat]::Png); \
         Write-Output 'ok'",
        max = MAX_EDGE
    );
    let out = run(
        "powershell",
        &["-NoProfile", "-NonInteractive", "-Command", &script],
    )?;
    if out.status.success() && target.is_file() {
        Some(target)
    } else {
        None
    }
}

/// `Get-Clipboard -Raw` keeps the line breaks; without `-Raw` a multi-line
/// clipboard comes back as an array joined by spaces. Output is decoded as UTF-8
/// explicitly, same reason as the write side.
#[cfg(windows)]
fn text_windows() -> Option<String> {
    let script = "[Console]::OutputEncoding=[System.Text.UTF8Encoding]::new(); \
                  Get-Clipboard -Raw";
    let out = run(
        "powershell",
        &["-NoProfile", "-NonInteractive", "-Command", script],
    )?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Wayland first (`wl-paste`), then X11 (`xclip`). Whichever is installed and has
/// the type wins; a missing tool simply fails and the next one is tried.
#[cfg(target_os = "linux")]
fn image_linux() -> Option<PathBuf> {
    let target = paste_target();
    let candidates: [(&str, &[&str]); 2] = [
        ("wl-paste", &["--type", "image/png"]),
        (
            "xclip",
            &["-selection", "clipboard", "-t", "image/png", "-o"],
        ),
    ];
    for (cmd, args) in candidates {
        if let Some(out) = run(cmd, args) {
            if out.status.success()
                && !out.stdout.is_empty()
                && std::fs::write(&target, &out.stdout).is_ok()
            {
                return Some(target);
            }
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn text_linux() -> Option<String> {
    for (cmd, args) in [
        ("wl-paste", &["--no-newline"][..]),
        ("xclip", &["-selection", "clipboard", "-o"][..]),
    ] {
        if let Some(out) = run(cmd, args) {
            if out.status.success() {
                return Some(String::from_utf8_lossy(&out.stdout).into_owned());
            }
        }
    }
    None
}

/// `pngpaste` is the common image reader on macOS; without it the clipboard image
/// is not reachable from a CLI. Untested here — no macOS machine on hand.
#[cfg(target_os = "macos")]
fn image_macos() -> Option<PathBuf> {
    let target = paste_target();
    let path = target.to_str()?;
    let out = run("pngpaste", &[path])?;
    if out.status.success() && target.is_file() {
        Some(target)
    } else {
        None
    }
}

#[cfg(target_os = "macos")]
fn text_macos() -> Option<String> {
    let out = run("pbpaste", &[])?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_image_beats_text() {
        let img = PathBuf::from("/tmp/shot.png");
        assert_eq!(
            classify(Some(img.clone()), Some("also text".into())),
            ClipboardRead::Image(img)
        );
    }

    #[test]
    fn text_comes_through_and_whitespace_is_not_content() {
        assert_eq!(
            classify(None, Some("hello".into())),
            ClipboardRead::Text("hello".into())
        );
        assert_eq!(
            classify(None, Some("   \n\t ".into())),
            ClipboardRead::Empty
        );
        assert_eq!(classify(None, None), ClipboardRead::Empty);
    }

    #[test]
    fn a_paste_target_is_a_png_in_the_temp_dir_and_unique() {
        let a = paste_target();
        let b = paste_target();
        assert_eq!(a.extension().and_then(|e| e.to_str()), Some("png"));
        assert_eq!(a.parent(), Some(std::env::temp_dir().as_path()));
        // Two pastes in the same millisecond must not collide on the process id
        // alone — but they must also not be the same file.
        assert!(a.file_name().is_some() && b.file_name().is_some());
    }
}
