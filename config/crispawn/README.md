# CRISPAWN 設定契約

正本は[設定ファイル仕様](../../docs/CONFIGURATION.md)。
独自の仕様・Schema・例・検証コードはApache-2.0です。

| 場所 | 内容 |
| --- | --- |
| `schemas/builder-project-v1.schema.json` | Windowsで再編集するproject |
| `schemas/live-v2.schema.json` | v2対応LiveOS用runtime。v1専用の旧ISOへ直接入れない |
| `schemas/credentials-v1.schema.json` | privateな認証情報と参照型 |
| `schemas/common-v1.schema.json` | 共通型と上限 |
| `schemas/release-capabilities-v1.schema.json` | 対応するconfig版と機能の宣言 |
| `examples/builder-usb.json` | USB・30秒後開始・5秒後OFFの既定project |
| `examples/builder-iso.json` | ローカルrelease入力、外部ESP用ISO保存 |
| `examples/builder-menu-v2.json` | v2の自動開始しないmenu |
| `examples/builder-restore-select-v2.json` | v2の復元元選択。無人復元ではない |
| `examples/builder-smb-v2.json`, `builder-s3-v2.json` | 将来backendの構造例。対応済みとは限らない |
| `examples/credentials.example.json` | 架空のsecretのみ。実値をcommitしない |
| `examples/capabilities-*.fixture.json` | 試験用の機能宣言。署名済みreleaseの証明ではない |
| `examples/live-v1-usb.generated.json` | 既定projectから生成する現行v1用golden file |
| `examples/live-v2-wifi-static.json` | Wi-Fi、固定IPv4、認証情報参照を指定したruntime |
| `examples/live-v2-optical.json` | CD/DVD起動と別USB上のNTFS。保存先UUIDは実値へ置き換える |

v1のgolden fileは旧reader向けの出力例であり、新ファイル用checkerの直接入力ではありません。

SchemaはJSON Schema Draft 2020-12、参照は固定のURNです。外部への問い合わせは不要です。
byte長、相互参照、対応release、v1変換はSchemaだけでは検査しきれないため、仕様の意味規則も実装してください。
現行の[標準ISO](../../docs/ARCHITECTURE.md)はネットワークdriverなしです。
network設定を保存できることと、そのISO単独で接続できることを区別し、capabilityと追加moduleを照合してください。
Schemaの`additionalProperties:false`等の意味は[公式のobject仕様](https://json-schema.org/understanding-json-schema/reference/object)を参照してください。

## 参考checker

開発用Python依存は`tools/requirements-config-contract.txt`に記録しています（Python 3.9以降）。
Linuxのv2 ISO設定差し替えtoolもこのhost用依存を使います。guestへは追加しません。
これらは配布するWindows BuilderやISOのruntime依存ではありません。

```text
python -m pip install -r tools/requirements-config-contract.txt
python tools/check-crispawn-config.py config/crispawn/examples/builder-usb.json
python tools/check-crispawn-config.py config/crispawn/examples/builder-usb.json --capabilities config/crispawn/examples/capabilities-legacy-v1.fixture.json
python test/config/crispawn-config-contract.test.py
```

`--emit-runtime`を付けると、指定済みのcapabilityに対して変換した**新規の設定fileだけ**を書き出します。
親directoryは先に用意し、既存fileは上書きしません。secretを必要とする設定のprovisioningは参考checkerでは行いません。
checkerは入力の署名を検証せず、ISO・媒体・networkも操作しません。

rootの`npm test`にも共通試験の入口があります。Pythonや開発用validatorがなければ、その試験は理由付きでskipします。
Windowsでは必要なら`CRISPAWN_TEST_PYTHON`へ使用するPython executableのpathを指定してください。
