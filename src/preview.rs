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
//!   「モニター」の小さい方（縦横比を保つ）。シーンの方が小さければ等倍で、余白を作らない

use std::collections::VecDeque;
use std::sync::atomic::{AtomicIsize, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, EndPaint, FillRect, GetMonitorInfoW, GetStockObject, InvalidateRect,
    MonitorFromWindow, SetBkMode, SetBrushOrgEx, SetStretchBltMode, SetTextColor, StretchDIBits,
    BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLACK_BRUSH, DIB_RGB_COLORS, DT_CENTER, DT_SINGLELINE,
    DT_VCENTER, HALFTONE, HBRUSH, MONITORINFO, MONITOR_DEFAULTTONEAREST, PAINTSTRUCT, SRCCOPY,
    TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, GetMessageW,
    LoadCursorW, PostMessageW, PostQuitMessage, RegisterClassExW, SetWindowPos, ShowWindow,
    TranslateMessage, HWND_TOPMOST, IDC_ARROW, MA_NOACTIVATE, MSG, SWP_NOACTIVATE, SWP_SHOWWINDOW,
    SW_HIDE, WM_APP, WM_CLOSE, WM_DESTROY, WM_ERASEBKGND, WM_LBUTTONDOWN, WM_MOUSEACTIVATE,
    WM_PAINT, WM_RBUTTONDOWN, WNDCLASSEXW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_POPUP,
};

use crate::readback::{PixelFormat, Readback};
use crate::input::HOST_HWND;
use crate::receiver::RECEIVER_NAME;
use crate::EDIT_HANDLE;

const WM_APP_SHOW: u32 = WM_APP + 1;
const WM_APP_HIDE: u32 = WM_APP + 2;
const WM_APP_QUIT: u32 = WM_APP + 3;

/// 届いたフレームを溜めておく上限（枚数と合計バイト）。本体の再生バッファ（既定 500ms）より多めに取る
const QUEUE_MAX_FRAMES: usize = 30;
const QUEUE_MAX_BYTES: usize = 400 * 1024 * 1024;
/// これより古く届いたものは捨てる（繰り返し再生の折り返しで残った先の番号など）
const QUEUE_MAX_AGE: Duration = Duration::from_millis(2000);
/// 再生位置よりこれだけ（秒）以上古いフレームは、折り返しの後の番号とみなして選ばない
const PAST_WINDOW_SEC: f64 = 3.0;
/// 最初からこの枚数続けて空（全画素 0）なら、受け口がカメラ制御の対象になっているとみて案内する
const EMPTY_FRAMES_BEFORE_NOTICE: u32 = 8;

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

#[derive(Default)]
struct Stats {
    received: u64,
    shown: u64,
    /// 届いた時点の再生位置との差（届いたフレーム − 再生位置）。正なら本体が先に描いている
    lead_sum: f64,
    lead_min: i64,
    lead_max: i64,
    /// 受け口での読み出し（GPU → CPU）にかかった時間
    readback_ms_sum: f64,
    readback_ms_max: f64,
    receivers: Vec<i64>,
}

struct Preview {
    hwnd: AtomicIsize,
    window_thread: Mutex<Option<JoinHandle<()>>>,
    display_thread: Mutex<Option<JoinHandle<()>>>,
    /// 表示中のセッション番号（0 = 閉じている）。開くたびに増える
    session: AtomicU64,
    next_session: AtomicU64,
    clock: Mutex<Option<Clock>>,
    queue: Mutex<VecDeque<Incoming>>,
    /// 使い終わった画素バッファ（受け口で毎フレーム 8MB を確保し直さないため）
    pool: Mutex<Vec<Vec<u8>>>,
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
    hwnd: AtomicIsize::new(0),
    window_thread: Mutex::new(None),
    display_thread: Mutex::new(None),
    session: AtomicU64::new(0),
    next_session: AtomicU64::new(0),
    clock: Mutex::new(None),
    queue: Mutex::new(VecDeque::new()),
    pool: Mutex::new(Vec::new()),
    frame: Mutex::new(None),
    status: Mutex::new(String::new()),
    stats: Mutex::new(Stats {
        received: 0,
        shown: 0,
        lead_sum: 0.0,
        lead_min: 0,
        lead_max: 0,
        readback_ms_sum: 0.0,
        readback_ms_max: 0.0,
        receivers: Vec::new(),
    }),
    started_at: Mutex::new(None),
    scene_size: Mutex::new((0, 0)),
    background: Mutex::new([0, 0, 0]),
    control: Mutex::new(()),
};

