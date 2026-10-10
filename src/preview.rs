//! フルサイズプレビューウィンドウと、受け口から届いたフレームを時刻に合わせて映すループ。
//!
//! - ウィンドウは専用スレッドで持つ。最前面・非アクティブ（キーボードの入力先は本体のまま）なので、
//!   空白キーはそのまま本体に届いて再生を止められる。止まったら隠す
//! - 画像は受け口のカスタムオブジェクト（`receiver`）が、本体の描画したフレームバッファを渡してくる。
//!   こちらから描画は頼まない（v0.2.0 の `rendering_scene_video` は、重いシーンで 1 フレームを 2 回描くことになり 3.5 fps まで落ちた）
//! - 本体は再生位置より先のフレームを描いておく（再生バッファ）ことがあるので、届いた順に出さず、
//!   **時刻から割り出した再生位置**に合うものを出す。再生中も `get_edit_info().frame` は開始フレームのまま動かない
//!   （実機 2026-09-27: 5 秒再生して 0 のまま）ので、開始時のカーソル位置 ＋ 経過時間 × fps × 再生速度
//!   （`aviutl2.ini` の `[Player] Speed`）で数える
//! - ウィンドウは本体のメインウィンドウがあるモニターの中央に置き、大きさは「シーンの解像度」と
//!   「モニター」の小さい方（縦横比を保つ）。シーンの方が小さければ等倍で、余白を作らない。
//!   シーンの方が大きければ、表示スレッドでウィンドウの大きさへ縮めてから渡す（`compose`。GDI では等倍で描く）
//! - **本体の再生 fps を落とさない**（v0.6.1）: 受け口の読み出しは本体の描画スレッドに上乗せされるので、
//!   本体が追いつかなくなったら間引く（`throttle`）。表示スレッドは次のフレームの時刻か新しい 1 枚が届くまで眠り、
//!   本体の参照ロックを取る `get_edit_info` は 0.1 秒に 1 回まで
//! - 本体以外のアプリが前に出たら最前面をやめ、そのアプリの後ろへ下がる。本体へ戻ったら最前面に戻す（v0.6.1）

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HMODULE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, EndPaint, FillRect, GetMonitorInfoW, GetStockObject, InvalidateRect,
    MonitorFromWindow, SetBkMode, SetStretchBltMode, SetTextColor, StretchDIBits, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, BLACK_BRUSH, COLORONCOLOR, DIB_RGB_COLORS, DT_CENTER, DT_SINGLELINE,
    DT_VCENTER, HBRUSH, MONITORINFO, MONITOR_DEFAULTTONEAREST, PAINTSTRUCT, SRCCOPY, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::{
    GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_PIN,
    GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
};
use windows::Win32::UI::HiDpi::{
    SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect,
    GetForegroundWindow, GetMessageW, GetWindowThreadProcessId, IsWindowVisible, LoadCursorW,
    PostMessageW, PostQuitMessage, RegisterClassExW, SetWindowPos, ShowWindow, TranslateMessage,
    UnregisterClassW, HWND_TOPMOST, IDC_ARROW, MA_NOACTIVATE, MSG, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, SWP_SHOWWINDOW, SW_HIDE, WM_APP, WM_CLOSE, WM_DESTROY, WM_ERASEBKGND,
    WM_LBUTTONDOWN, WM_MOUSEACTIVATE, WM_PAINT, WM_RBUTTONDOWN, WNDCLASSEXW, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::compose::{analyze, center_pixel, compose, has_content, Compose, Scaler};
use crate::readback::{PixelFormat, Readback};
use crate::throttle::Throttle;
use crate::input::HOST_HWND;
use crate::receiver::RECEIVER_NAME;
use crate::EDIT_HANDLE;

const WM_APP_SHOW: u32 = WM_APP + 1;
const WM_APP_HIDE: u32 = WM_APP + 2;
const WM_APP_QUIT: u32 = WM_APP + 3;
/// 前面のアプリが本体（このプロセス）か、ほかのアプリかが変わった。wparam = 1 なら本体、lparam = 前面のウィンドウ
const WM_APP_FOREGROUND: u32 = WM_APP + 4;

/// ウィンドウクラスの名前。この DLL の HINSTANCE で登録し、`shutdown` で外す
const CLASS_NAME: PCWSTR = w!("FullSizePreview_H_Window");
/// ウィンドウができるのを待つ上限。超えたらこの回の再生では開かない（できたら次の再生から使う）
const WINDOW_CREATE_TIMEOUT: Duration = Duration::from_secs(3);
/// 終了時にスレッドの終わりを待つ上限（全部合わせて）。超えたら警告して DLL を pin する
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// 届いたフレームを溜めておく上限（枚数と合計バイト）。本体の再生バッファ（既定 500ms）より多めに取る
const QUEUE_MAX_FRAMES: usize = 30;
const QUEUE_MAX_BYTES: usize = 400 * 1024 * 1024;
/// これより古く届いたものは捨てる（繰り返し再生の折り返しで残った先の番号など）
const QUEUE_MAX_AGE: Duration = Duration::from_millis(2000);
/// 再生位置よりこれだけ（秒）以上古いフレームは、折り返しの後の番号とみなして選ばない
const PAST_WINDOW_SEC: f64 = 3.0;
/// 最初からこの枚数続けて空（全画素 0）なら、受け口がカメラ制御の対象になっているとみて案内する
const EMPTY_FRAMES_BEFORE_NOTICE: u32 = 8;
/// 使い終わった画素バッファを取っておく上限（枚数と合計バイト）。4K（1 枚 33MB）なら 2 枚
const POOL_MAX_FRAMES: usize = 8;
const POOL_MAX_BYTES: usize = 96 * 1024 * 1024;
/// 再生中のカーソルの移動（シーク）を見る間隔。`get_edit_info` は本体の参照ロックを取るので、毎フレームは呼ばない
const EDIT_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// 表示スレッドが一度に眠る上限（前面のアプリの確かめもこの間隔）
const MAX_WAIT: Duration = Duration::from_millis(50);
/// 止めるときに表示スレッドの終わりを待つ上限。超えたら待たずに戻り、`shutdown` で待つ
const STOP_JOIN_TIMEOUT: Duration = Duration::from_secs(1);
/// 再生中のカーソルの移動を INFO で出す回数（再生 1 回あたり。以後は閉じたときの集計だけ）
const REANCHOR_LOG_MAX: u32 = 3;

/// 再生位置の見積もり。
#[derive(Clone, Debug)]
struct Clock {
    /// 数え始めのフレームと時刻
    anchor_frame: usize,
    anchor_at: Instant,
    /// 1 秒あたりに進むフレーム数（fps × 再生速度）
    frames_per_sec: f64,
    /// 終端を越えたら先頭へ戻す範囲（`loop_end <= loop_start` なら戻さない）
    loop_start: usize,
    loop_end: usize,
}

impl Clock {
    /// `now` の次に再生位置が 1 つ進む時刻
    fn next_change(&self, now: Instant) -> Instant {
        if self.frames_per_sec <= 0.0 {
            return now + MAX_WAIT;
        }
        let elapsed = now.saturating_duration_since(self.anchor_at).as_secs_f64();
        let next = ((elapsed * self.frames_per_sec).floor() + 1.0) / self.frames_per_sec;
        // 浮動小数の丸めで境目の手前に落ちないよう、少しだけ後ろにする
        self.anchor_at + Duration::from_secs_f64(next) + Duration::from_micros(10)
    }

    fn frame_at(&self, now: Instant) -> usize {
        let elapsed = now.saturating_duration_since(self.anchor_at).as_secs_f64();
        let raw = self.anchor_frame + (elapsed * self.frames_per_sec).floor() as usize;
        if self.loop_end > self.loop_start && raw > self.loop_end {
            // 繰り返し再生でなければ本体が止まり、ウィンドウも閉じる。繰り返しなら範囲の先頭から
            self.loop_start + (raw - self.loop_start) % (self.loop_end - self.loop_start + 1)
        } else {
            raw
        }
    }
}

/// 受け口から届いた 1 枚（フレームバッファを写したもの）
struct Incoming {
    frame: usize,
    arrived: Instant,
    width: u32,
    height: u32,
    format: PixelFormat,
    raw: Vec<u8>,
}

/// 表示する 1 枚。背景色の上に合成済みの BGRA（GDI にそのまま渡せる形）
struct Frame {
    width: i32,
    height: i32,
    bgra: Vec<u8>,
}

struct Stats {
    received: u64,
    shown: u64,
    /// 届いた時点の再生位置との差（届いたフレーム − 再生位置）。正なら本体が先に描いている
    lead_sum: f64,
    lead_min: i64,
    lead_max: i64,
    /// 受け口での読み出し（GPU → CPU）にかかった時間。内訳は写す（`copy_image_resource`）と取り出す（`get_image_resource_data`）
    readback_ms_sum: f64,
    readback_ms_max: f64,
    copy_ms_sum: f64,
    data_ms_sum: f64,
    data_ms_max: f64,
    receivers: Vec<i64>,
    /// 表示スレッドの `get_edit_info`（本体の参照ロック）
    edit_info_calls: u64,
    edit_info_ms_sum: f64,
    edit_info_ms_max: f64,
    /// 再生中にカーソルが動いて数え直した回数
    reanchors: u32,
}

impl Stats {
    const fn new() -> Self {
        Self {
            received: 0,
            shown: 0,
            lead_sum: 0.0,
            lead_min: 0,
            lead_max: 0,
            readback_ms_sum: 0.0,
            readback_ms_max: 0.0,
            copy_ms_sum: 0.0,
            data_ms_sum: 0.0,
            data_ms_max: 0.0,
            receivers: Vec::new(),
            edit_info_calls: 0,
            edit_info_ms_sum: 0.0,
            edit_info_ms_max: 0.0,
            reanchors: 0,
        }
    }
}

/// ウィンドウのスレッドと `shutdown` の受け渡し。
///
/// ウィンドウはそのスレッドが自分で載せる（`publish`）。生成が `WINDOW_CREATE_TIMEOUT` を過ぎて遅れて
/// できた場合も載るので、`shutdown` は WM_APP_QUIT を送れる。載せるのと終了の印（`request_quit`）が
/// 行き違っても、どちらかが必ず相手に気づく（どちらも「書いてから読む」を SeqCst で行う）。
/// 終了の印を見たウィンドウのスレッドは、自分でウィンドウを壊して抜ける
struct WindowSlot {
    hwnd: AtomicIsize,
    quitting: AtomicBool,
}

impl WindowSlot {
    const fn new() -> Self {
        Self { hwnd: AtomicIsize::new(0), quitting: AtomicBool::new(false) }
    }

    fn get(&self) -> Option<isize> {
        match self.hwnd.load(Ordering::SeqCst) {
            0 => None,
            h => Some(h),
        }
    }

    fn is_quitting(&self) -> bool {
        self.quitting.load(Ordering::SeqCst)
    }

    /// ウィンドウのスレッドから呼ぶ。作ったウィンドウを載せる。
    /// 終了に入っていたら載せずに false を返す（呼んだ側が自分で壊して抜ける）
    fn publish(&self, h: isize) -> bool {
        self.hwnd.store(h, Ordering::SeqCst);
        if self.quitting.load(Ordering::SeqCst) {
            self.clear(h);
            false
        } else {
            true
        }
    }

    /// 終了に入る。載っているウィンドウがあれば返す（呼んだ側が WM_APP_QUIT を送る）
    fn request_quit(&self) -> Option<isize> {
        self.quitting.store(true, Ordering::SeqCst);
        self.get()
    }

    /// `h` が載っていれば外す（別のウィンドウが載っていたら触らない）
    fn clear(&self, h: isize) {
        let _ = self.hwnd.compare_exchange(h, 0, Ordering::SeqCst, Ordering::SeqCst);
    }
}

/// 終わりを待つ対象として覚えておくスレッドの一覧。`close_and_join` の後は起こさない
struct Workers {
    handles: Mutex<Vec<JoinHandle<()>>>,
    closed: AtomicBool,
}

impl Workers {
    const fn new() -> Self {
        Self { handles: Mutex::new(Vec::new()), closed: AtomicBool::new(false) }
    }

    /// 名前付きのスレッドを起こして覚える。閉じた後と、起こせなかったときは false
    fn spawn(&self, name: &str, f: impl FnOnce() + Send + 'static) -> bool {
        let mut handles = self.handles.lock();
        // 印はロックの中で見る（`close_and_join` が一覧を持ち去った後に足さない）
        if self.closed.load(Ordering::SeqCst) {
            return false;
        }
        // 終わったものは片付ける（終わっているので join はすぐ返る）
        let mut i = 0;
        while i < handles.len() {
            if handles[i].is_finished() {
                let _ = handles.swap_remove(i).join();
            } else {
                i += 1;
            }
        }
        match std::thread::Builder::new().name(name.to_string()).spawn(f) {
            Ok(h) => {
                handles.push(h);
                true
            }
            Err(e) => {
                tracing::error!("FullSizePreview_H: スレッド「{name}」を立てられませんでした: {e}");
                false
            }
        }
    }

    /// 待ちきれなかったスレッドを預かる（`close_and_join` で待つ）。閉じた後なら返す
    fn adopt(&self, handle: JoinHandle<()>) -> Result<(), JoinHandle<()>> {
        let mut handles = self.handles.lock();
        if self.closed.load(Ordering::SeqCst) {
            return Err(handle);
        }
        handles.push(handle);
        Ok(())
    }

    /// 閉じて（以後は起こさない）、起こしたスレッドが終わるのを `deadline` まで待つ。待ちきれなかった本数を返す
    fn close_and_join(&self, deadline: Instant) -> usize {
        let handles = {
            let mut handles = self.handles.lock();
            self.closed.store(true, Ordering::SeqCst);
            std::mem::take(&mut *handles)
        };
        handles.into_iter().filter_map(|h| join_until(h, deadline)).count()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.handles.lock().len()
    }
}

/// `deadline` まで終わりを待ち、終わったら join して None。間に合わなければ handle を返す
fn join_until(handle: JoinHandle<()>, deadline: Instant) -> Option<JoinHandle<()>> {
    while !handle.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    if handle.is_finished() {
        let _ = handle.join();
        None
    } else {
        Some(handle)
    }
}

struct Preview {
    window: WindowSlot,
    window_thread: Mutex<Option<JoinHandle<()>>>,
    /// クリック・WM_CLOSE で閉じるために起こしたスレッドと、止めるときに待ちきれなかった表示スレッド（`shutdown` で待つ）
    stoppers: Workers,
    display_thread: Mutex<Option<JoinHandle<()>>>,
    /// 表示中のセッション番号（0 = 閉じている）。開くたびに増える
    session: AtomicU64,
    next_session: AtomicU64,
    clock: Mutex<Option<Clock>>,
    queue: Mutex<VecDeque<Incoming>>,
    /// 新しい 1 枚が届いた・止まった（表示スレッドを起こす。`queue` のロックと組で使う）
    arrived: Condvar,
    /// 使い終わった画素バッファ（受け口で毎フレーム 8MB を確保し直さないため）。閉じたら手放す
    pool: Mutex<Vec<Vec<u8>>>,
    /// 受け口の読み出しの間引き（開いている間だけ）
    throttle: Mutex<Option<Throttle>>,
    frame: Mutex<Option<Arc<Frame>>>,
    status: Mutex<String>,
    stats: Mutex<Stats>,
    started_at: Mutex<Option<Instant>>,
    /// シーンの解像度（ウィンドウの大きさを決める）
    scene_size: Mutex<(i32, i32)>,
    /// シーンの背景色（透明部分に敷く。本体のプレビューと同じ見え方にする）
    background: Mutex<[u8; 3]>,
    /// start / stop を直列にする
    control: Mutex<()>,
}

static PREVIEW: Preview = Preview {
    window: WindowSlot::new(),
    window_thread: Mutex::new(None),
    stoppers: Workers::new(),
    display_thread: Mutex::new(None),
    session: AtomicU64::new(0),
    next_session: AtomicU64::new(0),
    clock: Mutex::new(None),
    queue: Mutex::new(VecDeque::new()),
    arrived: Condvar::new(),
    pool: Mutex::new(Vec::new()),
    throttle: Mutex::new(None),
    frame: Mutex::new(None),
    status: Mutex::new(String::new()),
    stats: Mutex::new(Stats::new()),
    started_at: Mutex::new(None),
    scene_size: Mutex::new((0, 0)),
    background: Mutex::new([0, 0, 0]),
    control: Mutex::new(()),
};

fn hwnd() -> Option<HWND> {
    PREVIEW.window.get().map(|h| HWND(h as _))
}

fn set_status(text: impl Into<String>) {
    *PREVIEW.status.lock() = text.into();
    invalidate();
}

fn invalidate() {
    if let Some(h) = hwnd() {
        unsafe {
            let _ = InvalidateRect(Some(h), None, false);
        }
    }
}

/// 空白キーの再生で受け取りの準備ができているか（ウィンドウはまだ出ていないこともある）。
pub fn is_open() -> bool {
    PREVIEW.session.load(Ordering::Acquire) != 0
}

/// 受け口が呼ばれるたびに呼ぶ（本体の描画スレッド）。この回に画像を読み出すか
/// （空白キーで再生している間だけ。本体が追いつかないときは間引く）。
pub fn should_read(now: Instant) -> bool {
    if !is_open() {
        return false;
    }
    let (read, notice) = match PREVIEW.throttle.lock().as_mut() {
        Some(t) => (t.on_call(now), t.take_notice()),
        None => (true, None),
    };
    if let Some(n) = notice {
        tracing::info!(
            "FullSizePreview_H: 本体が再生に追いついていないため（受け口の呼ばれる間隔 平均 {:.1}ms / 1 フレーム {:.1}ms、読み出し 平均 {:.1}ms）、読み出しを {} フレームに 1 回へ間引きます（本体の様子を見て最大 {} フレームに 1 回まで広げ、追いつけば戻します。この再生では以後出しません）",
            n.interval_ms,
            n.period_ms,
            n.cost_ms,
            n.every,
            crate::throttle::MAX_EVERY
        );
    }
    read
}

/// 使い終わったバッファを取っておく（開いている間だけ。枚数とバイト数の上限まで）。
fn recycle(buffer: Vec<u8>) {
    if !is_open() || buffer.capacity() == 0 {
        return;
    }
    let mut pool = PREVIEW.pool.lock();
    let bytes: usize = pool.iter().map(|b| b.capacity()).sum();
    if pool.len() < POOL_MAX_FRAMES && bytes + buffer.capacity() <= POOL_MAX_BYTES {
        pool.push(buffer);
    }
}

/// 受け口が読み出しに使うバッファ（使い回し）。
pub fn take_buffer() -> Vec<u8> {
    PREVIEW.pool.lock().pop().unwrap_or_default()
}

/// 使わなかったバッファを返す（読み出しに失敗した・ビデオスコープのためだけに読んだ）。
pub fn give_back(buffer: Vec<u8>) {
    recycle(buffer);
}

/// 受け口（本体の描画スレッド）から呼ぶ。読み出したフレームを溜めておく。
pub fn push(effect_id: i64, frame: usize, rb: Readback, raw: Vec<u8>) {
    if !is_open() || rb.width == 0 || rb.height == 0 {
        recycle(raw);
        return;
    }
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    let readback_ms = ms(rb.total());
    let arrived = Instant::now();
    if let Some(t) = PREVIEW.throttle.lock().as_mut() {
        t.on_read(readback_ms, arrived);
    }

    let target = PREVIEW.clock.lock().as_ref().map(|c| c.frame_at(arrived));
    {
        let mut stats = PREVIEW.stats.lock();
        stats.received += 1;
        stats.readback_ms_sum += readback_ms;
        stats.readback_ms_max = stats.readback_ms_max.max(readback_ms);
        stats.copy_ms_sum += ms(rb.copy);
        stats.data_ms_sum += ms(rb.data);
        stats.data_ms_max = stats.data_ms_max.max(ms(rb.data));
        if let Some(t) = target {
            let lead = frame as i64 - t as i64;
            if stats.received == 1 {
                stats.lead_min = lead;
                stats.lead_max = lead;
            }
            stats.lead_sum += lead as f64;
            stats.lead_min = stats.lead_min.min(lead);
            stats.lead_max = stats.lead_max.max(lead);
        }
        if !stats.receivers.contains(&effect_id) {
            stats.receivers.push(effect_id);
            if stats.receivers.len() == 2 {
                tracing::warn!("FullSizePreview_H: {RECEIVER_NAME}が複数あります（シーンオブジェクトの中のシーンにも置いていると、別のシーンの絵が混ざります）");
            }
        }
    }

    let mut queue = PREVIEW.queue.lock();
    queue.push_back(Incoming { frame, arrived, width: rb.width, height: rb.height, format: rb.format, raw });
    let mut bytes: usize = queue.iter().map(|q| q.raw.len()).sum();
    while queue.len() > QUEUE_MAX_FRAMES || bytes > QUEUE_MAX_BYTES {
        let Some(old) = queue.pop_front() else { break };
        bytes -= old.raw.len();
        recycle(old.raw);
    }
    drop(queue);
    PREVIEW.arrived.notify_one();
}

/// 溜めたフレームから、再生位置 `target` に出すものを選ぶ。
/// `target` 以下で一番新しい番号。ただし `past_window` より古いもの（折り返し後の番号）は選ばない。
fn pick(queue: &VecDeque<Incoming>, target: usize, past_window: usize) -> Option<usize> {
    let floor = target.saturating_sub(past_window);
    queue
        .iter()
        .enumerate()
        .filter(|(_, q)| q.frame <= target && q.frame >= floor)
        .max_by_key(|(_, q)| (q.frame, q.arrived))
        .map(|(i, _)| i)
}

/// ウィンドウのスレッドを立てる（1 本だけ）。
fn ensure_window() -> Option<HWND> {
    if let Some(h) = hwnd() {
        return Some(h);
    }
    let mut guard = PREVIEW.window_thread.lock();
    if let Some(h) = hwnd() {
        return Some(h);
    }
    if PREVIEW.window.is_quitting() {
        return None;
    }
    if let Some(t) = guard.take() {
        if !t.is_finished() {
            // 前の回の生成が時間切れの後もまだ続いている。2 本目は立てない（できたら次の再生から使う）
            *guard = Some(t);
            tracing::warn!("FullSizePreview_H: プレビューウィンドウをまだ作っている途中です（この再生では開きません）");
            return None;
        }
        // 生成に失敗して抜けたスレッド。片付けて作り直す
        let _ = t.join();
    }
    let (tx, rx) = std::sync::mpsc::channel::<bool>();
    let handle = match std::thread::Builder::new()
        .name("FullSizePreview_H window".into())
        .spawn(move || window_thread(tx))
    {
        Ok(h) => h,
        Err(e) => {
            tracing::error!("FullSizePreview_H: ウィンドウのスレッドを立てられませんでした: {e}");
            return None;
        }
    };
    *guard = Some(handle);
    match rx.recv_timeout(WINDOW_CREATE_TIMEOUT) {
        // ウィンドウはスレッドが自分で載せている
        Ok(true) => hwnd(),
        Ok(false) => {
            tracing::error!("FullSizePreview_H: プレビューウィンドウを作れませんでした");
            None
        }
        Err(_) => {
            tracing::error!(
                "FullSizePreview_H: プレビューウィンドウが {} 秒以内にできませんでした（この再生では開きません）",
                WINDOW_CREATE_TIMEOUT.as_secs()
            );
            None
        }
    }
}

/// 空白キーで再生が始まったときに呼ぶ（イベント用スレッド）。
/// ここでは受け取りの準備だけをする。ウィンドウは受け口から最初の 1 枚が届いたときに出す
/// （受け口の無いシーンでは開かない。ユーザーの指定、2026-09-27）。
pub fn start() {
    let _control = PREVIEW.control.lock();
    stop_locked();
    // 再生開始の時刻とカーソル位置は、ウィンドウを作るより先に取る（数え始めがずれないように）
    let started = Instant::now();
    let info = EDIT_HANDLE.get_edit_info();
    let player = crate::settings::read_player_config();
    let fps = *info.fps.numer() as f64 / (*info.fps.denom()).max(1) as f64;
    let (loop_start, loop_end) = match (&info.select_range, player.select_range) {
        (Some(r), true) => (*r.start(), *r.end()),
        _ => (0, info.frame_max),
    };
    let clock = Clock {
        anchor_frame: info.frame,
        anchor_at: started,
        frames_per_sec: fps * player.speed,
        loop_start,
        loop_end,
    };
    tracing::debug!(
        "FullSizePreview_H: 受け取りを始めます。{}x{} / 開始フレーム {} / {fps:.3} fps × 速度 {:.0}% / 範囲 {loop_start}〜{loop_end}",
        info.width,
        info.height,
        info.frame,
        player.speed * 100.0
    );

    let Some(h) = ensure_window() else {
        return;
    };
    let session = PREVIEW.next_session.fetch_add(1, Ordering::AcqRel) + 1;
    *PREVIEW.frame.lock() = None;
    *PREVIEW.status.lock() = String::new();
    *PREVIEW.stats.lock() = Stats::new();
    *PREVIEW.started_at.lock() = Some(started);
    *PREVIEW.scene_size.lock() = (info.width as i32, info.height as i32);
    *PREVIEW.background.lock() = info.background.map(|(r, g, b)| [r, g, b]).unwrap_or([0, 0, 0]);
    *PREVIEW.throttle.lock() = Some(Throttle::new(clock.frames_per_sec));
    *PREVIEW.clock.lock() = Some(clock);
    PREVIEW.session.store(session, Ordering::Release);
    let _ = h;
    match std::thread::Builder::new()
        .name("FullSizePreview_H display".into())
        .spawn(move || display_loop(session))
    {
        Ok(t) => *PREVIEW.display_thread.lock() = Some(t),
        Err(e) => tracing::error!("FullSizePreview_H: 表示スレッドを立てられませんでした: {e}"),
    }
}

/// 再生が止まったとき・ウィンドウをクリックしたときに呼ぶ。
pub fn stop() {
    let _control = PREVIEW.control.lock();
    stop_locked();
}

/// 止める。表示スレッドが `STOP_JOIN_TIMEOUT` で終わらなければ、待たずに預けて戻る（`shutdown` で待つ）。
///
/// 表示スレッドは眠っていても `arrived` で起こすので、ふつうはすぐ終わる。長く待たされうるのは
/// 本体の `get_edit_info`（参照ロック）の中にいるときだけ（0.1 秒に 1 回しか呼ばない）
fn stop_locked() {
    if let Some(t) = stop_display(Instant::now() + STOP_JOIN_TIMEOUT) {
        tracing::warn!(
            "FullSizePreview_H: 表示スレッドが {} 秒以内に止まりませんでした（待たずに続け、終了時に待ちます）",
            STOP_JOIN_TIMEOUT.as_secs()
        );
        if PREVIEW.stoppers.adopt(t).is_err() {
            tracing::warn!("FullSizePreview_H: 終了の処理に入っているため、表示スレッドを待てません");
        }
    }
}

/// 表示を止めて片付け、閉じたときの集計を出す。表示スレッドが `deadline` までに終わらなければ、その handle を返す
fn stop_display(deadline: Instant) -> Option<JoinHandle<()>> {
    let session = PREVIEW.session.swap(0, Ordering::AcqRel);
    {
        // 表示スレッドは `queue` のロックの中で印を見てから眠るので、ロックを取ってから起こせば取りこぼさない
        let _queue = PREVIEW.queue.lock();
        PREVIEW.arrived.notify_all();
    }
    if let Some(h) = hwnd() {
        unsafe {
            let _ = PostMessageW(Some(h), WM_APP_HIDE, WPARAM(0), LPARAM(0));
        }
    }
    let display = PREVIEW.display_thread.lock().take();
    let straggler = display.and_then(|t| join_until(t, deadline));
    *PREVIEW.clock.lock() = None;
    // 溜めたフレーム・使い回しのバッファ・最後の 1 枚を手放す（4K では合わせて数百 MB になる）
    PREVIEW.queue.lock().clear();
    PREVIEW.pool.lock().clear();
    *PREVIEW.frame.lock() = None;
    let throttle = PREVIEW.throttle.lock().take();
    if session == 0 {
        return straggler;
    }
    let elapsed = PREVIEW
        .started_at
        .lock()
        .take()
        .map(|t| t.elapsed().as_secs_f64())
        .unwrap_or(0.0);
    let stats = PREVIEW.stats.lock();
    if stats.received == 0 {
        tracing::debug!("FullSizePreview_H: {RECEIVER_NAME}が無いため開きませんでした");
        return straggler;
    }
    let per_sec = |n: u64| if elapsed > 0.0 { n as f64 / elapsed } else { 0.0 };
    let avg = |sum: f64| if stats.received > 0 { sum / stats.received as f64 } else { 0.0 };
    let (skipped, max_every) = throttle.map(|t| (t.skipped, t.max_every)).unwrap_or((0, 1));
    let edit_avg = if stats.edit_info_calls > 0 { stats.edit_info_ms_sum / stats.edit_info_calls as f64 } else { 0.0 };
    tracing::info!(
        "FullSizePreview_H: 閉じました。{elapsed:.1} 秒 / 受け取り {} 枚（{:.1} fps）/ 表示 {} 枚（{:.1} fps）/ 読み出し 平均 {:.1}ms 最大 {:.1}ms（写す 平均 {:.1}ms・取り出す 平均 {:.1}ms 最大 {:.1}ms）/ 間引き {} 回（最大 {} フレームに 1 回）/ get_edit_info {} 回 平均 {:.2}ms 最大 {:.2}ms / カーソルの移動 {} 回 / 再生位置との差 平均 {:.1} 最小 {} 最大 {} フレーム / 受け口 {} 個",
        stats.received,
        per_sec(stats.received),
        stats.shown,
        per_sec(stats.shown),
        avg(stats.readback_ms_sum),
        stats.readback_ms_max,
        avg(stats.copy_ms_sum),
        avg(stats.data_ms_sum),
        stats.data_ms_max,
        skipped,
        max_every,
        stats.edit_info_calls,
        edit_avg,
        stats.edit_info_ms_max,
        stats.reanchors,
        avg(stats.lead_sum),
        stats.lead_min,
        stats.lead_max,
        stats.receivers.len()
    );
    straggler
}

/// ウィンドウのクリック・WM_CLOSE から呼ぶ。閉じる処理（描画スレッドの join を含む）を別のスレッドで行う。
///
/// ウィンドウのスレッドで join を待たないのは、その間このウィンドウのメッセージが止まるため
/// （描画スレッドは本体の関数 `get_edit_info` を呼んでいて、どれだけ待たされるかはこちらで決められない）。
/// 起こしたスレッドは `shutdown` で終わりを待つ
fn request_stop() {
    if is_open() {
        PREVIEW.stoppers.spawn("FullSizePreview_H stop", stop);
    }
}

/// プラグインの解放時に呼ぶ（プラグインの Drop）。起こしたスレッドを全部止めて終わりを待ち、ウィンドウクラスを外す。
pub fn shutdown() {
    let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
    // 1. クリックで起こした「閉じる」スレッドを待つ（以後は起こさない）
    let mut left = PREVIEW.stoppers.close_and_join(deadline);
    // 2. 表示を止める（描画スレッドを join する）。「閉じる」スレッドが止まらずに印を持ったままなら、待たずに進む
    match PREVIEW.control.try_lock_until(deadline) {
        Some(_control) => {
            if stop_display(deadline).is_some() {
                left += 1;
            }
        }
        None => left += 1,
    }
    // 3. ウィンドウのスレッドを止める。生成が時間切れの後に遅れてできた場合も、終了の印を見て自分で抜ける
    if let Some(h) = PREVIEW.window.request_quit() {
        unsafe {
            let _ = PostMessageW(Some(HWND(h as _)), WM_APP_QUIT, WPARAM(0), LPARAM(0));
        }
    }
    let window_done = match PREVIEW.window_thread.lock().take() {
        Some(t) => join_until(t, deadline).is_none(),
        None => true,
    };
    if window_done {
        // ウィンドウが全部壊れた後で外す。残すと、DLL が外れた後も消えた wnd_proc を指すクラスが残る
        if let Some(instance) = dll_instance() {
            unsafe {
                let _ = UnregisterClassW(CLASS_NAME, Some(instance));
            }
        }
    } else {
        left += 1;
    }
    if left > 0 {
        tracing::warn!("FullSizePreview_H: 終了時に {left} 本のスレッドが時間内に止まりませんでした（DLL を外させないようにします）");
        pin_module();
    }
}

/// この DLL の HINSTANCE（ウィンドウクラスの登録と生成に使う）。
fn dll_instance() -> Option<HINSTANCE> {
    let mut module = HMODULE::default();
    let anchor = dll_instance as *const () as *const u16;
    // SAFETY: FROM_ADDRESS では `anchor` をこの DLL の中のアドレスとして読むだけ。参照カウントは変えない
    unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR(anchor),
            &mut module,
        )
        .ok()?;
    }
    Some(module.into())
}

