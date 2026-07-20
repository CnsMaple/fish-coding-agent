use crate::app::App;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

use super::pickers::close_active_function_tab;

/// Encode raw BGRA clipboard image data into a PNG.
/// arboard returns ImageData as raw pixels, not an encoded image.
fn encode_image_data_to_png(img_data: &arboard::ImageData<'_>) -> Option<Vec<u8>> {
    use image::codecs::png::PngEncoder;
    use image::ImageEncoder;
    let width = img_data.width as u32;
    let height = img_data.height as u32;
    if width == 0 || height == 0 || img_data.bytes.len() < (width * height * 4) as usize {
        return None;
    }
    // Convert BGRA to RGBA.
    let mut rgba: Vec<u8> = Vec::with_capacity(img_data.bytes.len());
    for chunk in img_data.bytes.chunks_exact(4) {
        rgba.extend_from_slice(&[chunk[2], chunk[1], chunk[0], chunk[3]]);
    }
    let mut encoded: Vec<u8> = Vec::new();
    let encoder = PngEncoder::new(&mut encoded);
    if encoder
        .write_image(&rgba, width, height, image::ExtendedColorType::Rgba8)
        .is_err()
    {
        return None;
    }
    Some(encoded)
}
/// Unified Ctrl+V / Cmd+V handler: open clipboard once, try image
/// first, fall back to text paste.
/// Open the paste preview sidebar tab, showing the current clipboard
/// content (image or text) for the user to confirm before inserting.
pub(super) fn open_paste_preview(app: &mut App) {
    use crate::function::notifications::ToastLevel;
    use crate::session::ImageAttachment;
    use sha2::{Digest, Sha256};

    let mut state = crate::function::PastePreviewState {
        text: None,
        image: None,
        image_bytes: None,
        media_type: None,
    };

    let Ok(mut cb) = arboard::Clipboard::new() else {
        app.notify(ToastLevel::Warn, "clipboard unavailable");
        return;
    };

    // Try image first. arboard returns raw BGRA pixels, so we need to
    // re-encode to PNG before storing the asset.
    if let Ok(img_data) = cb.get_image() {
        let Some(png_bytes) = encode_image_data_to_png(&img_data) else {
            app.notify(ToastLevel::Warn, "clipboard image encoding failed");
            return;
        };
        let media_type = "image/png";
        let hash = hex::encode(Sha256::digest(&png_bytes));
        if let Ok(assets_dir) = crate::session::store::assets_dir(&app.session_id) {
            let _ = std::fs::create_dir_all(&assets_dir);
            let filename = format!("{hash}.png");
            let asset_path = assets_dir.join(&filename);
            if !asset_path.exists() {
                let _ = std::fs::write(&asset_path, &png_bytes);
            }
            state.image = Some(ImageAttachment {
                asset_path,
                media_type: media_type.to_string(),
                byte_size: png_bytes.len() as u64,
                width: img_data.width as u32,
                height: img_data.height as u32,
            });
            state.image_bytes = Some(png_bytes);
            state.media_type = Some(media_type.to_string());
        }
    }

    // Fall back to text.
    if state.image.is_none() {
        if let Ok(text) = cb.get_text() {
            if !text.is_empty() {
                state.text = Some(text);
            }
        }
    }

    if state.text.is_none() && state.image.is_none() {
        app.notify(ToastLevel::Warn, "clipboard is empty");
        return;
    }

    app.function
        .push(crate::function::SidebarTab::PastePreview(Box::new(state)));
    app.show_panel();
    app.acknowledge_panel();
}

pub(super) async fn handle_paste(text: String, app: &mut App) {
    app.input_scroll_decoupled = false;
    insert_paste_block(text, app, false);
}

pub(super) fn handle_paste_preview_key(
    k: crossterm::event::KeyEvent,
    app: &mut App,
    state: &mut crate::function::PastePreviewState,
) -> bool {
    use crossterm::event::KeyCode;
    match k.code {
        KeyCode::Enter => {
            // Confirm paste.
            app.input_scroll_decoupled = false;
            if let Some(ref image) = state.image {
                // Insert [image #N] marker.
                app.push_input_undo();
                let idx = app.image_blocks.len() + 1;
                app.image_blocks.push_back(image.clone());
                app.input.insert_str(&format!("[image #{idx}]"));
            } else if let Some(ref text) = state.text {
                // Use insert_paste_block to create [paste N lines] marker.
                // This also handles image path detection as a fallback.
                insert_paste_block(text.clone(), app, false);
            }
            close_active_function_tab(app);
            true
        }
        KeyCode::Esc => {
            // Cancel: close the tab without pasting.
            close_active_function_tab(app);
            true
        }
        _ => false,
    }
}

