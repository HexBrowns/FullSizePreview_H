//! メニュー「フルサイズプレビュー受け口を置く」（編集メニュー・レイヤーの右クリックメニュー）。
//!
//! 受け口はカメラ制御の対象に入ると空の画面しか受け取れない（実機 2026-09-27）。フィルタプラグインの登録には
//! 「カメラ制御の対象にしない」という指定が無く、カメラ制御の対象かは置いたオブジェクトごとの設定なので、
//! 置くときにこちらで外す。
//!
//! - 受け口が無いシーン: 受け口以外で一番大きいレイヤーの次に、フレーム 0 からシーンの最後まで置く
//! - 受け口があるシーン: カメラ制御の対象から外し、それより大きいレイヤーにオブジェクトがあれば一番下へ移す
//! - どちらも置いた受け口を選択状態にする。本体の「元に戻す」1 回で取り消せる（`call_edit_section` の中で編集するため）

use aviutl2::generic::{EditSection, ObjectFlagType, ObjectHandle};
use windows::core::PCWSTR;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONINFORMATION, MB_ICONWARNING, MB_OK};

use crate::receiver::RECEIVER_NAME;
use crate::EDIT_HANDLE;

pub const MENU_NAME: &str = "フルサイズプレビュー受け口を置く";

/// 結果。`notice` があるときだけメッセージを出す
struct Outcome {
    summary: String,
    notice: Option<String>,
}

/// メニューから呼ぶ（本体の UI スレッド。ユーザーの操作が起点なので `call_edit_section` を使ってよい）。
pub fn on_menu() {
    if !EDIT_HANDLE.is_ready() {
        return;
    }
    let result = EDIT_HANDLE.call_edit_section(|edit| place(edit));
    let (text, warn) = match result {
        Ok(Ok(outcome)) => {
            tracing::info!("FullSizePreview_H: {}", outcome.summary);
            match outcome.notice {
                Some(n) => (format!("{}\n\n{n}", outcome.summary), true),
                None => return,
            }
        }
        Ok(Err(e)) => {
            tracing::warn!("FullSizePreview_H: {MENU_NAME}: {e}");
            (format!("{RECEIVER_NAME}を置けませんでした。\n\n{e}"), true)
        }
        Err(e) => {
            tracing::warn!("FullSizePreview_H: {MENU_NAME}: 編集を始められませんでした: {e}");
            (format!("{RECEIVER_NAME}を置けませんでした（編集を始められませんでした: {e}）。"), true)
        }
    };
    show_message(&text, warn);
}

fn show_message(text: &str, warn: bool) {
    let owner = HWND(crate::input::HOST_HWND.load(std::sync::atomic::Ordering::Relaxed) as _);
    let text: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let title: Vec<u16> = "フルサイズプレビュー".encode_utf16().chain(std::iter::once(0)).collect();
    let icon = if warn { MB_ICONWARNING } else { MB_ICONINFORMATION };
    unsafe {
        MessageBoxW(Some(owner), PCWSTR(text.as_ptr()), PCWSTR(title.as_ptr()), MB_OK | icon);
    }
}

fn place(edit: &mut EditSection) -> Result<Outcome, String> {
    let layer_max = edit.info.layer_max;
    let last_frame = edit.info.frame_max;

    // 受け口と、受け口以外で一番大きいレイヤーを探す
    let mut receivers: Vec<(usize, usize, usize, ObjectHandle)> = Vec::new();
    let mut bottom_other: Option<usize> = None;
    for layer in 0..=layer_max {
        for (lf, obj) in edit.objects_in_layer(layer) {
            let is_receiver = edit
                .count_object_effect(obj, RECEIVER_NAME)
                .map_err(|e| format!("オブジェクトを調べられませんでした: {e}"))?
                > 0;
            if is_receiver {
                receivers.push((lf.layer, lf.start, lf.end, obj));
            } else {
                bottom_other = Some(bottom_other.map_or(lf.layer, |b| b.max(lf.layer)));
            }
        }
    }
    let target_layer = bottom_other.map_or(0, |b| b + 1);

    if receivers.is_empty() {
        let length = last_frame + 1;
        let obj = edit
            .create_object(RECEIVER_NAME, target_layer, 0, Some(length))
            .map_err(|e| format!("レイヤー {} に作れませんでした: {e}", target_layer + 1))?;
        turn_off_camera(edit, obj)?;
        let _ = edit.set_focus_object(Some(obj));
        return Ok(Outcome {
            summary: format!(
                "{RECEIVER_NAME}をレイヤー {} のフレーム 0〜{last_frame} に置き、カメラ制御の対象から外しました",
                target_layer + 1
            ),
            notice: None,
        });
    }

    let mut moved = Vec::new();
    let mut gaps = Vec::new();
    for &(layer, start, end, obj) in &receivers {
        if layer < target_layer {
            edit.move_object(obj, target_layer, start).map_err(|e| {
                format!("レイヤー {} の受け口をレイヤー {} へ移せませんでした: {e}", layer + 1, target_layer + 1)
            })?;
            moved.push(format!("レイヤー {} → {}", layer + 1, target_layer + 1));
        }
        turn_off_camera(edit, obj)?;
        if start > 0 || end < last_frame {
            gaps.push(format!("フレーム {start}〜{end}"));
        }
    }
    let _ = edit.set_focus_object(Some(receivers[0].3));

    let mut summary = format!("{RECEIVER_NAME}（{} 個）をカメラ制御の対象から外しました", receivers.len());
    if !moved.is_empty() {
        summary.push_str(&format!("。一番下のレイヤーへ移しました（{}）", moved.join("、")));
    }
    let mut notices = Vec::new();
    if receivers.len() > 1 {
        notices.push(format!("{RECEIVER_NAME}が {} 個あります。1 個にしてください。", receivers.len()));
    }
    if !gaps.is_empty() && receivers.len() == 1 {
        notices.push(format!(
            "受け口が {} にしかありません。シーン全体（フレーム 0〜{last_frame}）で映すには、受け口を伸ばしてください。",
            gaps.join("、")
        ));
    }
    Ok(Outcome { summary, notice: (!notices.is_empty()).then(|| notices.join("\n")) })
}

/// カメラ制御の対象から外し、読み返して確かめる。
fn turn_off_camera(edit: &mut EditSection, obj: ObjectHandle) -> Result<(), String> {
    edit.set_object_flag(obj, ObjectFlagType::EnableCamera, false)
        .map_err(|e| format!("カメラ制御の対象から外せませんでした: {e}"))?;
    match edit.get_object_flag(obj, ObjectFlagType::EnableCamera) {
        Ok(false) => Ok(()),
        Ok(true) => Err("カメラ制御の対象から外したはずが、読み返すと対象のままでした".into()),
        Err(e) => Err(format!("カメラ制御の設定を読み返せませんでした: {e}")),
    }
}
