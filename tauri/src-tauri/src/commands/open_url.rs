use tauri::command;

#[command]
pub fn open_url(url: String) -> Result<(), String> {
    open::that(&url).map_err(|e| format!("打开 URL 失败: {}", e))
}
