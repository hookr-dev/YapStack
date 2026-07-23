//! Tauri command for writing user-exported text (e.g. transcripts) to a
//! filesystem path the user chose via the frontend save dialog.

use super::error::CommandError;

/// Extensions this command is willing to write; keeps the renderer-callable
/// write bounded to text exports.
const ALLOWED_EXPORT_EXTENSIONS: [&str; 4] = ["md", "markdown", "txt", "text"];

fn is_allowed_export_path(path: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
        .is_some_and(|e| ALLOWED_EXPORT_EXTENSIONS.contains(&e))
}

/// Write UTF-8 `contents` to `path`.
///
/// Trusted-renderer boundary: `path` comes from the frontend save dialog and
/// cannot be verified here, so this is a renderer-callable file-write
/// primitive. It refuses non-text-export extensions, does not create
/// directories, and the `fs` plugin grant stays read-only so this remains the
/// only renderer write path.
#[tauri::command]
#[specta::specta]
pub async fn write_text_file(path: String, contents: String) -> Result<(), CommandError> {
    if !is_allowed_export_path(&path) {
        return Err(CommandError::InvalidInput {
            message: format!(
                "write_text_file only writes text exports ({}); refusing '{path}'",
                ALLOWED_EXPORT_EXTENSIONS.join("/"),
            ),
        });
    }
    std::fs::write(&path, contents).map_err(|e| CommandError::Internal {
        message: format!("failed to write {path}: {e}"),
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::is_allowed_export_path;

    #[test]
    fn allows_text_export_extensions() {
        assert!(is_allowed_export_path("/tmp/notes.md"));
        assert!(is_allowed_export_path("/tmp/notes.MARKDOWN"));
        assert!(is_allowed_export_path("transcript - my side.txt"));
        assert!(is_allowed_export_path("C:\\Users\\me\\out.TXT"));
    }

    #[test]
    fn rejects_non_text_targets() {
        assert!(!is_allowed_export_path("/tmp/app.exe"));
        assert!(!is_allowed_export_path("/etc/hosts")); // no extension
        assert!(!is_allowed_export_path("/tmp/noext"));
        assert!(!is_allowed_export_path("/tmp/archive.zip"));
        assert!(!is_allowed_export_path("/tmp/config.json"));
    }
}