/// 止まらないスレッドが残ったとき、この DLL をプロセスが終わるまで外させない（`au2-rs-plugin` の保険）。
fn pin_module() {
    let mut module = HMODULE::default();
    let anchor = pin_module as *const () as *const u16;
    // SAFETY: 同上。PIN はプロセスの終わりまで外させない
    let result = unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_PIN | GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
            PCWSTR(anchor),
            &mut module,
        )
    };
    if let Err(e) = result {
        tracing::warn!("FullSizePreview_H: DLL を pin できませんでした: {e}");
    }
}

/// 表示スレッドの作業場所（1 枚ごとに確保し直さない）
struct Workspace {
    scaler: Scaler,
    /// 次の 1 枚を書く BGRA。前に出した 1 枚を描き終わっていれば、そのバッファを使い回す
    spare: Vec<u8>,
}

/// 表示する大きさ（ウィンドウの大きさ）。ウィンドウのスレッドと同じ DPI の扱いで数える
fn display_size(scene: (i32, i32)) -> (usize, usize) {
    let (w, h) = match target_monitor_rect() {
        Some(m) => {
            let r = window_rect(m, scene);
            (r.right - r.left, r.bottom - r.top)
        }
        None => scene,
    };
    (w.max(0) as usize, h.max(0) as usize)
}