/// Quick MIME detection from magic bytes. Defaults to PNG.
pub(super) fn infer_image_type(bytes: &[u8]) -> &'static str {
    if bytes.len() < 4 {
        return "image/png";
    }
    if bytes[0] == 0xFF && bytes[1] == 0xD8 && bytes[2] == 0xFF {
        return "image/jpeg";
    }
    if bytes[0] == 0x47 && bytes[1] == 0x49 && bytes[2] == 0x46 {
        return "image/gif";
    }
    if bytes.len() >= 8
        && bytes[0] == 0x89
        && bytes[1] == 0x50
        && bytes[2] == 0x4E
        && bytes[3] == 0x47
    {
        return "image/png";
    }
    if bytes.len() >= 12
        && bytes[0] == 0x52
        && bytes[1] == 0x49
        && bytes[2] == 0x46
        && bytes[3] == 0x46
        && bytes[8] == 0x57
        && bytes[9] == 0x45
        && bytes[10] == 0x42
        && bytes[11] == 0x50
    {
        return "image/webp";
    }
    "image/png"
}
/// `quota=true` 表示这是 legacy 逐字符终端（如 conhost），需要在
/// handle_key 里抑制随后重发的字符，避免输入重复。
/// Image file extensions that we support loading directly from path.
const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp"];

/// If `text` looks like a file path ending in a known image extension,
/// load the file from disk and insert it as an `[image #K]` marker.
/// Returns `true` if an image was successfully loaded and inserted.
pub(super) fn try_insert_image_from_path(text: &str, app: &mut App) -> bool {
    let path = std::path::Path::new(text.trim().trim_matches('"'));
    let ext = match path.extension().and_then(|e| e.to_str()) {
        Some(e) => e.to_ascii_lowercase(),
        None => return false,
    };
    if !IMAGE_EXTENSIONS.contains(&ext.as_str()) {
        return false;
    }
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => return false,
    };
    let media_type = infer_image_type(&bytes);
    use sha2::{Digest, Sha256};
    let hash = hex::encode(Sha256::digest(&bytes));
    let assets_dir = match crate::session::store::assets_dir(&app.session_id) {
        Ok(d) => d,
        Err(_) => return false,
    };
    if let Err(e) = std::fs::create_dir_all(&assets_dir) {
        use crate::function::notifications::ToastLevel;
        app.notify(ToastLevel::Warn, format!("image: create assets dir: {e}"));
        return false;
    }
    let extension = media_type.split('/').nth(1).unwrap_or("png");
    let filename = format!("{hash}.{extension}");
    let asset_path = assets_dir.join(&filename);
    if !asset_path.exists() {
        if let Err(e) = std::fs::write(&asset_path, &bytes) {
            use crate::function::notifications::ToastLevel;
            app.notify(ToastLevel::Warn, format!("image: write {filename}: {e}"));
            return false;
        }
    }
    let attachment = crate::session::ImageAttachment {
        asset_path: asset_path.clone(),
        media_type: media_type.to_string(),
        byte_size: bytes.len() as u64,
        width: 0,
        height: 0,
    };
    let idx = app.image_blocks.len() + 1;
    app.push_input_undo();
    app.image_blocks.push_back(attachment);
    let marker = format!("[image #{idx}]");
    if app.input.has_selection() {
        app.input.delete_selection();
    }
    app.input.insert_str(&marker);
    app.sync_completion();
    use crate::function::notifications::ToastLevel;
    app.notify(
        ToastLevel::Ok,
        format!("image #{idx} attached ({media_type})"),
    );
    true
}

pub(super) fn insert_paste_block(text: String, app: &mut App, quota: bool) {
    let mut text = normalize_paste_text(&text);
    // Strip trailing newline so the paste doesn't inadvertently send
    // the prompt when Enter is pressed afterwards.
    if text.ends_with('\n') {
        text.pop();
    }
    if let Ok(mut cb) = arboard::Clipboard::new() {
        if let Ok(clip) = cb.get_text() {
            let clip = normalize_paste_text(&clip);
            if !clip.is_empty() && (clip == text || clip.contains(&text)) {
                text = clip;
            }
        }
    }
    if text.is_empty() {
        return;
    }
    // If the paste text looks like a local image file path, load it directly.
    if try_insert_image_from_path(&text, app) {
        // Also update last_paste_text so the dedup check catches
        // repeated burst classifications of the same path.
        app.last_paste_text = Some(text.clone());
        app.last_paste_at = Some(Instant::now());
        return;
    }
    let now = Instant::now();
    if app
        .last_paste_text
        .as_ref()
        .map(|last| last == &text)
        .unwrap_or(false)
        && app
            .last_paste_at
            .map(|at| now.duration_since(at) < Duration::from_secs(2))
            .unwrap_or(false)
    {
        return;
    }
    app.last_paste_text = Some(text.clone());
    app.last_paste_at = Some(now);
    if quota {
        app.paste_key_quota = text.chars().count();
    }
    if app.input.has_selection() {
        app.input.delete_selection();
    }
    let line_count = paste_line_count(&text);
    let marker = format!("[paste {line_count} lines]");
    app.push_input_undo();
    app.paste_blocks.push_back(text);
    app.input.insert_str(&marker);
    app.sync_completion();
}

