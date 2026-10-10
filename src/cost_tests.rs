//! 机上で測る重さ（実機の要望の規模: FHD と 4K、不透明と半透明）。
//! 時間は機械で変わるので、既定では走らせない:
//!   cargo test --release -- --ignored --nocapture
//!
//! 測れないもの（本体の関数）: `get_edit_info`・`copy_image_resource`・`get_image_resource_data` は、
//! 実機のログの「閉じました。…」の行（読み出しの内訳）で取る。

use std::time::Instant;

use crate::preview::bench;
use crate::share;

/// `n` 回走らせて 1 回あたりの平均と最大（ms）
fn measure(n: usize, mut f: impl FnMut()) -> (f64, f64) {
    // 1 回目（確保・ページフォルト）は捨てる
    f();
    let mut sum = 0.0;
    let mut max: f64 = 0.0;
    for _ in 0..n {
        let t = Instant::now();
        f();
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        sum += ms;
        max = max.max(ms);
    }
    (sum / n as f64, max)
}

fn report(label: &str, (avg, max): (f64, f64)) {
    println!("[INFO] {label}: 平均 {avg:.2} ms / 最大 {max:.2} ms");
}

/// R,G,B,A 各 8bit・ストレートα。`translucent` なら α を画素ごとに変える（0〜255 を一巡）
fn frame(w: usize, h: usize, translucent: bool) -> Vec<u8> {
    let mut v = vec![0u8; w * h * 4];
    for (i, p) in v.chunks_exact_mut(4).enumerate() {
        p[0] = (i * 7) as u8;
        p[1] = (i * 13) as u8;
        p[2] = (i * 29) as u8;
        p[3] = if translucent { (i % 256) as u8 } else { 255 };
    }
    v
}

const SIZES: [(&str, usize, usize); 2] = [("FHD", 1920, 1080), ("4K", 3840, 2160)];

#[test]
#[ignore]
fn cost_convert_and_composite() {
    for (name, w, h) in SIZES {
        for translucent in [false, true] {
            let raw = frame(w, h, translucent);
            let kind = if translucent { "半透明" } else { "不透明" };
            // 表示はシーンと同じ大きさ（等倍）と、FHD のモニター（4K なら縮める）
            for display in [(w, h), (1920, 1080)] {
                let mut state = bench::State::default();
                report(
                    &format!("変換と合成 {name} {kind} → 表示 {}x{}", display.0, display.1),
                    measure(20, || bench::convert(&mut state, &raw, w as u32, h as u32, display)),
                );
                if (w, h) == (1920, 1080) {
                    break;
                }
            }
        }
    }
    // 整数分の 1 でない縮小（2560x1440 のシーンを FHD のモニターで）
    let raw = frame(2560, 1440, true);
    let mut state = bench::State::default();
    report("変換と合成 2560x1440 半透明 → 表示 1920x1080", measure(20, || bench::convert(&mut state, &raw, 2560, 1440, (1920, 1080))));
}

#[test]
#[ignore]
fn cost_share_publish() {
    let mut mem = vec![0u64; share::SIZE.div_ceil(8)];
    let region = unsafe { share::Region::from_raw(mem.as_mut_ptr() as *mut u8, mem.len() * 8).unwrap() };
    region.request(0, u64::MAX / 2, 0);
    for (name, w, h) in SIZES {
        let raw = frame(w, h, false);
        report(
            &format!("共有メモリへ書く {name}"),
            measure(20, || {
                region.publish(0, w as u32, h as u32, &raw, 1, 0);
            }),
        );
    }
}

#[test]
#[ignore]
fn cost_gdi_stretch() {
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject, SetBrushOrgEx, SetStretchBltMode,
        StretchDIBits, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, COLORONCOLOR, DIB_RGB_COLORS, HALFTONE, SRCCOPY,
        STRETCH_BLT_MODE,
    };
    let info = |w: i32, h: i32| BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    unsafe {
        let dc = CreateCompatibleDC(None);
        for (sw, sh, dw, dh) in [(1920, 1080, 1920, 1080), (3840, 2160, 1920, 1080), (2560, 1440, 1920, 1080), (3840, 2160, 2560, 1440), (1920, 1080, 1536, 864)] {
            let mut bits = std::ptr::null_mut();
            let bmp = CreateDIBSection(Some(dc), &info(dw, dh), DIB_RGB_COLORS, &mut bits, None, 0).unwrap();
            let old = SelectObject(dc, bmp.into());
            let src = frame(sw as usize, sh as usize, false);
            let src_info = info(sw, sh);
            let modes: &[(&str, STRETCH_BLT_MODE)] =
                if (sw, sh) == (dw, dh) { &[("等倍", COLORONCOLOR)] } else { &[("HALFTONE", HALFTONE), ("COLORONCOLOR", COLORONCOLOR)] };
            for &(mode_name, mode) in modes {
                report(
                    &format!("GDI {sw}x{sh}→{dw}x{dh} {mode_name}"),
                    measure(20, || {
                        SetStretchBltMode(dc, mode);
                        let _ = SetBrushOrgEx(dc, 0, 0, None);
                        StretchDIBits(dc, 0, 0, dw, dh, 0, 0, sw, sh, Some(src.as_ptr() as *const _), &src_info, DIB_RGB_COLORS, SRCCOPY);
                    }),
                );
            }
            SelectObject(dc, old);
            let _ = DeleteObject(bmp.into());
        }
        let _ = DeleteDC(dc);
    }
}

