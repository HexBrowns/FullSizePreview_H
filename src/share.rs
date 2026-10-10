//! 再生中のフレームの受け渡し（FullSizePreview_H の受け口 → VideoScope_H）。
//!
//! **同じ内容のファイルが 2 つある**: `AI/plugins/FullSizePreview_H/src/share.rs` と `AI/plugins/VideoScope_H/src/share.rs`。
//! 直すときは両方を同じにする（公開リポジトリが別々なので、共通のクレートにしていない）。配置は `layout_is_fixed` のテストで固定する。
//!
//! - 同じプロセスの中の名前付きの共有メモリ（`NAME`）。どちらの DLL も、先に開いた方が作り、後の方は同じものを開く
//! - 欲しい側（VideoScope_H）は「この時刻まで欲しい」（`want_until`、`GetTickCount64` のミリ秒）と、欲しい間隔を書く
//! - 受け口（FullSizePreview_H の `proc_video`）は、期限の内で間隔が空いていれば、フレームバッファを読んで書く。
//!   自分のウィンドウが閉じていても書く。期限が切れていれば読まない（置いたままでも重さは変わらない）
//! - 書いている途中を読まないように、番号（`seq`）を書く前と後に 1 ずつ進める（奇数の間は書いている途中）。
//!   読む側は前後で番号が同じで偶数のときだけ使う
//! - 画素は R,G,B,A 各 8bit・ストレートα・行詰め（受け口が `get_image_resource_data` で読んだまま）。
//!   `MAX_PIXELS` より大きいシーンは、縦横を同じ割合で間引いて入れる（スコープは間引いて測るので足りる）

use std::sync::atomic::{fence, AtomicU32, AtomicU64, Ordering};
use std::sync::OnceLock;

pub const NAME: &str = "Local\\HexBrowns.PreviewFrame.v1";
pub const MAGIC: u32 = u32::from_le_bytes(*b"HXPF");
pub const VERSION: u32 = 1;
pub const HEADER: usize = 64;
/// 入れられる画素の数（FHD）
pub const MAX_PIXELS: usize = 1920 * 1080;
pub const SIZE: usize = HEADER + MAX_PIXELS * 4;

const OFF_MAGIC: usize = 0;
const OFF_VERSION: usize = 4;
const OFF_SEQ: usize = 8;
const OFF_WANT: usize = 16;
const OFF_INTERVAL: usize = 24;
const OFF_FRAME: usize = 28;
const OFF_WIDTH: usize = 32;
const OFF_HEIGHT: usize = 36;
const OFF_SRC_WIDTH: usize = 40;
const OFF_SRC_HEIGHT: usize = 44;
const OFF_PUBLISHED: usize = 48;
const OFF_READ_US: usize = 56;

/// 受け取った 1 枚の情報
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameInfo {
    pub seq: u32,
    /// シーンの先頭からのフレーム番号
    pub frame: u32,
    /// 入っている画素の大きさ（間引いた後）
    pub width: u32,
    pub height: u32,
    /// シーンの大きさ（間引く前）
    pub src_width: u32,
    pub src_height: u32,
    /// 受け口が書いた時刻（`GetTickCount64`）
    pub published_ms: u64,
    /// 受け口がフレームバッファを読むのにかかった時間
    pub readback_us: u32,
}

/// 共有メモリの 1 区画（先頭が 8 の倍数に揃っていること）
pub struct Region {
    ptr: *mut u8,
    len: usize,
}

// 中身は番号（seq）で守る。ポインタはプロセスが終わるまで有効
unsafe impl Send for Region {}
unsafe impl Sync for Region {}

impl Region {
    /// # Safety
    /// `ptr` から `len` バイトが読み書きでき、`Region` より長く生きること。`ptr` は 8 の倍数に揃っていること
    pub unsafe fn from_raw(ptr: *mut u8, len: usize) -> Option<Region> {
        if ptr.is_null() || len < SIZE || (ptr as usize) % 8 != 0 {
            return None;
        }
        let r = Region { ptr, len };
        // 作ったばかりは 0。どちらが先でも同じ値を書くので、重なっても困らない
        if r.u32_at(OFF_MAGIC).load(Ordering::Acquire) != MAGIC {
            r.u32_at(OFF_VERSION).store(VERSION, Ordering::Relaxed);
            r.u32_at(OFF_MAGIC).store(MAGIC, Ordering::Release);
        }
        Some(r)
    }