pub(super) fn normalize_paste_text(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

pub(super) fn paste_line_count(text: &str) -> usize {
    text.lines().count().max(1)
}
pub(super) fn try_remove_paste_marker(app: &mut App) -> bool {
    let buf = &app.input.buffer;
    let cursor = app.input.cursor;
    if cursor < "[paste 1 lines]".len() || !buf.is_char_boundary(cursor) {
        return false;
    }
    let before = &buf[..cursor];
    // Find "[paste " backwards from cursor
    if let Some(start) = before.rfind("[paste ") {
        let candidate = &buf[start..cursor];
        if let Some(rest) = candidate
            .strip_prefix("[paste ")
            .and_then(|s| s.strip_suffix(" lines]"))
        {
            if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
                app.push_input_undo();
                app.input.buffer.replace_range(start..cursor, "");
                app.input.cursor = start;
                app.paste_blocks.pop_front();
                return true;
            }
        }
    }
    false
}

pub(super) fn try_remove_image_marker(app: &mut App) -> bool {
    let buf = &app.input.buffer;
    let cursor = app.input.cursor;
    // Minimum length: "[image #1]" is 9 chars.
    if cursor < 9 || !buf.is_char_boundary(cursor) {
        return false;
    }
    let before = &buf[..cursor];
    if let Some(start) = before.rfind("[image #") {
        let candidate = &buf[start..cursor];
        if let Some(rest) = candidate
            .strip_prefix("[image #")
            .and_then(|s| s.strip_suffix(']'))
        {
            if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
                let idx: usize = rest.parse().unwrap_or(0);
                if idx > 0 && idx <= app.image_blocks.len() {
                    // Remove the image file from disk.
                    app.push_input_undo();
                    if let Some(att) = app.image_blocks.get(idx - 1) {
                        let _ = std::fs::remove_file(&att.asset_path);
                    }
                    app.image_blocks.remove(idx - 1);
                    app.input.buffer.replace_range(start..cursor, "");
                    app.input.cursor = start;
                    // Re-number remaining image markers in the buffer.
                    renumber_image_markers(app);
                    return true;
                }
            }
        }
    }
    false
}

/// Re-number all `[image #K]` markers in the input buffer to match
/// the current `app.image_blocks` order (1-based). Called after a
/// marker is removed from the middle of the list.
pub(super) fn renumber_image_markers(app: &mut App) {
    let buf = &app.input.buffer.clone();
    let mut new_buf = buf.clone();
    let mut block_idx = 1usize;
    let mut search_start = 0usize;
    // Track cumulative byte delta for markers that appear before the
    // cursor, so we can adjust the cursor after the buffer is replaced.
    let cursor = app.input.cursor;
    let mut delta_before_cursor = 0i64;
    loop {
        let remaining = &new_buf[search_start..];
        let Some(marker_start) = remaining.find("[image #") else {
            break;
        };
        let abs_start = search_start + marker_start;
        let after_marker = &new_buf[abs_start + 8..];
        let Some(bracket_end) = after_marker.find(']') else {
            break;
        };
        let num_str = &after_marker[..bracket_end];
        if !num_str.chars().all(|c| c.is_ascii_digit()) {
            search_start = abs_start + 1;
            continue;
        }
        let old_len = 8 + bracket_end + 1; // "[image #N]" length
        let new_marker = format!("[image #{block_idx}]");
        let new_len = new_marker.len();
        if abs_start < cursor {
            delta_before_cursor += (new_len as i64) - (old_len as i64);
        }
        new_buf.replace_range(abs_start..abs_start + old_len, &new_marker);
        search_start = abs_start + new_len;
        block_idx += 1;
    }
    app.input.buffer = new_buf;
    if delta_before_cursor != 0 {
        let adjusted = (cursor as i64 + delta_before_cursor).max(0) as usize;
        // Clamp to buffer len and snap to nearest char boundary.
        let adjusted = adjusted.min(app.input.buffer.len());
        if !app.input.buffer.is_char_boundary(adjusted) {
            let mut p = adjusted;
            while p > 0 && !app.input.buffer.is_char_boundary(p) {
                p -= 1;
            }
            app.input.cursor = p;
        } else {
            app.input.cursor = adjusted;
        }
    }
}

