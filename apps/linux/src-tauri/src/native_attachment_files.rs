//! User-initiated Linux file attachments from the clipboard or native window drop.
use base64::Engine as _;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, DragDropEvent, Manager, Webview, WebviewEvent, Window, WindowEvent};

const MAX_FILES: usize = 8;
const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_BATCH_BYTES: u64 = 64 * 1024 * 1024;
const DROP_LIFETIME: Duration = Duration::from_secs(15);

#[derive(Default)]
pub struct DropState(Mutex<Option<PendingDrop>>);

struct PendingDrop {
    token: String,
    paths: Vec<PathBuf>,
    created: Instant,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeAttachmentFile {
    file_name: String,
    mime_type: String,
    bytes_base64: String,
}

fn stage_drop(app: &AppHandle, label: &str, paths: &[PathBuf], x: f64, y: f64) {
    if label != "main" || paths.is_empty() {
        return;
    }
    let token = uuid::Uuid::new_v4().to_string();
    let Some(state) = app.try_state::<DropState>() else {
        return;
    };
    if let Ok(mut pending) = state.0.lock() {
        *pending = Some(PendingDrop {
            token: token.clone(),
            paths: paths.to_vec(),
            created: Instant::now(),
        });
    } else {
        return;
    }
    let Some(webview) = app.get_webview(label) else {
        return;
    };
    let detail = serde_json::json!({"token": token, "x": x, "y": y});
    let _ = webview.eval(format!(
        "window.dispatchEvent(new CustomEvent('openclaw-native-attachment-drop', {{detail: {detail}}}));"
    ));
}

pub fn handle_window_event(window: &Window, event: &WindowEvent) {
    if let WindowEvent::DragDrop(DragDropEvent::Drop { paths, position }) = event {
        stage_drop(
            window.app_handle(),
            window.label(),
            paths,
            position.x,
            position.y,
        );
    }
}

// Connected dashboards are child webviews; their drops do not surface as
// WindowEvent::DragDrop. Keep the window handler for local window content.
pub fn handle_webview_event(webview: &Webview, event: &WebviewEvent) {
    if let WebviewEvent::DragDrop(DragDropEvent::Drop { paths, position }) = event {
        stage_drop(
            webview.app_handle(),
            webview.label(),
            paths,
            position.x,
            position.y,
        );
    }
}

fn clipboard_paths(app: &AppHandle) -> Result<Vec<PathBuf>, String> {
    let (send, receive) = mpsc::sync_channel(1);
    app.run_on_main_thread(move || {
        let clipboard = gtk::Clipboard::get(&gtk::gdk::SELECTION_CLIPBOARD);
        let paths = clipboard
            .wait_for_uris()
            .into_iter()
            .filter_map(|uri| {
                let (path, host) = gtk::glib::filename_from_uri(uri.as_str()).ok()?;
                if host.is_some_and(|value| value.as_str() != "localhost") {
                    return None;
                }
                Some(path)
            })
            .collect();
        let _ = send.send(paths);
    })
    .map_err(|error| format!("Clipboard unavailable: {error}"))?;
    receive
        .recv_timeout(Duration::from_secs(3))
        .map_err(|_| "Clipboard did not respond".to_string())
}

fn read_files(paths: Vec<PathBuf>) -> Result<Vec<NativeAttachmentFile>, String> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    if paths.len() > MAX_FILES {
        return Err(format!("Select at most {MAX_FILES} files"));
    }
    let mut total = 0_u64;
    let mut result = Vec::with_capacity(paths.len());
    for path in paths {
        let metadata =
            std::fs::metadata(&path).map_err(|_| "A selected file cannot be read".to_string())?;
        if !metadata.is_file() {
            return Err("Directories cannot be attached".to_string());
        }
        total = total.saturating_add(metadata.len());
        if metadata.len() > MAX_FILE_BYTES || total > MAX_BATCH_BYTES {
            return Err("Selected files exceed the attachment size limit".to_string());
        }
        let bytes =
            std::fs::read(&path).map_err(|_| "A selected file cannot be read".to_string())?;
        let (content_type, _) =
            gtk::gio::content_type_guess(Some(&path), &bytes[..bytes.len().min(4096)]);
        let mime_type = gtk::gio::content_type_get_mime_type(&content_type)
            .map(|value| value.to_string())
            .unwrap_or_else(|| "application/octet-stream".to_string());
        result.push(NativeAttachmentFile {
            file_name: path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            mime_type,
            bytes_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        });
    }
    Ok(result)
}

