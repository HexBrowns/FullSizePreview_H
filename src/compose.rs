//! 受け口から届いた画素（R,G,B,A 各 8bit・ストレートα）を、背景色の上に合成した BGRA（GDI にそのまま渡せる形）へ。
//!
//! - 1 回の走査で変換と合成を済ませる（v0.6.0 までは乗算済みへの変換と合成で 2 回走査し、1 枚ごとに出力を確保していた）
//! - ウィンドウよりシーンが大きい（4K のシーンを FHD のモニターで見る）ときは、ここで面積平均で縮める。
//!   GDI の HALFTONE での縮小は 4K→FHD で 1 枚 32ms かかり（机上 `cost_tests.rs`）、ウィンドウのスレッドを塞いでいた
//! - 縮小は乗算済みの値（色×α）で平均する。ストレートのまま平均すると、透明な画素の色が縁に混ざる

/// 合成の仕方。
#[derive(Clone, Copy, Debug)]
pub struct Compose {
    /// 透明部分に敷く色（R,G,B）
    pub background: [u8; 3],
    /// αを無視して不透明として出す
    pub ignore_alpha: bool,
}

/// 届いた画素の統計（切り分け用にログへ出す）。
pub struct PixelStats {
    pub alpha_min: u8,
    pub alpha_max: u8,
    pub alpha_mean: f64,
    pub rgb_max: u8,
    /// R,G,B のどれかが 0 でない画素の数
    pub colored: usize,
}