/// Replace `[image #K]` markers in `raw` with the corresponding
/// `ContentPart`s from `image_blocks`, and collect them in order.
/// Returns `(cleaned_text, image_parts)`.
pub(super) fn expand_image_blocks(
    raw: &str,
    image_blocks: &mut VecDeque<crate::session::ImageAttachment>,
) -> (String, Vec<crate::session::ContentPart>) {
    let mut out = raw.to_string();
    let mut parts: Vec<crate::session::ContentPart> = Vec::new();
    let mut search_start = 0usize;
    loop {
        let remaining = &out[search_start..];
        let Some(marker_start) = remaining.find("[image #") else {
            break;
        };
        let abs_start = search_start + marker_start;
        let after_marker = &out[abs_start + 8..];
        let Some(bracket_end) = after_marker.find(']') else {
            break;
        };
        let num_str = &after_marker[..bracket_end];
        if !num_str.chars().all(|c| c.is_ascii_digit()) {
            search_start = abs_start + 1;
            continue;
        }
        let idx: usize = num_str.parse().unwrap_or(0);
        let old_len = 8 + bracket_end + 1;
        // Drain the corresponding image block.
        if idx > 0 && idx <= image_blocks.len() {
            let att = image_blocks.remove(idx - 1).unwrap();
            parts.push(crate::session::ContentPart::Image(att));
        }
        out.replace_range(abs_start..abs_start + old_len, "");
        search_start = abs_start; // re-scan after the removal
    }
    (out, parts)
}

/// Check if `raw` is a single image file path. If so, load the file
/// and push an `Image` ContentPart into `image_parts`. Returns true
/// if an image was loaded.
pub(super) fn try_extract_image_path_from_input(
    raw: &str,
    image_parts: &mut Vec<crate::session::ContentPart>,
    app: &mut App,
) -> bool {
    let trimmed = raw.trim().trim_matches('"');
    let path = std::path::Path::new(trimmed);
    let ext = match path.extension().and_then(|e| e.to_str()) {
        Some(e) => e.to_ascii_lowercase(),
        None => return false,
    };
    if !IMAGE_EXTENSIONS.contains(&ext.as_str()) {
        return false;
    }
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(_e) => {
            #[cfg(windows)]
            {
                let wide_path = format!("\\\\?\\{}", trimmed);
                let wide_path = std::path::Path::new(&wide_path);
                match std::fs::read(wide_path) {
                    Ok(b) => b,
                    Err(_) => return false,
                }
            }
            #[cfg(not(windows))]
            {
                return false;
            }
        }
    };
    let media_type = infer_image_type(&bytes);
    use sha2::{Digest, Sha256};
    let hash = hex::encode(Sha256::digest(&bytes));
    let assets_dir = match crate::session::store::assets_dir(&app.session_id) {
        Ok(d) => d,
        Err(_) => return false,
    };
    if let Err(e) = std::fs::create_dir_all(&assets_dir) {
        use crate::function::notifications::ToastLevel;
        app.notify(ToastLevel::Warn, format!("image: create assets dir: {e}"));
        return false;
    }
    let extension = media_type.split('/').nth(1).unwrap_or("png");
    let filename = format!("{hash}.{extension}");
    let asset_path = assets_dir.join(&filename);
    if !asset_path.exists() {
        if let Err(e) = std::fs::write(&asset_path, &bytes) {
            use crate::function::notifications::ToastLevel;
            app.notify(ToastLevel::Warn, format!("image: write {filename}: {e}"));
            return false;
        }
    }
    let attachment = crate::session::ImageAttachment {
        asset_path: asset_path.clone(),
        media_type: media_type.to_string(),
        byte_size: bytes.len() as u64,
        width: 0,
        height: 0,
    };
    app.image_blocks.push_back(attachment.clone());
    image_parts.push(crate::session::ContentPart::Image(attachment));
    let idx = app.image_blocks.len();
    app.notify(
        crate::function::notifications::ToastLevel::Ok,
        format!("image #{idx} loaded from path ({media_type})"),
    );
    true
}

pub(super) fn expand_paste_blocks(mut raw: String, paste_blocks: &mut VecDeque<String>) -> String {
    while let Some(text) = paste_blocks.pop_front() {
        let line_count = paste_line_count(&text);
        let marker = format!("[paste {line_count} lines]");
        let text = text.strip_suffix('\n').unwrap_or(&text);
        let block = format!("```paste\n{text}\n```");
        if raw.contains(&marker) {
            raw = raw.replacen(&marker, &block, 1);
        }
    }
    raw
}