/// Bridge user file gestures into the already-running Gateway Control UI.
/// The dashboard is served by the Gateway, not bundled with the AppImage.
pub fn initialization_script(origin: &str) -> String {
    const SCRIPT: &str = r#"(() => {
  const allowedOrigin = __ORIGIN__;
  if (window !== window.top) return;
  function nativeInvoke() {
    const internals = window.__TAURI_INTERNALS__;
    if (internals?.invoke) return internals.invoke.bind(internals);
    const core = window.__TAURI__?.core;
    return core?.invoke?.bind(core);
  }
  function inComposer(target) {
    return target instanceof Element &&
      Boolean(target.closest('.chat, .new-session-page__composer, .chat-session-rail'));
  }
  function forwardFiles(target, files) {
    if (!(target instanceof Element) || !target.isConnected || !inComposer(target)) return;
    const transfer = new DataTransfer();
    for (const entry of files) {
      const raw = atob(entry.bytesBase64);
      const bytes = new Uint8Array(raw.length);
      for (let i = 0; i < raw.length; i++) bytes[i] = raw.charCodeAt(i);
      transfer.items.add(new File([bytes], entry.fileName, {type: entry.mimeType}));
    }
    if (!transfer.files.length) return;
    const drop = new Event('drop', {bubbles: true, cancelable: true});
    Object.defineProperty(drop, 'dataTransfer', {value: transfer});
    target.dispatchEvent(drop);
  }
  window.addEventListener('paste', (event) => {
    if (location.origin !== allowedOrigin || !inComposer(event.target)) return;
    const data = event.clipboardData;
    if (!data || !Array.from(data.types || []).includes('text/uri-list')) return;
    if (Array.from(data.items || []).some((item) => item.kind === 'file')) return;
    const invoke = nativeInvoke();
    if (!invoke) return;
    const target = event.target;
    event.preventDefault();
    event.stopImmediatePropagation();
    invoke('native_attachment_files', {token: null})
      .then((files) => forwardFiles(target, files))
      .catch((error) => console.warn('Native file paste failed', error));
  }, true);
  window.addEventListener('openclaw-native-attachment-drop', (event) => {
    if (location.origin !== allowedOrigin) return;
    const detail = event.detail;
    if (!detail || typeof detail.token !== 'string') return;
    const scale = window.devicePixelRatio || 1;
    const target = document.elementFromPoint(detail.x / scale, detail.y / scale);
    if (!inComposer(target)) return;
    const invoke = nativeInvoke();
    if (!invoke) return;
    invoke('native_attachment_files', {token: detail.token})
      .then((files) => forwardFiles(target, files))
      .catch((error) => console.warn('Native file drop failed', error));
  });
})();"#;
    SCRIPT.replace(
        "__ORIGIN__",
        &serde_json::to_string(origin).unwrap_or_default(),
    )
}

#[tauri::command]
pub async fn native_attachment_files(
    app: AppHandle,
    token: Option<String>,
) -> Result<Vec<NativeAttachmentFile>, String> {
    let paths = if let Some(token) = token {
        let state = app.state::<DropState>();
        let mut pending = state
            .0
            .lock()
            .map_err(|_| "Drop state unavailable".to_string())?;
        match pending.take() {
            Some(drop) if drop.token == token && drop.created.elapsed() <= DROP_LIFETIME => {
                drop.paths
            }
            _ => return Err("File drop expired".to_string()),
        }
    } else {
        clipboard_paths(&app)?
    };
    tauri::async_runtime::spawn_blocking(move || read_files(paths))
        .await
        .map_err(|_| "Could not read selected files".to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_user_selected_file_without_exposing_its_path() {
        let path =
            std::env::temp_dir().join(format!("openclaw-native-{}.txt", uuid::Uuid::new_v4()));
        std::fs::write(&path, b"attachment probe").unwrap();
        let files = read_files(vec![path.clone()]).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].file_name,
            path.file_name().unwrap().to_string_lossy()
        );
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(&files[0].bytes_base64)
                .unwrap(),
            b"attachment probe"
        );
        assert!(!files[0].file_name.contains("/"));
    }

    #[test]
    fn rejects_directories_and_oversized_batches() {
        assert!(read_files(vec![std::env::temp_dir()]).is_err());
        assert!(read_files(vec![PathBuf::from("ignored"); MAX_FILES + 1]).is_err());
    }

    #[test]
    fn script_scopes_attachment_bridge_to_the_connected_origin() {
        let script = initialization_script("https://gateway.example");
        assert!(script.contains("const allowedOrigin = \"https://gateway.example\""));
        assert!(script.contains("location.origin !== allowedOrigin"));
    }
}