/// 最初の 1 枚だけに使う（全画素を見る）。
pub fn analyze(rgba: &[u8]) -> PixelStats {
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

/// 何か描かれているか（0 でない画素が 1 つでもあれば、そこで打ち切る）。
pub fn has_content(rgba: &[u8]) -> bool {
    rgba.chunks_exact(8).any(|p| u64::from_ne_bytes([p[0], p[1], p[2], p[3], p[4], p[5], p[6], p[7]]) != 0)
        || rgba.chunks_exact(8).remainder().iter().any(|&b| b != 0)
}

pub fn center_pixel(rgba: &[u8], width: u32, height: u32) -> [u8; 4] {
    let i = ((height as usize / 2) * width as usize + width as usize / 2) * 4;
    rgba.get(i..i + 4).map(|p| [p[0], p[1], p[2], p[3]]).unwrap_or_default()
}

/// 縮小の重み（面積平均）。出力の 1 画素が覆う入力の範囲と、その中の各画素の重み（合計 256）
#[derive(Debug, PartialEq)]
struct Taps {
    /// 出力の画素ごとの、入力の最初の位置と、`weights` の中の範囲
    spans: Vec<(usize, std::ops::Range<usize>)>,
    weights: Vec<u32>,
}

impl Taps {
    fn new(src: usize, dst: usize) -> Self {
        let r = src as f64 / dst as f64;
        let mut spans = Vec::with_capacity(dst);
        let mut weights = Vec::with_capacity(dst * (r.ceil() as usize + 1));
        for o in 0..dst {
            let x0 = o as f64 * r;
            let x1 = ((o + 1) as f64 * r).min(src as f64);
            let first = (x0.floor() as usize).min(src - 1);
            let last = (x1.ceil() as usize).clamp(first + 1, src);
            let total = (x1 - x0).max(f64::EPSILON);
            let begin = weights.len();
            let mut sum = 0u32;
            let mut biggest = begin;
            for s in first..last {
                let cover = (x1.min(s as f64 + 1.0) - x0.max(s as f64)).max(0.0);
                let w = (cover / total * 256.0).round() as u32;
                weights.push(w);
                sum += w;
                if w > weights[biggest] {
                    biggest = weights.len() - 1;
                }
            }
            // 丸めの誤差は一番重い画素へ寄せて、合計を 256 ちょうどにする
            weights[biggest] = (weights[biggest] as i64 + 256 - sum as i64).max(0) as u32;
            spans.push((first, begin..weights.len()));
        }
        Self { spans, weights }
    }
}

/// 縮小の作業領域（同じ大きさの間は使い回す）
#[derive(Default)]
pub struct Scaler {
    key: (usize, usize, usize, usize),
    xs: Option<Taps>,
    ys: Option<Taps>,
    /// 入力 1 行ぶんの乗算済みの値（色×α、α×255。0〜65025）
    premul: Vec<u32>,
    /// 横に縮めた 1 行（`premul` と同じ尺度）。直前に作った行は、次の出力の行でも使うことがある
    row: Vec<u32>,
    row_y: Option<usize>,
    /// 縦に重ねた 1 行
    acc: Vec<u32>,
}

/// 0〜65025 の尺度の乗算済みの値（色×α と α×255）を、背景色の上に合成して 8bit へ。
/// 8bit の乗算済みの色は `p / 255`、背景の分は `back × (1 − a / 65025)`
#[inline]
fn over16(p: u32, a: u32, back: u8) -> u8 {
    ((p * 255 + back as u32 * (65025 - a) + 32512) / 65025).min(255) as u8
}

/// `raw`（`sw`×`sh`、ストレートα）を、`dw`×`dh` の BGRA にして `out` へ。
/// `dw`×`dh` が `sw`×`sh` と同じなら等倍、小さければ面積平均で縮める（拡大はしない）。入力が足りなければ false
pub fn compose(
    raw: &[u8],
    (sw, sh): (usize, usize),
    (dw, dh): (usize, usize),
    c: &Compose,
    scaler: &mut Scaler,
    out: &mut Vec<u8>,
) -> bool {
    if sw == 0 || sh == 0 || raw.len() < sw * sh * 4 {
        return false;
    }
    let (dw, dh) = if dw == 0 || dh == 0 || dw > sw || dh > sh { (sw, sh) } else { (dw, dh) };
    out.resize(dw * dh * 4, 0);
    let [br, bg, bb] = c.background;
    if (dw, dh) == (sw, sh) {
        for (s, d) in raw[..sw * sh * 4].chunks_exact(4).zip(out.chunks_exact_mut(4)) {
            let a = if c.ignore_alpha { 255 } else { s[3] as u32 };
            let rest = 255 - a;
            let over = |v: u8, back: u8| ((v as u32 * a + back as u32 * rest + 127) / 255) as u8;
            d[0] = over(s[2], bb);
            d[1] = over(s[1], bg);
            d[2] = over(s[0], br);
            d[3] = 255;
        }
        return true;
    }

    // ちょうど整数分の 1（4K → FHD は 2）なら、K×K の画素を足すだけで済む
    for k in [2usize, 3, 4] {
        if sw == dw * k && sh == dh * k {
            match k {
                2 => box_exact::<2>(raw, sw, dw, dh, c, out),
                3 => box_exact::<3>(raw, sw, dw, dh, c, out),
                _ => box_exact::<4>(raw, sw, dw, dh, c, out),
            }
            return true;
        }
    }
    general(raw, (sw, sh), (dw, dh), c, scaler, out);
    true
}

/// どの割合でも使える面積平均（横に縮めた行を縦に重ねる）
fn general(raw: &[u8], (sw, sh): (usize, usize), (dw, dh): (usize, usize), c: &Compose, scaler: &mut Scaler, out: &mut [u8]) {
    let [br, bg, bb] = c.background;
    let key = (sw, sh, dw, dh);
    if scaler.key != key || scaler.xs.is_none() {
        scaler.key = key;
        scaler.xs = Some(Taps::new(sw, dw));
        scaler.ys = Some(Taps::new(sh, dh));
    }
    // 前の 1 枚の行を使わない
    scaler.row_y = None;
    let Scaler { xs, ys, premul, row, row_y, acc, .. } = scaler;
    let (xs, ys) = (xs.as_ref().unwrap(), ys.as_ref().unwrap());
    premul.resize(sw * 4, 0);
    row.resize(dw * 4, 0);
    acc.resize(dw * 4, 0);

    // 入力の 1 行を、乗算済みにして横に縮める
    let shrink_row = |y: usize, premul: &mut Vec<u32>, dst: &mut Vec<u32>| {
        for (s, p) in raw[y * sw * 4..(y + 1) * sw * 4].chunks_exact(4).zip(premul.chunks_exact_mut(4)) {
            let a = if c.ignore_alpha { 255 } else { s[3] as u32 };
            p[0] = s[0] as u32 * a;
            p[1] = s[1] as u32 * a;
            p[2] = s[2] as u32 * a;
            p[3] = a * 255;
        }
        for (d, (first, range)) in dst.chunks_exact_mut(4).zip(&xs.spans) {
            let mut sum = [128u32; 4];
            for (k, &w) in xs.weights[range.clone()].iter().enumerate() {
                let p = &premul[(first + k) * 4..(first + k) * 4 + 4];
                sum[0] += p[0] * w;
                sum[1] += p[1] * w;
                sum[2] += p[2] * w;
                sum[3] += p[3] * w;
            }
            d[0] = sum[0] >> 8;
            d[1] = sum[1] >> 8;
            d[2] = sum[2] >> 8;
            d[3] = sum[3] >> 8;
        }
    };

    for (oy, (first, range)) in ys.spans.iter().enumerate() {
        acc.iter_mut().for_each(|v| *v = 128);
        for (k, &w) in ys.weights[range.clone()].iter().enumerate() {
            let y = first + k;
            // 出力の行は上から順に作り、入力の行は前へ戻らない。境目の行だけが続けて使われる
            if *row_y != Some(y) {
                shrink_row(y, premul, row);
                *row_y = Some(y);
            }
            for (a, &v) in acc.iter_mut().zip(row.iter()) {
                *a += v * w;
            }
        }
        let line = &mut out[oy * dw * 4..(oy + 1) * dw * 4];
        for (d, v) in line.chunks_exact_mut(4).zip(acc.chunks_exact(4)) {
            let (r, g, b, a) = (v[0] >> 8, v[1] >> 8, v[2] >> 8, (v[3] >> 8).min(65025));
            d[0] = over16(b, a, bb);
            d[1] = over16(g, a, bg);
            d[2] = over16(r, a, br);
            d[3] = 255;
        }
    }
}

/// 縦横ちょうど 1/K に縮める（K×K の画素の面積平均。乗算済みで平均し、背景色の上に合成する）
fn box_exact<const K: usize>(raw: &[u8], sw: usize, dw: usize, dh: usize, c: &Compose, out: &mut [u8]) {
    let full = 255 * (K * K) as u32;
    let half = full / 2;
    let [br, bg, bb] = c.background;
    for oy in 0..dh {
        let line = &mut out[oy * dw * 4..(oy + 1) * dw * 4];
        for (ox, d) in line.chunks_exact_mut(4).enumerate() {
            let mut sum = [0u32; 4];
            for ky in 0..K {
                let row = &raw[((oy * K + ky) * sw + ox * K) * 4..((oy * K + ky) * sw + ox * K + K) * 4];
                for s in row.chunks_exact(4) {
                    let a = if c.ignore_alpha { 255 } else { s[3] as u32 };
                    sum[0] += s[0] as u32 * a;
                    sum[1] += s[1] as u32 * a;
                    sum[2] += s[2] as u32 * a;
                    sum[3] += a;
                }
            }
            // 8bit の乗算済みの平均は sum / (255 K²)、背景の分は back × (255 K² − Σα) / (255 K²)
            let rest = full - sum[3];
            let over = |p: u32, back: u8| ((p + back as u32 * rest + half) / full).min(255) as u8;
            d[0] = over(sum[2], bb);
            d[1] = over(sum[1], bg);
            d[2] = over(sum[0], br);
            d[3] = 255;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLACK: Compose = Compose { background: [0, 0, 0], ignore_alpha: false };

    fn run(raw: &[u8], src: (usize, usize), dst: (usize, usize), c: &Compose) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        compose(raw, src, dst, c, &mut Scaler::default(), &mut out).then_some(out)
    }

    #[test]
    fn ignores_alpha_when_asked() {
        let st = analyze(&[200, 100, 50, 0, 0, 0, 0, 0]);
        assert_eq!((st.alpha_max, st.rgb_max, st.colored), (0, 200, 1));
        let opaque = Compose { background: [3, 0, 46], ignore_alpha: true };
        assert_eq!(run(&[200, 100, 50, 0], (1, 1), (1, 1), &opaque).unwrap(), vec![50, 100, 200, 255]);
    }

    #[test]
    fn rejects_short_buffer() {
        assert!(run(&[0u8; 4], (2, 1), (2, 1), &BLACK).is_none());
        assert!(run(&[], (0, 0), (0, 0), &BLACK).is_none());
    }

    #[test]
    fn converts_straight_rgba_over_background() {
        // 不透明の赤、半透明（α 128）の白、透明（色が残っていても出さない）
        let raw = [255, 0, 0, 255, 255, 255, 255, 128, 200, 100, 50, 0];
        let f = run(&raw, (3, 1), (3, 1), &BLACK).expect("合成できる");
        assert_eq!(&f[0..4], &[0, 0, 255, 255]);
        assert_eq!(&f[4..8], &[128, 128, 128, 255]);
        assert_eq!(&f[8..12], &[0, 0, 0, 255]);
        // 透明部分には背景色（R3 G0 B46）。v0.6.0（乗算済みへ変換してから合成）と同じ値
        let blue = Compose { background: [3, 0, 46], ignore_alpha: false };
        let f = run(&raw, (3, 1), (3, 1), &blue).unwrap();
        assert_eq!(&f[8..12], &[46, 0, 3, 255]);
        assert_eq!(&f[4..8], &[128 + 23, 128, 128 + 1, 255]);
    }

    #[test]
    fn same_result_as_two_step_conversion() {
        // v0.6.0 の 2 段（乗算済みへ丸め → 背景の分を丸めて足す）との差は丸めの 1 まで
        let back = [3u8, 0, 46];
        let c = Compose { background: back, ignore_alpha: false };
        for a in 0..=255u32 {
            for v in [0u32, 1, 77, 128, 200, 254, 255] {
                let raw = [v as u8, v as u8, v as u8, a as u8];
                let f = run(&raw, (1, 1), (1, 1), &c).unwrap();
                let pm = (v * a + 127) / 255;
                let old = |b: u8| (pm + (b as u32 * (255 - a) + 127) / 255).min(255) as i32;
                assert!((f[2] as i32 - old(back[0])).abs() <= 1, "a={a} v={v}");
                assert!((f[0] as i32 - old(back[2])).abs() <= 1, "a={a} v={v}");
            }
        }
    }

    #[test]
    fn taps_cover_source_evenly() {
        // 2 倍: 2 画素ずつ半分
        let t = Taps::new(4, 2);
        assert_eq!(t.spans, vec![(0, 0..2), (2, 2..4)]);
        assert_eq!(t.weights, vec![128, 128, 128, 128]);
        // 1.5 倍: 境目の画素を分け合う
        let t = Taps::new(3, 2);
        assert_eq!(t.spans, vec![(0, 0..2), (1, 2..4)]);
        assert_eq!(t.weights, vec![171, 85, 85, 171]);
        // どの割合でも重みの合計は 256、入力の外を指さない
        for (src, dst) in [(3840, 1920), (2560, 1920), (1440, 1080), (3840, 2560), (1081, 1080), (7, 3)] {
            let t = Taps::new(src, dst);
            for (first, range) in &t.spans {
                assert_eq!(t.weights[range.clone()].iter().sum::<u32>(), 256, "{src}->{dst}");
                assert!(first + range.len() <= src);
            }
        }
    }

    #[test]
    fn halves_by_area_average() {
        // 2x2 → 1x1: 不透明の白 2 つと透明 2 つ → 白 α 50% を黒の上に
        let raw = [255, 255, 255, 255, 0, 0, 0, 0, 255, 255, 255, 255, 0, 0, 0, 0];
        let f = run(&raw, (2, 2), (1, 1), &BLACK).unwrap();
        assert_eq!(f, vec![128, 128, 128, 255]);
        // 透明な画素に色が残っていても混ざらない（乗算済みで平均する）
        let raw = [255, 0, 0, 255, 0, 255, 0, 0, 255, 0, 0, 255, 0, 255, 0, 0];
        let f = run(&raw, (2, 2), (1, 1), &BLACK).unwrap();
        assert_eq!(f, vec![0, 0, 128, 255]);
        // 4K → FHD の 1 画素目は左上 2x2 の平均
        let (w, h) = (3840usize, 2160usize);
        let mut raw = vec![0u8; w * h * 4];
        for (i, p) in raw.chunks_exact_mut(4).enumerate() {
            let (x, y) = (i % w, i / w);
            p.copy_from_slice(&[(x * 40 % 256) as u8, (y * 60 % 256) as u8, 10, 255]);
        }
        let f = run(&raw, (w, h), (1920, 1080), &BLACK).unwrap();
        assert_eq!(f.len(), 1920 * 1080 * 4);
        assert_eq!(&f[0..4], &[10, 30, 20, 255]);
    }

    #[test]
    fn uniform_image_stays_uniform_at_any_ratio() {
        let (w, h) = (2560usize, 1440usize);
        let raw: Vec<u8> = [90u8, 160, 30, 200].iter().copied().cycle().take(w * h * 4).collect();
        let c = Compose { background: [3, 0, 46], ignore_alpha: false };
        let one = run(&raw[..4], (1, 1), (1, 1), &c).unwrap();
        let f = run(&raw, (w, h), (1920, 1080), &c).unwrap();
        for p in f.chunks_exact(4) {
            for k in 0..3 {
                assert!((p[k] as i32 - one[k] as i32).abs() <= 1, "{p:?} vs {one:?}");
            }
        }
    }

    #[test]
    fn exact_box_matches_general_area_average() {
        // ちょうど 1/2・1/3 の近道と、一般の面積平均（重み付き）の差は丸めの 1 まで
        let c = Compose { background: [3, 0, 46], ignore_alpha: false };
        for k in [2usize, 3, 4] {
            let (dw, dh) = (7usize, 5usize);
            let (w, h) = (dw * k, dh * k);
            let raw: Vec<u8> = (0..w * h * 4).map(|i| (i * 37 % 251) as u8).collect();
            let fast = run(&raw, (w, h), (dw, dh), &c).unwrap();
            let mut out = Vec::new();
            let mut scaler = Scaler::default();
            out.resize(dw * dh * 4, 0);
            general(&raw, (w, h), (dw, dh), &c, &mut scaler, &mut out);
            for (i, (a, b)) in fast.iter().zip(&out).enumerate() {
                assert!((*a as i32 - *b as i32).abs() <= 1, "k={k} i={i} {a} {b}");
            }
        }
    }

    #[test]
    fn content_check_stops_at_first_pixel() {
        assert!(!has_content(&[0u8; 4 * 9]));
        let mut v = vec![0u8; 4 * 9];
        v[35] = 1;
        assert!(has_content(&v));
        v[35] = 0;
        v[3] = 1;
        assert!(has_content(&v));
    }
}
