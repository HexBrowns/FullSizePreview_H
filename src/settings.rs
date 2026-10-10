//! 本体の設定の読み取り（`aviutl2.ini` の `[Player]`）。このプラグイン自身は設定を持たない。

use std::path::PathBuf;

/// この DLL（`.aux2`）のあるフォルダ。
fn plugin_dir() -> Option<PathBuf> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::System::LibraryLoader::{
        GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
        GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    };

    let mut module = HMODULE::default();
    let anchor = plugin_dir as *const () as *const u16;
    unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR(anchor),
            &mut module,
        )
        .ok()?;
    }
    let mut buf = vec![0u16; 1024];
    let len = unsafe { GetModuleFileNameW(Some(module), &mut buf) } as usize;
    if len == 0 || len >= buf.len() {
        return None;
    }
    let path = PathBuf::from(String::from_utf16_lossy(&buf[..len]));
    path.parent().map(|p| p.to_path_buf())
}

/// 本体のプレビュー再生の設定（`aviutl2.ini` の `[Player]`）。
///
/// 本体がいつ書き戻すかは確かめていない。再生速度を変えた直後は古い値を読んでいる可能性がある
pub struct PlayerConfig {
    /// 再生速度（1.0 = 100%）
    pub speed: f64,
    /// 「フレーム範囲選択を再生範囲にする」
    pub select_range: bool,
}

pub fn read_player_config() -> PlayerConfig {
    let config = PlayerConfig { speed: 1.0, select_range: false };
    // Plugin/FullSizePreview_H/ から 2 つ上がユーザーデータフォルダ（aviutl2.ini の場所）
    match plugin_dir().and_then(|d| d.ancestors().map(|a| a.join("aviutl2.ini")).find(|p| p.is_file())) {
        Some(ini) => read_player_config_at(&ini),
        None => config,
    }
}

/// `[Player]` の節を読んだところで打ち切る（`aviutl2.ini` は 300KB あり、`[Player]` は先頭の方にある。
/// v0.6.0 までは再生のたびに全部を読んでいた）
pub fn read_player_config_at(ini: &std::path::Path) -> PlayerConfig {
    use std::io::BufRead;
    let mut config = PlayerConfig { speed: 1.0, select_range: false };
    let Ok(file) = std::fs::File::open(ini) else {
        return config;
    };
    let mut reader = std::io::BufReader::new(file);
    let mut buf = Vec::new();
    let mut in_player = false;
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let text = String::from_utf8_lossy(&buf);
        let line = text.trim();
        if line.starts_with('[') {
            if in_player {
                break;
            }
            in_player = line == "[Player]";
            continue;
        }
        if !in_player {
            continue;
        }
        match line.split_once('=') {
            Some(("Speed", v)) => {
                if let Ok(percent) = v.trim().parse::<f64>() {
                    if percent > 0.0 {
                        config.speed = percent / 100.0;
                    }
                }
            }
            Some(("PreviewSelectRange", v)) => config.select_range = v.trim() == "1",
            _ => {}
        }
    }
    config
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_player_section_only() {
        let path = std::env::temp_dir().join("fullsizepreview_h_settings_test.ini");
        let text = "[Window]
Speed=999
[Player]
Volume=10
Speed=50
PreviewSelectRange=1
[After]
Speed=300
";
        std::fs::write(&path, text).unwrap();
        let c = read_player_config_at(&path);
        assert_eq!((c.speed, c.select_range), (0.5, true));
        // 節が無ければ既定
        std::fs::write(&path, "[Window]
Speed=999
").unwrap();
        let c = read_player_config_at(&path);
        assert_eq!((c.speed, c.select_range), (1.0, false));
        let _ = std::fs::remove_file(&path);
        // ファイルが無ければ既定
        let c = read_player_config_at(&std::env::temp_dir().join("fullsizepreview_h_missing.ini"));
        assert_eq!((c.speed, c.select_range), (1.0, false));
    }
}
