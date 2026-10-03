//! 再生が「空白キー」で始まったかを見分けるための入力の記録。
//!
//! SDK の再生開始イベント（`CHANGE_EDIT_STATE`）には、何で再生したかが入っていない。
//! そこで本体の UI スレッドにスレッド限定のフックを掛け、最後の空白キーとマウスボタンの押下時刻を覚えておく。
//! 再生が始まった時点で「空白キーが最近押され、その後にマウスボタンが押されていない」なら空白キーでの再生とみなす。
//! プレビュー再生ボタンと右クリックメニューの「再生」はマウスの押下が後に来るので、開かない側に倒れる。

use std::sync::atomic::{AtomicIsize, AtomicU64, Ordering};

use parking_lot::Mutex;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON, VK_RBUTTON, VK_SPACE};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetWindowThreadProcessId, SetWindowsHookExW, UnhookWindowsHookEx, HC_ACTION,
    HHOOK, WH_KEYBOARD, WH_MOUSE, WM_LBUTTONDOWN, WM_MBUTTONDOWN, WM_NCLBUTTONDOWN,
    WM_NCRBUTTONDOWN, WM_RBUTTONDOWN, WM_XBUTTONDOWN,
};

/// 空白キーの押下からこの時間内に再生が始まれば、空白キーでの再生とみなす
const SPACE_WINDOW_MS: u64 = 1500;

static LAST_SPACE_MS: AtomicU64 = AtomicU64::new(0);
static LAST_MOUSE_MS: AtomicU64 = AtomicU64::new(0);
/// フックを掛けた本体の UI スレッドの ID（0 なら未設定）
static HOOK_THREAD: AtomicU64 = AtomicU64::new(0);
/// 本体のメインウィンドウ
pub static HOST_HWND: AtomicIsize = AtomicIsize::new(0);

struct Hooks {
    handles: Vec<HHOOK>,
}
// HHOOK はただのハンドル値。外すのは Drop のときだけ
unsafe impl Send for Hooks {}

static HOOKS: Mutex<Option<Hooks>> = Mutex::new(None);

fn now_ms() -> u64 {
    unsafe { GetTickCount64() }
}

unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && wparam.0 == VK_SPACE.0 as usize {
        let flags = lparam.0 as u32;
        let key_up = flags & (1 << 31) != 0;
        let repeat = flags & (1 << 30) != 0;
        if !key_up && !repeat {
            LAST_SPACE_MS.store(now_ms(), Ordering::Relaxed);
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let msg = wparam.0 as u32;
        if matches!(
            msg,
            WM_LBUTTONDOWN
                | WM_RBUTTONDOWN
                | WM_MBUTTONDOWN
                | WM_XBUTTONDOWN
                | WM_NCLBUTTONDOWN
                | WM_NCRBUTTONDOWN
        ) {
            LAST_MOUSE_MS.store(now_ms(), Ordering::Relaxed);
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// 本体の UI スレッドにフックを掛ける。2 回目以降は何もしない。
pub fn install(host_hwnd: HWND) {
    let mut guard = HOOKS.lock();
    if guard.is_some() {
        return;
    }
    HOST_HWND.store(host_hwnd.0 as isize, Ordering::Relaxed);
    let thread_id = unsafe { GetWindowThreadProcessId(host_hwnd, None) };
    if thread_id == 0 {
        tracing::warn!("FullSizePreview_H: 本体の UI スレッドが分からないため、空白キーの判定はキーの押下状態だけで行います");
        *guard = Some(Hooks { handles: Vec::new() });
        return;
    }
    HOOK_THREAD.store(thread_id as u64, Ordering::Relaxed);

    let mut handles = Vec::new();
    // 同じプロセスのスレッドに掛けるフックなので hmod は None でよい（SetWindowsHookEx の説明）
    for (kind, proc_, label) in [
        (WH_KEYBOARD, keyboard_proc as unsafe extern "system" fn(i32, WPARAM, LPARAM) -> LRESULT, "キーボード"),
        (WH_MOUSE, mouse_proc, "マウス"),
    ] {
        match unsafe { SetWindowsHookExW(kind, Some(proc_), None, thread_id) } {
            Ok(h) => handles.push(h),
            Err(e) => tracing::warn!("FullSizePreview_H: {label}のフックを掛けられませんでした: {e}"),
        }
    }
    *guard = Some(Hooks { handles });
}

pub fn uninstall() {
    if let Some(hooks) = HOOKS.lock().take() {
        for h in hooks.handles {
            unsafe {
                let _ = UnhookWindowsHookEx(h);
            }
        }
    }
}

/// 今始まった再生が空白キーによるものか。
pub fn started_by_space() -> bool {
    let now = now_ms();
    let space = LAST_SPACE_MS.load(Ordering::Relaxed);
    let mouse = LAST_MOUSE_MS.load(Ordering::Relaxed);
    let by_hook = space != 0 && now.saturating_sub(space) <= SPACE_WINDOW_MS && space >= mouse;
    // フックに載らない経路（別スレッドのウィンドウから転送されたキー等）の保険: 今まさに空白キーが押されていて、マウスボタンは押されていない
    let by_state = unsafe {
        (GetAsyncKeyState(VK_SPACE.0 as i32) as u16 & 0x8000) != 0
            && (GetAsyncKeyState(VK_LBUTTON.0 as i32) as u16 & 0x8000) == 0
            && (GetAsyncKeyState(VK_RBUTTON.0 as i32) as u16 & 0x8000) == 0
    };
    tracing::debug!(
        "FullSizePreview_H: 再生開始の判定 space={}ms前 mouse={}ms前 hook={by_hook} state={by_state}",
        now.saturating_sub(space),
        now.saturating_sub(mouse)
    );
    by_hook || by_state
}
