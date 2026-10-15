use tauri::plugin::PluginHandle;
use tauri::Manager;

#[cfg(mobile)]
use tauri::plugin::mobile::PluginInvokeError;

#[cfg(mobile)]
use serde_json::Value;

// The in-app download path runs through an Android-side plugin: downloading
// must happen on the app process's own network stack (the system
// DownloadManager process often cannot reach intranet hubs), and MediaStore
// is the only permission-free way to land a file in public Downloads. The
// handle stays None anywhere the plugin never registered (desktop/iOS).
#[derive(Clone)]
#[allow(dead_code)] // the handle is only read on mobile targets
struct DownloadPlugin(Option<PluginHandle<tauri::Wry>>);

#[tauri::command]
async fn download_file(
  state: tauri::State<'_, DownloadPlugin>,
  url: String,
  name: String,
) -> Result<String, String> {
  #[cfg(mobile)]
  {
    let Some(handle) = state.0.as_ref() else {
      return Err("DM_FALLBACK".to_string());
    };
    handle
      .run_mobile_plugin_async::<Value>(
        "download",
        serde_json::json!({ "url": url, "name": name }),
      )
      .await
      .map(|v| {
        v.get("name")
          .and_then(|n| n.as_str())
          .unwrap_or("")
          .to_string()
      })
      .map_err(|e| match e {
        // Pre-Android-10 devices have no MediaStore.Downloads; the JS side
        // falls back to the anchor-click DownloadManager path on this marker.
        // Matched as a substring, not an equality: the plugin's reject handler
        // prefixes the exception class ("IOException: DM_FALLBACK") so a bare
        // timeout or hostname stays diagnosable from the toast.
        PluginInvokeError::InvokeRejected(err)
          if err.message.as_deref().is_some_and(|m| m.contains("DM_FALLBACK")) =>
        {
          "DM_FALLBACK".to_string()
        }
        other => other.to_string(),
      })
  }
  #[cfg(not(mobile))]
  {
    // Off-mobile the JS side never invokes (no Tauri shell → straight to the
    // anchor path); this only keeps the command compilable and total.
    let _ = (state, url, name);
    Err("DM_FALLBACK".to_string())
  }
}

// Root-screen double-back-to-exit (App.tsx): the app plugin's Kotlin `exit`
// command exists on this tauri version but has no ACL permission
// (`core:app:allow-exit` appeared in a later release), so the JS side calls
// this app command instead — app commands bypass the ACL (same as
// download_file below).
#[tauri::command]
fn exit_app(app: tauri::AppHandle) {
  app.exit(0);
}

// Open a local file (reveal=true: show it in Finder) on the machine the app
// runs on. The JS side only invokes this when the hub itself is loopback, so
// a bridge-reported path names a real local file. macOS-only — it is the one
// desktop target this app builds. The path passes as an argv element, never
// through a shell, so it cannot inject flags.
#[tauri::command]
fn open_local_path(path: String, reveal: bool) -> Result<(), String> {
  #[cfg(target_os = "macos")]
  {
    let mut cmd = std::process::Command::new("open");
    if reveal {
      cmd.arg("-R");
    }
    let status = cmd.arg(&path).status().map_err(|e| e.to_string())?;
    if status.success() {
      Ok(())
    } else {
      Err(format!("open exited with {}", status.code().unwrap_or(-1)))
    }
  }
  #[cfg(not(target_os = "macos"))]
  {
    let _ = (path, reveal);
    Err("unsupported platform".to_string())
  }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
  tauri::Builder::default()
    .plugin(
      tauri::plugin::Builder::<tauri::Wry>::new("download")
        .setup(|app, api| {
          #[cfg(target_os = "android")]
          {
            let handle = api.register_android_plugin("app.zcode.acp", "DownloadBridge")?;
            app.manage(DownloadPlugin(Some(handle)));
          }
          #[cfg(not(target_os = "android"))]
          {
            let _ = api; // only the android branch uses the plugin api
            app.manage(DownloadPlugin(None));
          }
          Ok(())
        })
        .build(),
    )
    .setup(|app| {
      if cfg!(debug_assertions) {
        app.handle().plugin(
          tauri_plugin_log::Builder::default()
            .level(log::LevelFilter::Info)
            .build(),
        )?;
      }
      Ok(())
    })
    .invoke_handler(tauri::generate_handler![download_file, exit_app, open_local_path])
    .run(tauri::generate_context!())
    .expect("error while running tauri application");
}