/// 前面のウィンドウがこのプロセス（本体）のものか。前面が無い（切り替えの途中）なら None
fn foreground_is_ours() -> Option<(bool, HWND)> {
    unsafe {
        let fg = GetForegroundWindow();
        if fg.0.is_null() {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(fg, Some(&mut pid));
        Some((pid == GetCurrentProcessId(), fg))
    }
}

/// 再生位置が変わるたびに、溜めたフレームから合うものを選んで出す。
///
/// 次のフレームの時刻・新しい 1 枚が届く・止める、のどれかまで眠る（v0.6.0 までは 1ms ごとに起きて
/// `get_edit_info` を呼んでいた）。シークを見る `get_edit_info` は `EDIT_POLL_INTERVAL` に 1 回
fn display_loop(session: u64) {
    // ウィンドウのスレッドと同じく実ピクセルで数える（ウィンドウの大きさに縮めるため）
    unsafe {
        let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let Some(mut clock) = PREVIEW.clock.lock().clone() else {
        return;
    };
    let past_window = (clock.frames_per_sec * PAST_WINDOW_SEC).ceil() as usize;
    let view = display_size(*PREVIEW.scene_size.lock());
    // 再生中に本体のカーソルが動いたら（シーク等）、そこから数え直す
    let mut cursor = clock.anchor_frame;
    let mut next_edit_poll = Instant::now() + EDIT_POLL_INTERVAL;
    let mut shown: Option<usize> = None;
    let mut compose_with = Compose { background: *PREVIEW.background.lock(), ignore_alpha: false };
    let mut work = Workspace { scaler: Scaler::default(), spare: Vec::new() };
    // 空でない 1 枚が来るまで、届いた画像が空かどうかを調べる
    let mut saw_content = false;
    let mut empty_frames: u32 = 0;
    // 再生は本体で空白キーを押して始まるので、前面は本体
    let mut ours_in_front = true;

    while PREVIEW.session.load(Ordering::Acquire) == session {
        let now = Instant::now();
        if now >= next_edit_poll {
            next_edit_poll = now + EDIT_POLL_INTERVAL;
            let t0 = Instant::now();
            let edit_frame = EDIT_HANDLE.get_edit_info().frame;
            let took = t0.elapsed().as_secs_f64() * 1000.0;
            let reanchors = {
                let mut stats = PREVIEW.stats.lock();
                stats.edit_info_calls += 1;
                stats.edit_info_ms_sum += took;
                stats.edit_info_ms_max = stats.edit_info_ms_max.max(took);
                if edit_frame != cursor {
                    stats.reanchors += 1;
                }
                stats.reanchors
            };
            if edit_frame != cursor {
                if reanchors <= REANCHOR_LOG_MAX {
                    tracing::info!(
                        "FullSizePreview_H: 再生中にカーソルが {cursor} から {edit_frame} へ動いたので、そこから数え直します{}",
                        if reanchors == REANCHOR_LOG_MAX { "（この再生では以後、閉じたときの集計にだけ出します）" } else { "" }
                    );
                }
                cursor = edit_frame;
                clock.anchor_frame = edit_frame;
                clock.anchor_at = now;
                *PREVIEW.clock.lock() = Some(clock.clone());
            }
        }
        if let Some((ours, fg)) = foreground_is_ours() {
            if ours != ours_in_front {
                ours_in_front = ours;
                if let Some(h) = hwnd() {
                    unsafe {
                        let _ = PostMessageW(Some(h), WM_APP_FOREGROUND, WPARAM(ours as usize), LPARAM(fg.0 as isize));
                    }
                }
            }
        }
        let target = clock.frame_at(now);

        let chosen = {
            let mut queue = PREVIEW.queue.lock();
            // 古すぎるものを捨てる
            while queue.front().is_some_and(|q| now.duration_since(q.arrived) > QUEUE_MAX_AGE) {
                if let Some(old) = queue.pop_front() {
                    recycle(old.raw);
                }
            }
            match pick(&queue, target, past_window) {
                Some(i) if shown != Some(queue[i].frame) => {
                    let chosen = queue.remove(i);
                    // 選んだものより前の番号は、もう出番が無い
                    if let Some(c) = &chosen {
                        let keep_from = c.frame;
                        let floor = keep_from.saturating_sub(past_window);
                        let mut rest = VecDeque::with_capacity(queue.len());
                        for q in queue.drain(..) {
                            if q.frame < keep_from && q.frame >= floor {
                                recycle(q.raw);
                            } else {
                                rest.push_back(q);
                            }
                        }
                        *queue = rest;
                    }
                    chosen
                }
                _ => {
                    // 出すものが無い。次のフレームの時刻・新しい 1 枚・止める・シークを見る時刻のどれかまで眠る。
                    // 止める側は印を下ろしてから、このロックを取って起こすので、印はロックの中で見る
                    if PREVIEW.session.load(Ordering::Acquire) == session {
                        let until = clock.next_change(now).min(now + MAX_WAIT).min(next_edit_poll);
                        let _ = PREVIEW.arrived.wait_until(&mut queue, until);
                    }
                    None
                }
            }
        };

        let Some(c) = chosen else { continue };
        if !saw_content {
            if has_content(&c.raw) {
                saw_content = true;
                if empty_frames >= EMPTY_FRAMES_BEFORE_NOTICE {
                    if let Some(t) = PREVIEW.throttle.lock().as_mut() {
                        t.set_idle(false);
                    }
                }
            } else {
                empty_frames += 1;
                if empty_frames == EMPTY_FRAMES_BEFORE_NOTICE {
                    // カメラ制御の対象の中では、受け口に届くのは合成前の空のバッファ（実機 2026-09-27）
                    tracing::warn!("FullSizePreview_H: {RECEIVER_NAME}に届く画像が {empty_frames} 枚続けて空です。受け口がカメラ制御の対象になっていないか確かめてください");
                    *PREVIEW.frame.lock() = None;
                    // 空の間は読み出しを 0.5 秒に 1 回へ（本体の描画スレッドで読む分を減らす）
                    if let Some(t) = PREVIEW.throttle.lock().as_mut() {
                        t.set_idle(true);
                    }
                    set_status(format!(
                        "{RECEIVER_NAME}に届く画像が空です。受け口がカメラ制御の対象になっていると画面が届きません。受け口を選んで、設定欄の「カメラ制御の対象」を外してください。編集メニュー「フルサイズプレビュー受け口を置く」でも外せます（クリックで閉じます）"
                    ));
                }
                if empty_frames >= EMPTY_FRAMES_BEFORE_NOTICE {
                    // 案内を出したまま、空の画像では上書きしない
                    recycle(c.raw);
                    continue;
                }
            }
        }
        if shown.is_none() {
            let px = analyze(&c.raw);
            // αが全面 0 なのに色がある場合は、αを当てにせず不透明として出す
            compose_with.ignore_alpha = px.alpha_max == 0 && px.rgb_max > 0;
            tracing::info!(
                "FullSizePreview_H: 最初の 1 枚（フレーム {} / {}x{} / {:?} / 表示 {}x{}）: α 最小 {} 最大 {} 平均 {:.1} / 色の最大 {} / 色のある画素 {} / 中央の画素 {:?} / 背景色 {:?} / αを無視: {}",
                c.frame,
                c.width,
                c.height,
                c.format,
                view.0,
                view.1,
                px.alpha_min,
                px.alpha_max,
                px.alpha_mean,
                px.rgb_max,
                px.colored,
                center_pixel(&c.raw, c.width, c.height),
                compose_with.background,
                compose_with.ignore_alpha
            );
        }
        let (w, h) = (c.width as usize, c.height as usize);
        let mut bgra = std::mem::take(&mut work.spare);
        if compose(&c.raw, (w, h), view, &compose_with, &mut work.scaler, &mut bgra) {
            let (fw, fh) = if view.0 > 0 && view.1 > 0 && view.0 <= w && view.1 <= h { view } else { (w, h) };
            let frame = Arc::new(Frame { width: fw as i32, height: fh as i32, bgra });
            let old = {
                let mut slot = PREVIEW.frame.lock();
                // 止められた後は出さない（次の再生の 1 枚を上書きしない）
                if PREVIEW.session.load(Ordering::Acquire) != session {
                    recycle(c.raw);
                    break;
                }
                slot.replace(frame)
            };
            // 描き終わっていれば（ウィンドウのスレッドが持っていなければ）、次の 1 枚に使い回す
            if let Some(old) = old.and_then(|f| Arc::try_unwrap(f).ok()) {
                work.spare = old.bgra;
            }
            PREVIEW.stats.lock().shown += 1;
            if shown.is_none() {
                // 最初の 1 枚が用意できてから出す（受け口の無いシーンでは出さない）
                if let Some(h) = hwnd() {
                    unsafe {
                        let _ = PostMessageW(Some(h), WM_APP_SHOW, WPARAM(0), LPARAM(0));
                    }
                }
            }
            shown = Some(c.frame);
            invalidate();
        } else {
            work.spare = bgra;
        }
        recycle(c.raw);
    }
}

// ---- ウィンドウ ----

/// ウィンドウを作ってメッセージを回す。作れたかを `tx` で知らせる（待つ側が時間切れで先に抜けていてもよい）。
fn window_thread(tx: std::sync::mpsc::Sender<bool>) {
    unsafe {
        // モニターの実ピクセルで大きさを決める（等倍表示のため）。このスレッドで作るウィンドウだけに効く
        let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        // exe ではなくこの DLL の HINSTANCE で登録する（`shutdown` で外す）
        let Some(instance) = dll_instance() else {
            tracing::error!("FullSizePreview_H: この DLL のハンドルを取れませんでした");
            let _ = tx.send(false);
            return;
        };
        let class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wnd_proc),
            hInstance: instance,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        // 作り直し（前の回の生成に失敗した後）では登録済みで 0 が返る。本当に無ければ生成の側で失敗する
        RegisterClassExW(&class);
        let created = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            CLASS_NAME,
            w!("フルサイズプレビュー"),
            WS_POPUP,
            0,
            0,
            640,
            360,
            None,
            None,
            Some(instance),
            None,
        );
        let hwnd = match created {
            Ok(h) => h,
            Err(e) => {
                tracing::error!("FullSizePreview_H: CreateWindowExW 失敗: {e}");
                let _ = tx.send(false);
                return;
            }
        };
        let raw = hwnd.0 as isize;
        if !PREVIEW.window.publish(raw) {
            // 作っている間に終了に入った。`shutdown` はこのウィンドウを知らないので、自分で壊して抜ける
            let _ = DestroyWindow(hwnd);
            let _ = tx.send(false);
            return;
        }
        let _ = tx.send(true);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        PREVIEW.window.clear(raw);
    }
}

