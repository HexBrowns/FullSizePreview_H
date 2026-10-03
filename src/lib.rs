//! FullSizePreview_H — 空白キーでプレビュー再生したとき、シーンを等倍で映す別ウィンドウを出す。
//!
//! - 開くのは「空白キーで再生を始めた」かつ「シーンに受け口のカスタムオブジェクトがある」とき（`input` / `receiver`）。
//!   プレビュー再生ボタン・右クリックメニューの「再生」では開かない。受け口の無いシーンでも開かない
//!   （設定での ON / OFF は持たない。受け口を置くかどうかで決める。ユーザーの指定、2026-09-27）
//! - 再生が止まると閉じる。ウィンドウをクリックしても閉じる（再生は続く）
//! - 受け口は編集メニュー／レイヤーの右クリックメニュー「フルサイズプレビュー受け口を置く」で置ける（`placer`）

mod input;
mod placer;
mod preview;
mod readback;
mod receiver;
mod settings;

use aviutl2::generic::{EditState, GlobalEditHandle};
use aviutl2::AnyResult;
use windows::Win32::Foundation::HWND;

pub static EDIT_HANDLE: GlobalEditHandle = GlobalEditHandle::new();

fn init_logging() {
    use aviutl2::tracing::Level;
    let level = if cfg!(debug_assertions) { Level::DEBUG } else { Level::INFO };
    let _ = aviutl2::tracing_subscriber::fmt()
        .with_max_level(level)
        .event_format(aviutl2::logger::AviUtl2Formatter)
        .with_writer(aviutl2::logger::AviUtl2LogWriter)
        .try_init();
}

#[aviutl2::plugin(GenericPlugin)]
pub struct FullSizePreviewPlugin {
    receiver: aviutl2::generic::SubPlugin<receiver::Receiver>,
    hooked: bool,
    hook_warned: bool,
}

impl FullSizePreviewPlugin {
    /// 本体の UI スレッドに入力のフックを掛ける（1 回だけ）。
    fn ensure_hooks(&mut self) {
        if self.hooked || !EDIT_HANDLE.is_ready() {
            return;
        }
        match EDIT_HANDLE.get_host_app_window_raw() {
            Some(h) => {
                input::install(HWND(h.hwnd.get() as _));
                self.hooked = true;
            }
            None if !self.hook_warned => {
                tracing::warn!("FullSizePreview_H: 本体のメインウィンドウを取得できませんでした（後で取り直します）");
                self.hook_warned = true;
            }
            None => {}
        }
    }
}

impl aviutl2::generic::GenericPlugin for FullSizePreviewPlugin {
    fn new(info: aviutl2::AviUtl2Info) -> AnyResult<Self> {
        init_logging();
        tracing::info!("FullSizePreview_H v{} 初期化", env!("CARGO_PKG_VERSION"));
        Ok(Self {
            receiver: aviutl2::generic::SubPlugin::new_filter_plugin(&info)?,
            hooked: false,
            hook_warned: false,
        })
    }

    fn plugin_info(&self) -> aviutl2::generic::GenericPluginTable {
        aviutl2::generic::GenericPluginTable {
            name: "FullSizePreview_H".to_string(),
            information: format!(
                "FullSizePreview_H v{} - 空白キーのプレビュー再生を等倍の別ウィンドウに映す（カスタムオブジェクト「{}」を置いたシーンで動く） / by HexBrowns",
                env!("CARGO_PKG_VERSION"),
                receiver::RECEIVER_NAME
            ),
        }
    }

    fn register(&mut self, registry: &mut aviutl2::generic::HostAppHandle) {
        EDIT_HANDLE.init(registry.create_edit_handle());
        registry.register_filter_plugin(&self.receiver);
        // 受け口を置いてカメラ制御の対象から外す（編集メニューはショートカットを割り当てられる）
        registry.register_edit_menu(placer::MENU_NAME, placer::on_menu);
        registry.register_layer_menu(placer::MENU_NAME, placer::on_menu);
    }

    fn on_project_load(&mut self, _project: &mut aviutl2::generic::ProjectFile) {
        // 本体のメインウィンドウはこの時点で取れる（register の中ではまだ取れない）
        self.ensure_hooks();
    }

    fn event_change_edit_state(&mut self) {
        // 起動直後に取れなかったときの保険。この回の判定はキーの押下状態だけになる
        self.ensure_hooks();
        match EDIT_HANDLE.get_edit_state() {
            Ok(EditState::Preview) => {
                if input::started_by_space() {
                    preview::start();
                }
            }
            Ok(_) => preview::stop(),
            Err(e) => tracing::warn!("FullSizePreview_H: 編集状態を読めませんでした: {e}"),
        }
    }
}

impl Drop for FullSizePreviewPlugin {
    fn drop(&mut self) {
        preview::shutdown();
        input::uninstall();
    }
}

aviutl2::register_generic_plugin!(FullSizePreviewPlugin);