fn hwnd() -> Option<HWND> {
    match PREVIEW.hwnd.load(Ordering::Acquire) {
        0 => None,
        h => Some(HWND(h as _)),
    }
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

/// 受け口が画像を読み出すべきか（空白キーで再生している間だけ）。
pub fn wants_frames() -> bool {
    is_open()
}

fn recycle(buffer: Vec<u8>) {
    let mut pool = PREVIEW.pool.lock();
    if pool.len() < 8 {
        pool.push(buffer);
    }
}

/// 受け口が読み出しに使うバッファ（使い回し）。
pub fn take_buffer() -> Vec<u8> {
    PREVIEW.pool.lock().pop().unwrap_or_default()
}

/// 読み出しに失敗したバッファを返す。
pub fn give_back(buffer: Vec<u8>) {
    recycle(buffer);
}

/// 受け口（本体の描画スレッド）から呼ぶ。読み出したフレームを溜めておく。
pub fn push(effect_id: i64, frame: usize, rb: Readback, raw: Vec<u8>, readback: Duration) {
    if !is_open() || rb.width == 0 || rb.height == 0 {
        recycle(raw);
        return;
    }
    let readback_ms = readback.as_secs_f64() * 1000.0;
    let arrived = Instant::now();

    let target = PREVIEW.clock.lock().as_ref().map(|c| c.frame_at(arrived));
    {
        let mut stats = PREVIEW.stats.lock();
        stats.received += 1;
        stats.readback_ms_sum += readback_ms;
        stats.readback_ms_max = stats.readback_ms_max.max(readback_ms);
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

/// ウィンドウのスレッドを立てる（1 回だけ）。
fn ensure_window() -> Option<HWND> {
    if let Some(h) = hwnd() {
        return Some(h);
    }
    let mut guard = PREVIEW.window_thread.lock();
    if let Some(h) = hwnd() {
        return Some(h);
    }
    let (tx, rx) = std::sync::mpsc::channel::<isize>();
    let handle = std::thread::Builder::new()
        .name("FullSizePreview_H window".into())
        .spawn(move || window_thread(tx))
        .ok()?;
    *guard = Some(handle);
    match rx.recv_timeout(Duration::from_secs(3)) {
        Ok(h) if h != 0 => {
            PREVIEW.hwnd.store(h, Ordering::Release);
            Some(HWND(h as _))
        }
        _ => {
            tracing::error!("FullSizePreview_H: プレビューウィンドウを作れませんでした");
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
    *PREVIEW.stats.lock() = Stats::default();
    *PREVIEW.started_at.lock() = Some(started);
    *PREVIEW.scene_size.lock() = (info.width as i32, info.height as i32);
    *PREVIEW.background.lock() = info.background.map(|(r, g, b)| [r, g, b]).unwrap_or([0, 0, 0]);
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

fn stop_locked() {
    let session = PREVIEW.session.swap(0, Ordering::AcqRel);
    if let Some(h) = hwnd() {
        unsafe {
            let _ = PostMessageW(Some(h), WM_APP_HIDE, WPARAM(0), LPARAM(0));
        }
    }
    if let Some(t) = PREVIEW.display_thread.lock().take() {
        let _ = t.join();
    }
    *PREVIEW.clock.lock() = None;
    for q in PREVIEW.queue.lock().drain(..) {
        recycle(q.raw);
    }
    if session == 0 {
        return;
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
        return;
    }
    let per_sec = |n: u64| if elapsed > 0.0 { n as f64 / elapsed } else { 0.0 };
    let avg = |sum: f64| if stats.received > 0 { sum / stats.received as f64 } else { 0.0 };
    tracing::info!(
        "FullSizePreview_H: 閉じました。{elapsed:.1} 秒 / 受け取り {} 枚（{:.1} fps）/ 表示 {} 枚（{:.1} fps）/ 読み出し 平均 {:.1}ms 最大 {:.1}ms / 再生位置との差 平均 {:.1} 最小 {} 最大 {} フレーム / 受け口 {} 個",
        stats.received,
        per_sec(stats.received),
        stats.shown,
        per_sec(stats.shown),
        avg(stats.readback_ms_sum),
        stats.readback_ms_max,
        avg(stats.lead_sum),
        stats.lead_min,
        stats.lead_max,
        stats.receivers.len()
    );
}

/// プラグインの解放時に呼ぶ。スレッドを止める。
pub fn shutdown() {
    stop();
    if let Some(h) = hwnd() {
        unsafe {
            let _ = PostMessageW(Some(h), WM_APP_QUIT, WPARAM(0), LPARAM(0));
        }
    }
    if let Some(t) = PREVIEW.window_thread.lock().take() {
        let _ = t.join();
    }
    PREVIEW.hwnd.store(0, Ordering::Release);
}

/// 再生位置が変わるたびに、溜めたフレームから合うものを選んで出す。
fn display_loop(session: u64) {
    let Some(mut clock) = PREVIEW.clock.lock().clone() else {
        return;
    };
    let past_window = (clock.frames_per_sec * PAST_WINDOW_SEC).ceil() as usize;
    // 再生中に本体のカーソルが動いたら（シーク等）、そこから数え直す
    let mut cursor = clock.anchor_frame;
    let mut shown: Option<usize> = None;
    let mut compose = Compose { background: *PREVIEW.background.lock(), ignore_alpha: false };
    let mut premul: Vec<u8> = Vec::new();
    // 空でない 1 枚が来るまで、届いた画像が空かどうかを調べる
    let mut saw_content = false;
    let mut empty_frames: u32 = 0;

    while PREVIEW.session.load(Ordering::Acquire) == session {
        let now = Instant::now();
        let edit_frame = EDIT_HANDLE.get_edit_info().frame;
        if edit_frame != cursor {
            tracing::info!("FullSizePreview_H: 再生中にカーソルが {cursor} から {edit_frame} へ動いたので、そこから数え直します");
            cursor = edit_frame;
            clock.anchor_frame = edit_frame;
            clock.anchor_at = now;
            *PREVIEW.clock.lock() = Some(clock.clone());
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
                _ => None,
            }
        };

        if let Some(c) = chosen {
            to_premul_rgba8(&c.raw, c.width, c.height, c.format, &mut premul);
            if shown.is_none() {
                let px = analyze(&premul);
                // αが全面 0 なのに色がある場合は、αを当てにせず不透明として出す
                compose.ignore_alpha = px.alpha_max == 0 && px.rgb_max > 0;
                tracing::info!(
                    "FullSizePreview_H: 最初の 1 枚（フレーム {} / {}x{} / {:?}）: α 最小 {} 最大 {} 平均 {:.1} / 色の最大 {} / 色のある画素 {} / 中央の画素 {:?} / 背景色 {:?} / αを無視: {}",
                    c.frame,
                    c.width,
                    c.height,
                    c.format,
                    px.alpha_min,
                    px.alpha_max,
                    px.alpha_mean,
                    px.rgb_max,
                    px.colored,
                    center_pixel(&premul, c.width, c.height),
                    compose.background,
                    compose.ignore_alpha
                );
            }
            if !saw_content {
                let px = analyze(&premul);
                if px.alpha_max > 0 || px.rgb_max > 0 {
                    saw_content = true;
                } else {
                    empty_frames += 1;
                    if empty_frames == EMPTY_FRAMES_BEFORE_NOTICE {
                        // カメラ制御の対象の中では、受け口に届くのは合成前の空のバッファ（実機 2026-09-27）
                        tracing::warn!("FullSizePreview_H: {RECEIVER_NAME}に届く画像が {empty_frames} 枚続けて空です。受け口がカメラ制御の対象になっていないか確かめてください");
                        *PREVIEW.frame.lock() = None;
                        set_status(format!(
                            "{RECEIVER_NAME}に届く画像が空です。受け口がカメラ制御の対象になっていると画面が届きません。受け口を選んで、設定欄の「カメラ制御の対象」を外してください。編集メニュー「フルサイズプレビュー受け口を置く」でも外せます（クリックで閉じます）"
                        ));
                    }
                    if empty_frames >= EMPTY_FRAMES_BEFORE_NOTICE {
                        // 案内を出したまま、空の画像では上書きしない
                        recycle(c.raw);
                        std::thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                }
            }
            if let Some(f) = composite(&premul, c.width, c.height, &compose) {
                *PREVIEW.frame.lock() = Some(Arc::new(f));
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
            }
            recycle(c.raw);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// 合成の仕方。
struct Compose {
    /// 透明部分に敷く色（R,G,B）
    background: [u8; 3],
    /// αを無視して不透明として出す
    ignore_alpha: bool,
}

/// 届いた画素の統計（切り分け用にログへ出す）。
struct PixelStats {
    alpha_min: u8,
    alpha_max: u8,
    alpha_mean: f64,
    rgb_max: u8,
    /// R,G,B のどれかが 0 でない画素の数
    colored: usize,
}

fn analyze(rgba: &[u8]) -> PixelStats {
    let mut st = PixelStats { alpha_min: 255, alpha_max: 0, alpha_mean: 0.0, rgb_max: 0, colored: 0 };
    let mut sum: u64 = 0;
    let mut n: u64 = 0;
    for p in rgba.chunks_exact(4) {
        st.alpha_min = st.alpha_min.min(p[3]);
        st.alpha_max = st.alpha_max.max(p[3]);
        sum += p[3] as u64;
        n += 1;
        let m = p[0].max(p[1]).max(p[2]);
        st.rgb_max = st.rgb_max.max(m);
        if m > 0 {
            st.colored += 1;
        }
    }
    if n == 0 {
        st.alpha_min = 0;
    } else {
        st.alpha_mean = sum as f64 / n as f64;
    }
    st
}

fn center_pixel(rgba: &[u8], width: u32, height: u32) -> [u8; 4] {
    let i = ((height as usize / 2) * width as usize + width as usize / 2) * 4;
    rgba.get(i..i + 4).map(|p| [p[0], p[1], p[2], p[3]]).unwrap_or_default()
}

/// 読み出した画素を、乗算済みの R,G,B,A（各 8bit）へ。
fn to_premul_rgba8(raw: &[u8], width: u32, height: u32, format: PixelFormat, out: &mut Vec<u8>) {
    let n = width as usize * height as usize;
    out.resize(n * 4, 0);
    let raw = &raw[..(n * format.bytes_per_pixel()).min(raw.len())];
    match format {
        PixelFormat::RgbaStraight8 => {
            for (s, d) in raw.chunks_exact(4).zip(out.chunks_exact_mut(4)) {
                let a = s[3] as u32;
                if a == 255 {
                    d.copy_from_slice(s);
                } else {
                    d[0] = ((s[0] as u32 * a + 127) / 255) as u8;
                    d[1] = ((s[1] as u32 * a + 127) / 255) as u8;
                    d[2] = ((s[2] as u32 * a + 127) / 255) as u8;
                    d[3] = s[3];
                }
            }
        }
    }
}

/// 乗算済みの R,G,B,A（各 8bit）を、背景色の上に合成した BGRA へ。
fn composite(premul: &[u8], width: u32, height: u32, compose: &Compose) -> Option<Frame> {
    let n = width as usize * height as usize;
    if n == 0 || premul.len() < n * 4 {
        return None;
    }
    let mut bgra = vec![0u8; n * 4];
    let [br, bg, bb] = compose.background;
    for (s, d) in premul.chunks_exact(4).zip(bgra.chunks_exact_mut(4)) {
        let rest = if compose.ignore_alpha { 0 } else { 255 - s[3] as u32 };
        let over = |c: u8, back: u8| (c as u32 + (back as u32 * rest + 127) / 255).min(255) as u8;
        d[0] = over(s[2], bb);
        d[1] = over(s[1], bg);
        d[2] = over(s[0], br);
        d[3] = 255;
    }
    Some(Frame { width: width as i32, height: height as i32, bgra })
}

// ---- ウィンドウ ----

fn window_thread(tx: std::sync::mpsc::Sender<isize>) {
    unsafe {
        // モニターの実ピクセルで大きさを決める（等倍表示のため）。このスレッドで作るウィンドウだけに効く
        let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        let instance = GetModuleHandleW(None).unwrap_or_default();
        let class_name = w!("FullSizePreview_H_Window");
        let class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wnd_proc),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: class_name,
            ..Default::default()
        };
        RegisterClassExW(&class);
        let created = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            class_name,
            w!("フルサイズプレビュー"),
            WS_POPUP,
            0,
            0,
            640,
            360,
            None,
            None,
            Some(instance.into()),
            None,
        );
        let hwnd = match created {
            Ok(h) => h,
            Err(e) => {
                tracing::error!("FullSizePreview_H: CreateWindowExW 失敗: {e}");
                let _ = tx.send(0);
                return;
            }
        };
        let _ = tx.send(hwnd.0 as isize);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
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
        // クリックで閉じる（再生は続く）。ワーカーで閉じるのは、描画スレッドの join をこのスレッドで待たないため
        WM_LBUTTONDOWN | WM_RBUTTONDOWN => {
            if is_open() {
                std::thread::spawn(stop);
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            if is_open() {
                std::thread::spawn(stop);
            }
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
                    SetStretchBltMode(hdc, HALFTONE);
                    let _ = SetBrushOrgEx(hdc, 0, 0, None);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_alpha_when_alpha_is_all_zero() {
        let st = analyze(&[200, 100, 50, 0, 0, 0, 0, 0]);
        assert_eq!((st.alpha_max, st.rgb_max, st.colored), (0, 200, 1));
        let opaque = Compose { background: [3, 0, 46], ignore_alpha: true };
        let f = composite(&[200, 100, 50, 0], 1, 1, &opaque).unwrap();
        assert_eq!(&f.bgra[..], &[50, 100, 200, 255]);
    }

    #[test]
    fn rejects_short_buffer() {
        assert!(composite(&[0u8; 4], 2, 1, &Compose { background: [0, 0, 0], ignore_alpha: false }).is_none());
        assert!(composite(&[], 0, 0, &Compose { background: [0, 0, 0], ignore_alpha: false }).is_none());
    }

    const BLACK: Compose = Compose { background: [0, 0, 0], ignore_alpha: false };

    #[test]
    fn converts_straight_rgba_over_background() {
        // 不透明の赤、半透明（α 128）の白、透明（色が残っていても出さない）
        let raw = [255, 0, 0, 255, 255, 255, 255, 128, 200, 100, 50, 0];
        let mut premul = Vec::new();
        to_premul_rgba8(&raw, 3, 1, PixelFormat::RgbaStraight8, &mut premul);
        assert_eq!(premul, vec![255, 0, 0, 255, 128, 128, 128, 128, 0, 0, 0, 0]);
        let f = composite(&premul, 3, 1, &BLACK).expect("合成できる");
        assert_eq!(&f.bgra[0..4], &[0, 0, 255, 255]);
        assert_eq!(&f.bgra[4..8], &[128, 128, 128, 255]);
        assert_eq!(&f.bgra[8..12], &[0, 0, 0, 255]);
        // 透明部分には背景色（R3 G0 B46）
        let blue = Compose { background: [3, 0, 46], ignore_alpha: false };
        let f = composite(&premul, 3, 1, &blue).unwrap();
        assert_eq!(&f.bgra[8..12], &[46, 0, 3, 255]);
        assert_eq!(&f.bgra[4..8], &[128 + 23, 128, 128 + 1, 255]);
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
    fn ignores_frames_after_loop_wrap() {
        // 繰り返し再生の終端（1580）で、本体が折り返し後の 0〜5 を先に描いている
        let q = queue(&[1578, 1579, 0, 1, 2, 3, 4, 5]);
        assert_eq!(pick(&q, 1580, 90).map(|i| q[i].frame), Some(1579));
        // 折り返した後は 0〜5 から選ぶ
        let q = queue(&[0, 1, 2, 3, 4, 5]);
        assert_eq!(pick(&q, 2, 90).map(|i| q[i].frame), Some(2));
    }
}
