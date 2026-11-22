# OS共通UIのRust API

既存の`libbunbun-ui.so.1`（runtime 1.2）へ動的リンクする独自Rust crateです。
LVGL 9.6.0、framebuffer、keyboard、Noto Sans JPはOS側に置きます。Slintには依存しません。

`Ui`はプロセスで1つのUI sessionを所有します。`Widget`と`Font`は`Ui`を借用し、sessionより長く保持できません。
UIとhandleは別threadへ移動・共有できず、バックアップworkerはsnapshotやeventをUI threadへ渡します。
widgetの独立削除とraw LVGL pointerは公開しません。終了時にsceneごと解放します。
Rust callbackをCから呼び返さず、OS側の有界action queueから整数IDを取得します。
queue overflowや文字列中のNUL、部品種類の不一致などはerrorとして扱います。

```rust,no_run
use bunbun_ui::{Kind, Ui};

let ui = Ui::open()?;
ui.direct_keys()?;
let root = ui.root()?;
let label = ui.child(root, Kind::Label)?;
label.font(&ui.font(24)?)?;
label.text("日本語で表示できます")?;
let bar = ui.child(root, Kind::Bar)?;
bar.bar(1000, 375)?;
let pause = ui.child(root, Kind::Button)?;
pause.on_click(1)?;
pause.shortcut('p')?;
// ui.step()でOSイベントを処理し、next_action()でボタン操作を受け取る。
# Ok::<(), bunbun_ui::Error>(())
```

製品固有の文言・layout・進捗計算はcrateへ置きません。[Rust製品view](../../../examples/perfect-backup/rust/crates/demo-ui/src/view.rs)が利用例です。
`Ui::direct_keys()`でフォーカス移動を無効にし、`Widget::shortcut('p')`で既存のclick actionを呼び出します。
英字の大小は区別せず、同じscene内のキー重複はerror、disabled/hiddenなボタンのキーは無効です。
キーを押し続けてもLVGLの押下edgeだけを処理し、ボタン選択やEnterを必要としません。
`Ui::memory()`、`render_rgb()`、`send_key()`、`send_shortcut()`は開発用で、画面・入力deviceを開かず実LVGLの描画と入力を検査します。
通常起動の`Ui::open()`ではfontファイルのpathをアプリへ持たせません。

C ABIは[`bunbun/widgets.h`](../../../br2-external/package/bunbun-ui-runtime/src/bunbun/widgets.h)に定義し、旧`ui.h` APIは維持します。
既存CコードでLVGLの初期化・破棄を並行して行う利用は対象外です。Rustからのsafeな利用でもC実装の全不具合を排除するものではありません。
build時は`BUNBUN_UI_LIB_DIR`と、同じOS SDKのtarget/linkerをそろえます。[製品のbuild手順](../../../examples/perfect-backup/README.md)を参照してください。
独自部分はApache-2.0、LVGL等の第三者部分の条件は維持します。

`Kind::Image`と`Widget::image_rgba()`はRGBAをscene側へcopyし、画面に収まる同一倍率で中央表示します。
`Widget::hidden()`で画面を同じsession内で切り替えられます。PNG decodeはAPIに含まず、製品のbuild時に変換します。
`Ui::display_only()`はキーボード列挙前の早期描画用です。未接続なら定期的に再検出し、入力は後から有効になります。
通常の`Ui::open()`の入力必須条件は維持します。
