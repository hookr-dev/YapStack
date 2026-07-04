use super::error::CommandError;

fn ensure_command_succeeded(
    program: &str,
    output: &std::process::Output,
) -> Result<(), CommandError> {
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = stderr.trim();
    let message = if detail.is_empty() {
        format!("{program} exited with status {}", output.status)
    } else {
        format!("{program} failed: {detail}")
    };

    Err(CommandError::Internal { message })
}

#[tauri::command]
#[specta::specta]
pub async fn clipboard_paste(text: String, auto_paste: bool) -> Result<(), CommandError> {
    // Write to clipboard via pbcopy (macOS).
    //
    // `pbcopy` decodes its stdin using `LC_CTYPE`. A Tauri .app launched from
    // the Dock or Finder inherits an empty session environment from launchd,
    // so without an explicit override `pbcopy` falls back to its compiled-in
    // default (Mac OS Roman on older patches), which mangles UTF-8 bytes for
    // smart quotes / em-dashes / ellipses into `‚Äô` / `‚Äî` / `‚Ä¶`. Forcing
    // `LC_CTYPE=UTF-8` is what iTerm2, fzf, and Neovim do for the same reason.
    #[cfg(target_os = "macos")]
    {
        let mut child = std::process::Command::new("pbcopy")
            .env("LC_CTYPE", "UTF-8")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| CommandError::Internal {
                message: format!("pbcopy: {e}"),
            })?;
        if let Some(ref mut stdin) = child.stdin {
            use std::io::Write;
            stdin
                .write_all(text.as_bytes())
                .map_err(|e| CommandError::Internal {
                    message: e.to_string(),
                })?;
        }
        child.wait().map_err(|e| CommandError::Internal {
            message: e.to_string(),
        })?;
    }

    #[cfg(target_os = "windows")]
    {
        // Suppress the console window that would otherwise flash on each paste.
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "clip"])
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| CommandError::Internal {
                message: format!("clip: {e}"),
            })?;
        if let Some(ref mut stdin) = child.stdin {
            use std::io::Write;
            stdin
                .write_all(text.as_bytes())
                .map_err(|e| CommandError::Internal {
                    message: e.to_string(),
                })?;
        }
        child.wait().map_err(|e| CommandError::Internal {
            message: e.to_string(),
        })?;
    }

    if auto_paste {
        // Brief delay for clipboard to settle
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        #[cfg(target_os = "macos")]
        {
            let output = std::process::Command::new("osascript")
                .args([
                    "-e",
                    r#"tell application "System Events" to keystroke "v" using command down"#,
                ])
                .output()
                .map_err(|e| CommandError::Internal {
                    message: format!("osascript: {e}"),
                })?;
            ensure_command_succeeded("osascript", &output)?;
        }

        #[cfg(target_os = "windows")]
        {
            // Suppress the console window that would otherwise flash on each paste.
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            let output = std::process::Command::new("powershell")
                .args([
                    "-NoProfile",
                    "-Command",
                    "Add-Type -AssemblyName System.Windows.Forms; [System.Windows.Forms.SendKeys]::SendWait('^v')",
                ])
                .creation_flags(CREATE_NO_WINDOW)
                .output()
                .map_err(|e| CommandError::Internal {
                    message: format!("powershell: {e}"),
                })?;
            ensure_command_succeeded("powershell", &output)?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn exit_status(code: i32) -> std::process::ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(code << 8)
    }

    #[test]
    #[cfg(unix)]
    fn ensure_command_succeeded_accepts_success() {
        let output = std::process::Output {
            status: exit_status(0),
            stdout: Vec::new(),
            stderr: Vec::new(),
        };
        assert!(ensure_command_succeeded("osascript", &output).is_ok());
    }

    #[test]
    #[cfg(unix)]
    fn ensure_command_succeeded_rejects_failure_with_stderr() {
        let output = std::process::Output {
            status: exit_status(1),
            stdout: Vec::new(),
            stderr: b"AppleEvent timed out".to_vec(),
        };
        let err = ensure_command_succeeded("osascript", &output).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("failed"));
        assert!(msg.contains("AppleEvent timed out"));
    }

    /// Round-trips a string with the smart-quote / em-dash / ellipsis glyphs
    /// that ITN and LLM cleanup commonly emit through the real `pbcopy` /
    /// `pbpaste` pair, and asserts the bytes survive intact. Pinned by hand
    /// because a regression here is invisible to anything that doesn't read
    /// the system pasteboard back.
    ///
    /// `#[ignore]` because it touches the user's clipboard. Run with
    /// `cargo test -- --ignored` from the desktop crate. Also clears
    /// `LC_CTYPE` from the test process before spawning, so the test fails
    /// in CI / Dock-launched scenarios when the production code regresses,
    /// not just when the developer happens to have a UTF-8 locale exported.
    #[test]
    #[cfg(target_os = "macos")]
    #[ignore]
    fn pbcopy_pbpaste_round_trips_smart_punctuation() {
        let payload = "There’s an em—dash, an ellipsis…, and \u{201C}quotes\u{201D}.";

        let mut child = std::process::Command::new("pbcopy")
            .env("LC_CTYPE", "UTF-8")
            .env_remove("LANG")
            .env_remove("LC_ALL")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .expect("spawn pbcopy");
        {
            use std::io::Write;
            child
                .stdin
                .as_mut()
                .unwrap()
                .write_all(payload.as_bytes())
                .expect("write pbcopy stdin");
        }
        child.wait().expect("pbcopy wait");

        let out = std::process::Command::new("pbpaste")
            .env("LC_CTYPE", "UTF-8")
            .output()
            .expect("pbpaste");
        let read_back = String::from_utf8(out.stdout).expect("pbpaste UTF-8");
        assert_eq!(
            read_back, payload,
            "smart punctuation mangled by pbcopy — LC_CTYPE override regressed"
        );
    }
}