/// 本体のメインウィンドウがあるモニターの矩形（このスレッドの DPI 文脈で実ピクセル）。
fn target_monitor_rect() -> Option<RECT> {
    let host = HOST_HWND.load(Ordering::Relaxed);
    if host == 0 {
        return None;
    }
    unsafe {
        let monitor = MonitorFromWindow(HWND(host as _), MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        GetMonitorInfoW(monitor, &mut info).as_bool().then_some(info.rcMonitor)
    }
}

/// ウィンドウの矩形。大きさはシーンの解像度とモニターの小さい方（縦横比を保つ。拡大はしない）、位置はモニターの中央。
fn window_rect(monitor: RECT, (scene_w, scene_h): (i32, i32)) -> RECT {
    let mw = monitor.right - monitor.left;
    let mh = monitor.bottom - monitor.top;
    let (w, h) = if scene_w <= 0 || scene_h <= 0 {
        (mw, mh)
    } else {
        let s = (mw as f64 / scene_w as f64).min(mh as f64 / scene_h as f64).min(1.0);
        (
            ((scene_w as f64 * s).round() as i32).clamp(1, mw),
            ((scene_h as f64 * s).round() as i32).clamp(1, mh),
        )
    };
    let left = monitor.left + (mw - w) / 2;
    let top = monitor.top + (mh - h) / 2;
    RECT { left, top, right: left + w, bottom: top + h }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_APP_SHOW => {
            if let Some(m) = target_monitor_rect() {
                let scene = *PREVIEW.scene_size.lock();
                let r = window_rect(m, scene);
                unsafe {
                    let _ = SetWindowPos(
                        hwnd,
                        Some(HWND_TOPMOST),
                        r.left,
                        r.top,
                        r.right - r.left,
                        r.bottom - r.top,
                        SWP_NOACTIVATE | SWP_SHOWWINDOW,
                    );
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            } else {
                tracing::warn!("FullSizePreview_H: 本体のウィンドウのモニターが分からないため表示できません");
            }
            LRESULT(0)
        }
        WM_APP_HIDE => {
            unsafe {
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
            LRESULT(0)
        }
        WM_APP_FOREGROUND => {
            // 本体以外のアプリが前に出たら、最前面をやめてそのアプリのすぐ後ろへ下がる（Alt+Tab で移った先を覆わない）。
            // 本体へ戻ったら最前面に戻す。どちらもフォーカスは取らない
            unsafe {
                if IsWindowVisible(hwnd).as_bool() {
                    let after = if wparam.0 != 0 { HWND_TOPMOST } else { HWND(lparam.0 as _) };
                    let _ = SetWindowPos(hwnd, Some(after), 0, 0, 0, 0, SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE);
                }
            }
            LRESULT(0)
        }
        WM_APP_QUIT => {
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        // フォーカスを取らない（空白キーを本体に届け続ける）
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        // クリックで閉じる（再生は続く）。別のスレッドで閉じるのは、描画スレッドの join をこのスレッドで待たないため。
        // そのスレッドは `shutdown` で終わりを待つ（`request_stop`）
        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_CLOSE => {
            request_stop();
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            unsafe { paint(hwnd) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

unsafe fn paint(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = unsafe { BeginPaint(hwnd, &mut ps) };
    let mut client = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut client);
    }
    let cw = client.right - client.left;
    let ch = client.bottom - client.top;
    let black = HBRUSH(unsafe { GetStockObject(BLACK_BRUSH) }.0);
    let frame = PREVIEW.frame.lock().clone();

    match frame {
        Some(f) if cw > 0 && ch > 0 => {
            // 等倍。はみ出すときだけ縦横比を保って縮める
            let (dw, dh) = if f.width <= cw && f.height <= ch {
                (f.width, f.height)
            } else {
                let s = (cw as f64 / f.width as f64).min(ch as f64 / f.height as f64);
                (((f.width as f64 * s).round() as i32).max(1), ((f.height as f64 * s).round() as i32).max(1))
            };
            let x = (cw - dw) / 2;
            let y = (ch - dh) / 2;
            // 画像の外側だけを黒で塗る（全面を塗ってから描くとちらつく）
            for r in [
                RECT { left: 0, top: 0, right: cw, bottom: y },
                RECT { left: 0, top: y + dh, right: cw, bottom: ch },
                RECT { left: 0, top: y, right: x, bottom: y + dh },
                RECT { left: x + dw, top: y, right: cw, bottom: y + dh },
            ] {
                if r.right > r.left && r.bottom > r.top {
                    unsafe { FillRect(hdc, &r, black) };
                }
            }
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: f.width,
                    biHeight: -f.height, // 上から下
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            unsafe {
                if dw != f.width {
                    // ふつうは表示スレッドがウィンドウの大きさに縮めてある（等倍で描く）。ずれたときだけここで縮める。
                    // HALFTONE は 4K→FHD で 1 枚 32ms かかるので使わない（机上 `cost_tests.rs`。COLORONCOLOR は 2.5ms）
                    SetStretchBltMode(hdc, COLORONCOLOR);
                }
                StretchDIBits(
                    hdc,
                    x,
                    y,
                    dw,
                    dh,
                    0,
                    0,
                    f.width,
                    f.height,
                    Some(f.bgra.as_ptr() as *const _),
                    &info,
                    DIB_RGB_COLORS,
                    SRCCOPY,
                );
            }
        }
        _ => unsafe {
            FillRect(hdc, &client, black);
            let text = PREVIEW.status.lock().clone();
            if !text.is_empty() {
                let mut wide: Vec<u16> = text.encode_utf16().collect();
                SetBkMode(hdc, TRANSPARENT);
                SetTextColor(hdc, COLORREF(0x00A0A0A0));
                let mut r = client;
                windows::Win32::Graphics::Gdi::DrawTextW(
                    hdc,
                    &mut wide,
                    &mut r,
                    DT_CENTER | DT_VCENTER | DT_SINGLELINE,
                );
            }
        },
    }
    unsafe {
        let _ = EndPaint(hwnd, &ps);
    }
}

/// 机上で測るテスト（`cost_tests.rs`）から呼ぶ入口。
#[cfg(test)]
pub(crate) mod bench {
    use super::*;

    #[derive(Default)]
    pub struct State {
        scaler: Scaler,
        out: Vec<u8>,
    }

    /// 表示スレッドが 1 枚ごとに行う変換と合成（`display` はウィンドウの大きさ）
    pub fn convert(state: &mut State, raw: &[u8], width: u32, height: u32, display: (usize, usize)) {
        let c = Compose { background: [3, 0, 46], ignore_alpha: false };
        let ok = compose(raw, (width as usize, height as usize), display, &c, &mut state.scaler, &mut state.out);
        assert!(ok);
        std::hint::black_box(&state.out);
    }

    pub fn read_player_config_at(path: &std::path::Path) -> crate::settings::PlayerConfig {
        crate::settings::read_player_config_at(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_change_is_the_next_frame_boundary() {
        let t0 = Instant::now();
        let clock = Clock { anchor_frame: 0, anchor_at: t0, frames_per_sec: 30.0, loop_start: 0, loop_end: 0 };
        let next = clock.next_change(t0 + Duration::from_millis(10));
        assert!(((next - t0).as_secs_f64() - 1.0 / 30.0).abs() < 1e-4);
        assert_eq!(clock.frame_at(next), 1);
        // 境目の直前なら、その境目
        let next = clock.next_change(t0 + Duration::from_millis(66));
        assert_eq!(clock.frame_at(next), 2);
        assert!(next > t0 + Duration::from_millis(66));
        // 進まない時計（fps 0）でも眠りすぎない
        let stopped = Clock { frames_per_sec: 0.0, ..clock };
        assert!(stopped.next_change(t0) <= t0 + MAX_WAIT);
    }

    #[test]
    fn join_until_hands_back_running_thread() {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let h = std::thread::spawn(move || {
            let _ = rx.recv();
        });
        let left = join_until(h, Instant::now() + Duration::from_millis(30)).expect("終わっていないので返す");
        let _ = tx.send(());
        assert!(join_until(left, Instant::now() + Duration::from_secs(5)).is_none());
    }

    #[test]
    fn workers_adopt_until_closed() {
        let workers = Workers::new();
        assert!(workers.adopt(std::thread::spawn(|| {})).is_ok());
        assert_eq!(workers.close_and_join(Instant::now() + Duration::from_secs(5)), 0);
        assert!(workers.adopt(std::thread::spawn(|| {})).is_err(), "閉じた後は預からない");
    }

    fn monitor(w: i32, h: i32) -> RECT {
        RECT { left: 100, top: 0, right: 100 + w, bottom: h }
    }

    #[test]
    fn window_matches_smaller_of_scene_and_monitor() {
        // FHD モニターに FHD のシーン → モニターいっぱい
        let r = window_rect(monitor(1920, 1080), (1920, 1080));
        assert_eq!((r.left, r.top, r.right - r.left, r.bottom - r.top), (100, 0, 1920, 1080));
        // FHD モニターに 1280x720 → 等倍、中央、余白なし
        let r = window_rect(monitor(1920, 1080), (1280, 720));
        assert_eq!((r.left, r.top, r.right - r.left, r.bottom - r.top), (420, 180, 1280, 720));
        // FHD モニターに 4K → モニターに合わせて縮める
        let r = window_rect(monitor(1920, 1080), (3840, 2160));
        assert_eq!((r.right - r.left, r.bottom - r.top), (1920, 1080));
        // FHD モニターに縦長 1080x1920 → 高さに合わせる
        let r = window_rect(monitor(1920, 1080), (1080, 1920));
        assert_eq!((r.right - r.left, r.bottom - r.top), (608, 1080));
    }

    #[test]
    fn clock_counts_frames_and_wraps() {
        let t0 = Instant::now();
        let clock = Clock { anchor_frame: 10, anchor_at: t0, frames_per_sec: 30.0, loop_start: 0, loop_end: 99 };
        assert_eq!(clock.frame_at(t0), 10);
        assert_eq!(clock.frame_at(t0 + Duration::from_millis(1000)), 40);
        // 100 フレーム目（範囲外）で先頭へ
        assert_eq!(clock.frame_at(t0 + Duration::from_millis(3000)), 0);
        // 速度 50%
        let half = Clock { frames_per_sec: 15.0, ..clock.clone() };
        assert_eq!(half.frame_at(t0 + Duration::from_millis(1000)), 25);
        // 範囲が無ければ戻さない
        let open = Clock { loop_end: 0, ..clock };
        assert_eq!(open.frame_at(t0 + Duration::from_millis(10_000)), 310);
    }

    fn queue(frames: &[usize]) -> VecDeque<Incoming> {
        let t0 = Instant::now();
        frames
            .iter()
            .enumerate()
            .map(|(i, &frame)| Incoming {
                frame,
                arrived: t0 + Duration::from_millis(i as u64),
                width: 1,
                height: 1,
                format: PixelFormat::RgbaStraight8,
                raw: vec![0; 4],
            })
            .collect()
    }

    #[test]
    fn picks_latest_frame_not_ahead_of_playback() {
        // 本体が先に描いた 12〜14 は、再生位置 11 ではまだ出さない
        let q = queue(&[9, 10, 11, 12, 13, 14]);
        assert_eq!(pick(&q, 11, 90).map(|i| q[i].frame), Some(11));
        // 本体が遅れているときは届いている一番新しいもの
        let q = queue(&[5, 6]);
        assert_eq!(pick(&q, 11, 90).map(|i| q[i].frame), Some(6));
        // まだ何も届いていない番号しかなければ出さない
        let q = queue(&[20, 21]);
        assert_eq!(pick(&q, 11, 90), None);
    }

    #[test]
    fn window_slot_publish_then_quit_hands_over_window() {
        let slot = WindowSlot::new();
        assert!(slot.publish(42));
        assert_eq!(slot.get(), Some(42));
        // shutdown はウィンドウを受け取って WM_APP_QUIT を送る
        assert_eq!(slot.request_quit(), Some(42));
        slot.clear(42);
        assert_eq!(slot.get(), None);
    }

    #[test]
    fn window_slot_late_window_after_quit_is_refused() {
        // 生成が時間切れの後に遅れてできた: shutdown はウィンドウを知らないまま印を立てる
        let slot = WindowSlot::new();
        assert_eq!(slot.request_quit(), None);
        assert!(slot.is_quitting());
        // ウィンドウのスレッドは載せられず、自分で壊して抜ける
        assert!(!slot.publish(7));
        assert_eq!(slot.get(), None);
    }

    #[test]
    fn window_slot_clear_keeps_other_window() {
        let slot = WindowSlot::new();
        assert!(slot.publish(5));
        slot.clear(6);
        assert_eq!(slot.get(), Some(5));
    }

    #[test]
    fn window_slot_race_never_loses_window() {
        // 載せるのと終了の印が行き違っても、「載った」のに shutdown が受け取れない組み合わせは起きない
        for _ in 0..2000 {
            let slot = Arc::new(WindowSlot::new());
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let (s, b) = (slot.clone(), barrier.clone());
            let publisher = std::thread::spawn(move || {
                b.wait();
                s.publish(99)
            });
            barrier.wait();
            let seen = slot.request_quit();
            let published = publisher.join().unwrap();
            assert!(!published || seen == Some(99), "published={published} seen={seen:?}");
        }
    }

    #[test]
    fn workers_join_spawned_threads() {
        let workers = Workers::new();
        let done = Arc::new(AtomicBool::new(false));
        let d = done.clone();
        assert!(workers.spawn("test", move || {
            std::thread::sleep(Duration::from_millis(50));
            d.store(true, Ordering::SeqCst);
        }));
        assert_eq!(workers.close_and_join(Instant::now() + Duration::from_secs(5)), 0);
        assert!(done.load(Ordering::SeqCst), "終わりを待ってから返る");
    }

    #[test]
    fn workers_refuse_after_close() {
        let workers = Workers::new();
        assert_eq!(workers.close_and_join(Instant::now()), 0);
        let ran = Arc::new(AtomicBool::new(false));
        let r = ran.clone();
        assert!(!workers.spawn("test", move || r.store(true, Ordering::SeqCst)));
        std::thread::sleep(Duration::from_millis(20));
        assert!(!ran.load(Ordering::SeqCst));
        assert_eq!(workers.len(), 0);
    }

    #[test]
    fn workers_report_threads_that_do_not_stop() {
        let workers = Workers::new();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        assert!(workers.spawn("test", move || {
            let _ = release_rx.recv();
        }));
        let started = Instant::now();
        assert_eq!(workers.close_and_join(started + Duration::from_millis(100)), 1);
        assert!(started.elapsed() < Duration::from_secs(2), "上限で打ち切る");
        let _ = release_tx.send(());
    }

    #[test]
    fn workers_reap_finished_threads() {
        let workers = Workers::new();
        for _ in 0..5 {
            assert!(workers.spawn("test", || {}));
            std::thread::sleep(Duration::from_millis(20));
        }
        // 終わったものは次に起こすときに片付く（溜まり続けない）
        assert!(workers.len() <= 2, "len={}", workers.len());
        assert_eq!(workers.close_and_join(Instant::now() + Duration::from_secs(5)), 0);
    }

    #[test]
    fn ignores_frames_after_loop_wrap() {
        // 繰り返し再生の終端（1580）で、本体が折り返し後の 0〜5 を先に描いている
        let q = queue(&[1578, 1579, 0, 1, 2, 3, 4, 5]);
        assert_eq!(pick(&q, 1580, 90).map(|i| q[i].frame), Some(1579));
        // 折り返した後は 0〜5 から選ぶ
        let q = queue(&[0, 1, 2, 3, 4, 5]);
        assert_eq!(pick(&q, 2, 90).map(|i| q[i].frame), Some(2));
    }
}
