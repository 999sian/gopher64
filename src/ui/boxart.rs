// boxart.rs — auto-download N64 box art / media from the libretro thumbnails server.
//
// libretro thumbnail filenames ARE the No-Intro game names with a fixed 1:1
// character escaping, so there is no matching to do: the caller resolves the
// No-Intro name (via `ui::gui::get_nointro_name` — ROM hash -> name, header-name
// fallback) and hands it here; we escape it the way libretro does and request
// "<name>.png" directly. No DAT, no CRC, no filename index, no fuzzy guessing.
//
// Source:
//   https://thumbnails.libretro.com/Nintendo - Nintendo 64/Named_{Boxarts,Snaps,Titles}/
// No API key or account required.

use crate::ui;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

const THUMB_BASE: &str = "https://thumbnails.libretro.com/";
const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

// rom path -> downloaded png path (the disk is the cache; these just avoid
// re-checking disk on the UI thread).
static ART_CACHE: LazyLock<Mutex<HashMap<String, PathBuf>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static SNAP_CACHE: LazyLock<Mutex<HashMap<String, PathBuf>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static TITLE_CACHE: LazyLock<Mutex<HashMap<String, PathBuf>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
// rom path -> resolved No-Intro name (set when art resolves; reused for snap/title).
static NAME_CACHE: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
// rom path -> unofficial? (its hash wasn't in the No-Intro DB).
static HOMEBREW: LazyLock<Mutex<HashMap<String, bool>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn boxart_dir() -> PathBuf {
    let dir = ui::get_dirs().cache_dir.join("boxart");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn media_dir(sub: &str) -> PathBuf {
    let dir = boxart_dir().join(sub);
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// The already-downloaded box art for a ROM, if any. Network-free (UI thread).
pub fn art_path(rom_path: &str) -> Option<PathBuf> {
    ART_CACHE.lock().get(rom_path).cloned()
}

/// The already-downloaded gameplay snap for a ROM, if any. Network-free.
pub fn snap_path(rom_path: &str) -> Option<PathBuf> {
    SNAP_CACHE.lock().get(rom_path).cloned()
}

/// The already-downloaded title screen for a ROM, if any. Network-free.
pub fn title_path(rom_path: &str) -> Option<PathBuf> {
    TITLE_CACHE.lock().get(rom_path).cloned()
}

/// Whether a ROM is unofficial (its hash wasn't in the No-Intro DB). Network-free.
pub fn is_homebrew(rom_path: &str) -> bool {
    HOMEBREW.lock().get(rom_path).copied().unwrap_or(false)
}

/// Record whether a ROM is unofficial (the caller computes this from the No-Intro map).
pub fn set_homebrew(rom_path: &str, homebrew: bool) {
    HOMEBREW.lock().insert(rom_path.to_string(), homebrew);
}

// Reject remote-derived names that could escape the cache dir via a path separator.
fn is_safe_name(name: &str) -> bool {
    !name.is_empty() && !name.contains('/') && !name.contains('\\')
}

/// The libretro thumbnail filename for a No-Intro game name. libretro replaces a
/// fixed set of filesystem/URL-unsafe characters with '_'; this is a deterministic
/// 1:1 transform, NOT a fuzzy match. Returns "<escaped name>.png".
pub fn escaped_filename(name: &str) -> String {
    let escaped: String = name
        .chars()
        .map(|c| {
            if matches!(
                c,
                '&' | '*' | '/' | ':' | '`' | '<' | '>' | '?' | '\\' | '|' | '"'
            ) {
                '_'
            } else {
                c
            }
        })
        .collect();
    format!("{escaped}.png")
}

async fn download(category: &str, filename: &str, dest: &Path) -> bool {
    let Ok(mut url) = reqwest::Url::parse(THUMB_BASE) else {
        return false;
    };
    if let Ok(mut segs) = url.path_segments_mut() {
        segs.pop_if_empty()
            .extend(["Nintendo - Nintendo 64", category, filename]);
    } else {
        return false;
    }
    match ui::WEB_CLIENT.get(url).timeout(HTTP_TIMEOUT).send().await {
        Ok(resp) if resp.status().is_success() => match resp.bytes().await {
            // Atomic: temp sibling then rename, so a crash mid-write can't leave a
            // truncated PNG that try_exists() would later serve as valid.
            Ok(bytes) => {
                let tmp = dest.with_extension("part");
                tokio::fs::write(&tmp, &bytes).await.is_ok()
                    && tokio::fs::rename(&tmp, dest).await.is_ok()
            }
            Err(_) => false,
        },
        _ => false,
    }
}

/// Ensure box art for `name` (a No-Intro game name) is downloaded + registered for
/// `rom_path`. Returns true when art is available. Network-free when cached on disk.
pub async fn resolve_and_cache(rom_path: &str, name: &str) -> bool {
    NAME_CACHE
        .lock()
        .insert(rom_path.to_string(), name.to_string());
    if ART_CACHE.lock().contains_key(rom_path) {
        return true;
    }
    let filename = escaped_filename(name);
    if !is_safe_name(&filename) {
        return false;
    }
    let dest = boxart_dir().join(&filename);
    if tokio::fs::try_exists(&dest).await.unwrap_or(false)
        || download("Named_Boxarts", &filename, &dest).await
    {
        ART_CACHE.lock().insert(rom_path.to_string(), dest);
        true
    } else {
        false
    }
}

/// Best-effort download of a ROM's gameplay snap + title screen, using the No-Intro
/// name recorded by `resolve_and_cache`. No-op until art has resolved for `rom_path`.
pub async fn resolve_media(rom_path: &str) {
    let Some(name) = NAME_CACHE.lock().get(rom_path).cloned() else {
        return;
    };
    let filename = escaped_filename(&name);
    if !is_safe_name(&filename) {
        return;
    }
    for (cache, sub, category) in [
        (&SNAP_CACHE, "snap", "Named_Snaps"),
        (&TITLE_CACHE, "title", "Named_Titles"),
    ] {
        if cache.lock().contains_key(rom_path) {
            continue;
        }
        let dest = media_dir(sub).join(&filename);
        if tokio::fs::try_exists(&dest).await.unwrap_or(false)
            || download(category, &filename, &dest).await
        {
            cache.lock().insert(rom_path.to_string(), dest);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unsafe_names() {
        assert!(is_safe_name("Super Mario 64 (USA).png"));
        assert!(!is_safe_name(""));
        assert!(!is_safe_name("../escape.png"));
        assert!(!is_safe_name("sub/dir.png"));
        assert!(!is_safe_name("sub\\dir.png"));
    }

    #[test]
    fn escaping_is_exact_and_safe() {
        assert_eq!(
            escaped_filename("Super Mario 64 (USA)"),
            "Super Mario 64 (USA).png"
        );
        assert_eq!(
            escaped_filename("Legend of Zelda, The - Ocarina of Time (USA)"),
            "Legend of Zelda, The - Ocarina of Time (USA).png"
        );
        assert_eq!(
            escaped_filename(r#"A&B*C/D:E`F<G>H?I\J|K"L"#),
            "A_B_C_D_E_F_G_H_I_J_K_L.png"
        );
        assert!(is_safe_name(&escaped_filename("Foo/Bar: Baz")));
    }
}