    fn u32_at(&self, off: usize) -> &AtomicU32 {
        debug_assert!(off + 4 <= HEADER && off % 4 == 0);
        unsafe { &*(self.ptr.add(off) as *const AtomicU32) }
    }

    fn u64_at(&self, off: usize) -> &AtomicU64 {
        debug_assert!(off + 8 <= HEADER && off % 8 == 0);
        unsafe { &*(self.ptr.add(off) as *const AtomicU64) }
    }

    fn compatible(&self) -> bool {
        self.u32_at(OFF_MAGIC).load(Ordering::Acquire) == MAGIC && self.u32_at(OFF_VERSION).load(Ordering::Relaxed) == VERSION
    }

    // ---- 欲しい側（VideoScope_H）

    /// `now_ms` から `hold_ms` の間、`interval_ms` ごとに 1 枚欲しい
    pub fn request(&self, now_ms: u64, hold_ms: u64, interval_ms: u32) {
        self.u32_at(OFF_INTERVAL).store(interval_ms, Ordering::Relaxed);
        self.u64_at(OFF_WANT).store(now_ms + hold_ms, Ordering::Release);
    }

    /// 欲しいのをやめる
    pub fn cancel(&self) {
        self.u64_at(OFF_WANT).store(0, Ordering::Release);
    }

    /// 番号が `last_seq` と違う新しい 1 枚があれば `out` に写す（書いている途中なら None）
    pub fn read(&self, last_seq: u32, out: &mut Vec<u8>) -> Option<FrameInfo> {
        if !self.compatible() {
            return None;
        }
        let s1 = self.u32_at(OFF_SEQ).load(Ordering::Acquire);
        if s1 == 0 || s1 % 2 == 1 || s1 == last_seq {
            return None;
        }
        let info = FrameInfo {
            seq: s1,
            frame: self.u32_at(OFF_FRAME).load(Ordering::Relaxed),
            width: self.u32_at(OFF_WIDTH).load(Ordering::Relaxed),
            height: self.u32_at(OFF_HEIGHT).load(Ordering::Relaxed),
            src_width: self.u32_at(OFF_SRC_WIDTH).load(Ordering::Relaxed),
            src_height: self.u32_at(OFF_SRC_HEIGHT).load(Ordering::Relaxed),
            published_ms: self.u64_at(OFF_PUBLISHED).load(Ordering::Relaxed),
            readback_us: self.u32_at(OFF_READ_US).load(Ordering::Relaxed),
        };
        let n = info.width as usize * info.height as usize;
        if n == 0 || n > MAX_PIXELS || HEADER + n * 4 > self.len {
            return None;
        }
        out.resize(n * 4, 0);
        // 書き手と重なっていたら、後の番号の確かめで捨てる（seqlock）
        unsafe { std::ptr::copy_nonoverlapping(self.ptr.add(HEADER), out.as_mut_ptr(), n * 4) };
        fence(Ordering::Acquire);
        let s2 = self.u32_at(OFF_SEQ).load(Ordering::Relaxed);
        (s1 == s2).then_some(info)
    }

    // ---- 受け口（FullSizePreview_H）

    /// 今欲しがられていて、前に書いてから間隔が空いているか
    pub fn due(&self, now_ms: u64) -> bool {
        if !self.compatible() || self.u64_at(OFF_WANT).load(Ordering::Acquire) <= now_ms {
            return false;
        }
        let interval = self.u32_at(OFF_INTERVAL).load(Ordering::Relaxed) as u64;
        let published = self.u64_at(OFF_PUBLISHED).load(Ordering::Relaxed);
        published == 0 || now_ms.saturating_sub(published) >= interval
    }

