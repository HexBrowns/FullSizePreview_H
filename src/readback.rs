//! フレームバッファの読み出し（受け口の `proc_video` の中＝本体の描画スレッドで呼ぶ）。
//!
//! 本体の API だけで読む: `copy_image_resource("framebuffer" → "resource:…")` で写し、
//! `get_image_resource_data` で `PIXEL_RGBA`（ストレートα）として取る。同期は本体に任せる。
//!
//! うまくいかなかった読み方（実機 2026-09-27）:
//! - v0.3.1 `get_image_data`: フィルタオブジェクトのオブジェクト画像は空（1920x1080 の全画素が 0,0,0,0）
//! - v0.3.2 `get_framebuffer_texture2d` を D3D11 でステージングへコピーして Map: 形式は RGBA16F で取れたが全画素 0。
//!   本体が描画の命令を GPU へ流し終える前に読んでいたとみられる（推論。本体の描画の作りは確かめていない）

use std::time::{Duration, Instant};

use aviutl2::filter::{FilterProcVideo, ImageResource, OutputImageResourcePixelFormat};

/// 読み出した画素の形式
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    /// R,G,B,A 各 8bit、ストレートα（`PIXEL_RGBA`）
    RgbaStraight8,
}

impl PixelFormat {
    pub fn bytes_per_pixel(self) -> usize {
        match self {
            Self::RgbaStraight8 => 4,
        }
    }
}

pub struct Readback {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    /// `copy_image_resource`（フレームバッファを写す）にかかった時間
    pub copy: Duration,
    /// `get_image_resource_size` と `get_image_resource_data`（GPU から取り出す）にかかった時間
    pub data: Duration,
}

impl Readback {
    pub fn total(&self) -> Duration {
        self.copy + self.data
    }
}

/// フレームバッファを写す先（`resource:` はフィルタ処理の後に本体が捨てる）
const RESOURCE_NAME: &str = "FullSizePreview_H";

/// フレームバッファを `out` に行詰めで写す。
pub fn read<U: aviutl2::filter::FilterUserdata>(
    video: &mut FilterProcVideo<U>,
    out: &mut Vec<u8>,
) -> Result<Readback, String> {
    let resource = ImageResource::Resource(RESOURCE_NAME.to_string());
    let t0 = Instant::now();
    video
        .copy_image_resource(&ImageResource::Framebuffer, &resource)
        .map_err(|e| format!("copy_image_resource(framebuffer): {e}"))?;
    let t1 = Instant::now();
    let (width, height) = video
        .get_image_resource_size(&resource)
        .map_err(|e| format!("get_image_resource_size: {e}"))?;
    if width == 0 || height == 0 {
        return Err(format!("フレームバッファの大きさが {width}x{height} です"));
    }
    let format = PixelFormat::RgbaStraight8;
    let pitch = width * format.bytes_per_pixel() as u32;
    prepare(out, pitch as usize * height as usize);
    video
        .get_image_resource_data(&resource, out, width, height, pitch, OutputImageResourcePixelFormat::Rgba)
        .map_err(|e| format!("get_image_resource_data: {e}"))?;
    Ok(Readback { width, height, format, copy: t1 - t0, data: t1.elapsed() })
}

/// `out` を `len` バイトにする。中身は読み出しで全部上書きされるので、0 で埋め直さない。
///
/// 使い回しのバッファは同じ大きさなので何もしない。足りなければ `vec![0; len]` で取り直す
/// （`resize` は伸ばした分を本体の描画スレッドで 0 埋めする。`vec!` は 0 のページを OS から受け取るだけ）
fn prepare(out: &mut Vec<u8>, len: usize) {
    if out.len() == len {
        return;
    }
    if out.capacity() >= len {
        out.resize(len, 0);
    } else {
        *out = vec![0u8; len];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_keeps_or_replaces_buffer() {
        let mut v = vec![7u8; 16];
        let ptr = v.as_ptr();
        prepare(&mut v, 16);
        assert_eq!((v.len(), v.as_ptr()), (16, ptr), "同じ大きさなら触らない");
        prepare(&mut v, 8);
        assert_eq!(v.len(), 8);
        let mut empty = Vec::new();
        prepare(&mut empty, 32);
        assert_eq!(empty.len(), 32);
    }
}