#[test]
#[ignore]
fn cost_read_player_config() {
    // 本番の aviutl2.ini と同じ並び（[Player] は 384 行目、全体 300KB）を写して測る
    let mut text = String::new();
    for i in 0..380 {
        text.push_str(&format!("Key{i}=value{i}\r\n"));
    }
    text.push_str("[Player]\r\nVolume=10\r\nSpeed=150\r\nPreviewSelectRange=1\r\n[After]\r\n");
    while text.len() < 300_000 {
        text.push_str("[Font.Dummy]\r\nVisible=1\r\n");
    }
    let path = std::env::temp_dir().join("fullsizepreview_h_cost_ini.ini");
    std::fs::write(&path, &text).unwrap();
    report("aviutl2.ini から [Player] を読む（300KB）", measure(50, || {
        let c = bench::read_player_config_at(&path);
        assert_eq!((c.speed, c.select_range), (1.5, true));
    }));
    let _ = std::fs::remove_file(&path);
}

/// 間引きの効き目の見積もり（本体の 1 フレームの描画時間と読み出しの時間を決めて、30 秒ぶん回す）。
/// 本体のモデル: フレーム n の時刻 n × 33.3ms より再生バッファ（`aviutl2.ini` の `BufferTime=500`）の分だけ先まで描いてよく、
/// 遅れて 1 フレーム以上過ぎたら、今の時刻のフレームへ飛ぶ（コマ落ち）。本体の fps は 1 秒に描いた枚数
#[test]
#[ignore]
fn cost_throttle_simulation() {
    use crate::throttle::Throttle;
    let period = 1000.0 / 30.0;
    let buffer = 500.0;
    // （場面, 本体の描画 ms, 読み出し ms）。読み出しは FHD の実機の平均 2.5〜9ms・最大 21ms、4K は FHD の 4 倍とみた
    for (label, host, read) in [
        ("軽い FHD・本体 10ms・読み出し 2.5ms", 10.0, 2.5),
        ("FHD・本体 28ms・読み出し 9ms", 28.0, 9.0),
        ("FHD・本体 30ms・読み出し 21ms", 30.0, 21.0),
        ("重い FHD・本体 45ms・読み出し 9ms", 45.0, 9.0),
        ("4K・本体 25ms・読み出し 36ms", 25.0, 36.0),
    ] {
        for throttled in [false, true] {
            let mut th = Throttle::new(30.0);
            let t0 = Instant::now();
            let (mut t, mut n) = (0.0f64, 0u64);
            let (mut rendered, mut reads) = (0u64, 0u64);
            let (from, to) = (5000.0, 35000.0);
            while t < to {
                let mut sched = n as f64 * period;
                let mut start = t.max(sched - buffer);
                if start > sched + period {
                    n = (start / period).floor() as u64;
                    sched = n as f64 * period;
                    start = t.max(sched - buffer);
                }
                let at = t0 + std::time::Duration::from_secs_f64(start / 1000.0);
                let read_now = if throttled { th.on_call(at) } else { true };
                let mut took = host;
                if read_now {
                    th.on_read(read, at);
                    took += read;
                }
                if start >= from {
                    rendered += 1;
                    reads += u64::from(read_now);
                }
                t = start + took;
                n += 1;
            }
            let secs = (to - from) / 1000.0;
            println!(
                "[INFO] {label}{}: 本体 {:.1} fps / フルサイズプレビュー {:.1} fps{}",
                if throttled { "（間引きあり）" } else { "（v0.6.0）" },
                rendered as f64 / secs,
                reads as f64 / secs,
                if throttled { format!("（最大 {} フレームに 1 回）", th.max_every) } else { String::new() }
            );
        }
    }
}