    /// 1 枚書く。`rgba` は `src_width`×`src_height` の行詰め（R,G,B,A 各 8bit・ストレートα）
    pub fn publish(&self, frame: u32, src_width: u32, src_height: u32, rgba: &[u8], now_ms: u64, readback_us: u32) -> bool {
        let (sw, sh) = (src_width as usize, src_height as usize);
        if !self.compatible() || sw == 0 || sh == 0 || rgba.len() < sw * sh * 4 {
            return false;
        }
        let step = decimation(sw, sh);
        let (w, h) = (sw.div_ceil(step), sh.div_ceil(step));
        let seq = self.u32_at(OFF_SEQ);
        // 奇数にしてから書く
        let start = seq.load(Ordering::Relaxed);
        let start = if start % 2 == 1 { start.wrapping_add(1) } else { start };
        seq.store(start.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        self.u32_at(OFF_FRAME).store(frame, Ordering::Relaxed);
        self.u32_at(OFF_WIDTH).store(w as u32, Ordering::Relaxed);
        self.u32_at(OFF_HEIGHT).store(h as u32, Ordering::Relaxed);
        self.u32_at(OFF_SRC_WIDTH).store(src_width, Ordering::Relaxed);
        self.u32_at(OFF_SRC_HEIGHT).store(src_height, Ordering::Relaxed);
        self.u64_at(OFF_PUBLISHED).store(now_ms.max(1), Ordering::Relaxed);
        self.u32_at(OFF_READ_US).store(readback_us, Ordering::Relaxed);
        let dst = unsafe { std::slice::from_raw_parts_mut(self.ptr.add(HEADER), w * h * 4) };
        if step == 1 {
            dst.copy_from_slice(&rgba[..w * h * 4]);
        } else {
            for (oy, y) in (0..sh).step_by(step).enumerate() {
                let row = &rgba[y * sw * 4..(y + 1) * sw * 4];
                let drow = &mut dst[oy * w * 4..(oy + 1) * w * 4];
                for (ox, x) in (0..sw).step_by(step).enumerate() {
                    drow[ox * 4..ox * 4 + 4].copy_from_slice(&row[x * 4..x * 4 + 4]);
                }
            }
        }
        // 偶数（0 は「まだ無い」なので飛ばす）に戻して書き終わり
        let mut end = start.wrapping_add(2);
        if end == 0 {
            end = 2;
        }
        seq.store(end, Ordering::Release);
        true
    }
}

/// `MAX_PIXELS` に収まる、縦横共通の最小の間引き
pub fn decimation(width: usize, height: usize) -> usize {
    let mut step = 1;
    while width.div_ceil(step) * height.div_ceil(step) > MAX_PIXELS {
        step += 1;
    }
    step
}

/// `GetTickCount64`（プロセスの中のどの DLL からも同じ時計）
pub fn now_ms() -> u64 {
    unsafe { windows::Win32::System::SystemInformation::GetTickCount64() }
}

static REGION: OnceLock<Option<Region>> = OnceLock::new();

/// 共有メモリを開く（無ければ作る）。失敗したら None（受け渡しをしないだけ）
pub fn region() -> Option<&'static Region> {
    REGION.get_or_init(open).as_ref()
}

