//! 受け口のカスタムオブジェクト「フルサイズプレビュー受け口」。
//!
//! 本体がプレビュー再生で描いたフレームバッファを、そのままフルサイズプレビューへ渡す。
//! 描画をもう 1 回頼む方式（v0.2.0 まで）は、重いシーンで本体の描画と同じ順番待ちに並び、
//! 1 フレームを 2 回描くことになって 3.5 fps まで落ちた（実機 2026-09-27、1 フレーム 24〜34ms のシーン）。
//!
//! - フレームバッファは「このオブジェクトより番号の小さいレイヤーまで」を描いたもの（Lua の `obj.load("framebuffer")` と同じ）。
//!   シーン全体を映すには、**レイヤー番号が一番大きいレイヤー**に、再生する範囲全体を覆う長さで置く
//! - **カメラ制御の対象から外す。** 対象の中では、届くのは合成前の空のバッファになる
//!   （実機 2026-09-27: レイヤー 1〜127 がカメラ制御の下、受け口がレイヤー 128 で、対象に入れると全画素 0、外すと映った）
//! - v0.4.0 まではフィルタオブジェクトだった。カスタムオブジェクトにしたのは、設定欄の「カメラ制御の対象」ボタンで
//!   外せるようにするため（フィルタオブジェクトには出ない。ユーザーの指摘、2026-09-27）
//! - 自分自身は 1x1 の透明な画像を出し、不透明度も 0 にして画面には何も描かない
//! - 画像は本体の API（`copy_image_resource` → `get_image_resource_data`）で読む（`readback`）
//! - ウィンドウが開いていないときは画像を読み出さないので、置いたままでも編集・出力の重さはほぼ変わらない

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use aviutl2::AnyResult;

use crate::{preview, readback};

/// エフェクト名（`.aup2` の `effect.name` に保存される。変えると保存済みのプロジェクトから外れる）
pub const RECEIVER_NAME: &str = "フルサイズプレビュー受け口";

static READ_ERROR_LOGGED: AtomicBool = AtomicBool::new(false);

#[aviutl2::plugin(FilterPlugin)]
pub struct Receiver;

impl aviutl2::filter::FilterPlugin for Receiver {
    type Userdata = ();

    fn new(_info: aviutl2::AviUtl2Info) -> AnyResult<Self> {
        Ok(Self)
    }

    fn plugin_info(&self) -> aviutl2::filter::FilterPluginTable {
        aviutl2::filter::FilterPluginTable {
            name: RECEIVER_NAME.to_string(),
            label: Some("HexScript".to_string()),
            information: format!(
                "{RECEIVER_NAME} v{} - フルサイズプレビューに本体の描画をそのまま渡す（レイヤー番号が一番大きいレイヤーに、カメラ制御の対象から外して置く） / by HexBrowns",
                env!("CARGO_PKG_VERSION")
            ),
            flags: aviutl2::bitflag!(aviutl2::filter::FilterPluginFlags {
                video: true,
                input: true,
            }),
            config_items: Vec::new(),
        }
    }

    fn proc_video(
        &self,
        _config: &[aviutl2::filter::FilterConfigItem],
        video: &mut aviutl2::filter::FilterProcVideo<Self::Userdata>,
    ) -> AnyResult<()> {
        if preview::wants_frames() {
            let frame = video.object.frame_s as usize + video.object.frame as usize;
            let effect_id = video.object.effect_id;
            let mut buffer = preview::take_buffer();
            let t0 = Instant::now();
            match readback::read(video, &mut buffer) {
                Ok(rb) => preview::push(effect_id, frame, rb, buffer, t0.elapsed()),
                Err(e) => {
                    if !READ_ERROR_LOGGED.swap(true, Ordering::Relaxed) {
                        tracing::warn!("FullSizePreview_H: フレームバッファを読み出せませんでした: {e}");
                    }
                    preview::give_back(buffer);
                }
            }
        }
        // 自分は何も描かない
        video.set_image_data(&[0u8; 4], 1, 1);
        video.param.alpha = 0.0;
        Ok(())
    }
}