fn open() -> Option<Region> {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows::Win32::System::Memory::{CreateFileMappingW, MapViewOfFile, FILE_MAP_ALL_ACCESS, PAGE_READWRITE};
    unsafe {
        let name = HSTRING::from(NAME);
        let mapping = match CreateFileMappingW(INVALID_HANDLE_VALUE, None, PAGE_READWRITE, 0, SIZE as u32, &name) {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!("再生中の受け渡しの共有メモリを作れませんでした: {e}");
                return None;
            }
        };
        // ハンドルは閉じない（プロセスが終わるまで使う。閉じても見える範囲は残るが、作り直しを避ける）
        let view = MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, SIZE);
        if view.Value.is_null() {
            tracing::warn!("再生中の受け渡しの共有メモリを開けませんでした");
            return None;
        }
        Region::from_raw(view.Value as *mut u8, SIZE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region_in(buf: &mut Vec<u64>) -> Region {
        unsafe { Region::from_raw(buf.as_mut_ptr() as *mut u8, buf.len() * 8).unwrap() }
    }

    fn buf() -> Vec<u64> {
        vec![0u64; SIZE.div_ceil(8)]
    }

    #[test]
    fn layout_is_fixed() {
        // 両方の DLL で同じ数字であること（FullSizePreview_H と VideoScope_H の share.rs で同じテスト）
        assert_eq!(NAME, "Local\\HexBrowns.PreviewFrame.v1");
        assert_eq!((MAGIC, VERSION, HEADER, MAX_PIXELS), (0x4650_5848, 1, 64, 2_073_600));
        assert_eq!(
            [OFF_MAGIC, OFF_VERSION, OFF_SEQ, OFF_WANT, OFF_INTERVAL, OFF_FRAME, OFF_WIDTH, OFF_HEIGHT, OFF_SRC_WIDTH, OFF_SRC_HEIGHT, OFF_PUBLISHED, OFF_READ_US],
            [0, 4, 8, 16, 24, 28, 32, 36, 40, 44, 48, 56]
        );
    }

    #[test]
    fn roundtrip() {
        let mut b = buf();
        let r = region_in(&mut b);
        let rgba: Vec<u8> = (0..4 * 3 * 4).map(|i| i as u8).collect();
        assert!(!r.due(1000), "誰も欲しがっていなければ書かない");
        r.request(1000, 600, 100);
        assert!(r.due(1000));
        assert!(r.publish(42, 4, 3, &rgba, 1000, 2500));
        let mut out = Vec::new();
        let info = r.read(0, &mut out).unwrap();
        assert_eq!((info.frame, info.width, info.height, info.src_width, info.readback_us), (42, 4, 3, 4, 2500));
        assert_eq!(out, rgba);
        assert!(r.read(info.seq, &mut out).is_none(), "同じ番号は 2 度読まない");
        // 間隔が空くまでは書かない。期限が切れたら書かない
        assert!(!r.due(1050));
        assert!(r.due(1100));
        assert!(!r.due(1600));
        r.request(1600, 600, 100);
        r.cancel();
        assert!(!r.due(1700));
    }

    #[test]
    fn torn_write_is_rejected() {
        let mut b = buf();
        let r = region_in(&mut b);
        r.request(0, 1000, 0);
        assert!(r.publish(1, 2, 2, &[7u8; 16], 10, 0));
        // 書いている途中（奇数）を装う
        let seq = r.u32_at(OFF_SEQ);
        let s = seq.load(Ordering::Relaxed);
        seq.store(s + 1, Ordering::Relaxed);
        let mut out = Vec::new();
        assert!(r.read(0, &mut out).is_none(), "奇数の間は読まない");
        seq.store(s + 2, Ordering::Relaxed);
        assert!(r.read(0, &mut out).is_some());
    }

    #[test]
    fn large_scene_is_decimated() {
        assert_eq!(decimation(1920, 1080), 1);
        assert_eq!(decimation(1921, 1080), 2);
        assert_eq!(decimation(3840, 2160), 2);
        let (w, h) = (3840usize, 2160usize);
        let mut rgba = vec![0u8; w * h * 4];
        // (2,2) の画素だけ赤。間引くと (1,1) に来る
        let i = (2 * w + 2) * 4;
        rgba[i..i + 4].copy_from_slice(&[255, 0, 0, 255]);
        let mut b = buf();
        let r = region_in(&mut b);
        assert!(r.publish(0, w as u32, h as u32, &rgba, 5, 0));
        let mut out = Vec::new();
        let info = r.read(0, &mut out).unwrap();
        assert_eq!((info.width, info.height, info.src_width, info.src_height), (1920, 1080, 3840, 2160));
        let j = (1920 + 1) * 4;
        assert_eq!(&out[j..j + 4], &[255, 0, 0, 255]);
    }

    #[test]
    fn rejects_short_or_foreign_memory() {
        let mut small = vec![0u64; 8];
        assert!(unsafe { Region::from_raw(small.as_mut_ptr() as *mut u8, 64) }.is_none());
        let mut b = buf();
        let r = region_in(&mut b);
        r.u32_at(OFF_VERSION).store(99, Ordering::Relaxed);
        r.request(0, 1000, 0);
        assert!(!r.due(1), "版が違えば書かない");
        assert!(!r.publish(0, 1, 1, &[0; 4], 1, 0));
    }
}
