<p align="center">
  <img src="examples/crispawn/assets/crispawn/logo-hero.png" alt="CRISPAWN" width="860">
</p>

# CRISPAWN Community

CRISPAWNは小容量のLinux環境で起動し、PCの内部ディスク全体のコピー・復元を行います。コピーしたデータはUSBハードディスクへ保存します。
Community版はネットワークに接続せず、USB／NTFSだけを保存先にします。

起動メディアの作成と設定は、Windows用の[CRISPAWN Builder](https://github.com/bunbun-digital/crispawn-builder)を利用してください。
BuilderがUSBドライブ、ISO、設定ファイルを準備します。このリポジトリではLiveISOとその設定・動作の仕様を定義しています。

ISOとSHA-256は[Releases](https://github.com/bunbun-digital/crispawn/releases)から取得できます。
コードは[Apache-2.0](LICENSE)、名称・ロゴ・ブランド画像は[All rights reserved](examples/crispawn/assets/crispawn/LICENSE)です。
Linuxなどの第三者部品はそれぞれのライセンスを維持します。

## 動作デモ

![CRISPAWNの日本語UIによるディスクコピー](https://github.com/bunbun-digital/crispawn/releases/download/v0.1.0-alpha.3/crispawn-demo-ja.gif)

[English demo](README.md#demo)

実際のQEMU上の動作を6秒に早送りした映像です。スプラッシュ後の操作画面から正常な電源OFFまでを収録しています。
10GBの合成ディスクに、圧縮しやすいダミーデータを1GB入れています。デモの開始待ちは5秒です（既定は30秒）。

## 生成するISOの仕様

| 項目 | Community LiveISO |
| --- | --- |
| 対象 | x86-64 PC。コピー元ディスクへOSをインストールせず動作するLive Linux |
| 起動 | BIOS・UEFIによる光学起動と、Builderで作成したUSBからの起動。Secure BootにはDebianの署名済みカーネル・shim・GRUBを使用 |
| サイズ | 公式ビルドの上限は100,000,000バイト。実サイズとSHA-256は各Releaseに記載 |
| 画面 | キーボード操作のGUI。日本語・英語に対応し、コンソール・画面なしの動作も設定可能 |
| 保存先 | NTFSのデータ領域を持つUSBハードディスク。保存したファイルの存在はWindowsから確認可能 |
| ネットワーク | オフライン。公式Community ISOにWi-Fi・ネットワークドライバーやSMB／S3保存機能は含まない |

光学起動は読み取り専用のISOを直接使用し、保存先USBのUUIDを明示設定する必要があります。
USB起動に必要なファイルの配置は後述の「起動メディアの構成情報」に記載しています。
Secure Boot用の署名済みファイルは変更しません。実際に確認した環境と制限は各Releaseに記録します。

## 設定ファイル

Builderは起動前にISOの動作を設定します。
実行時設定は`kind: "crispawn.live"`、`schema_version: 2`のJSONです。
設定項目とJSONの形式は[USB用の完全な設定例](config/crispawn/examples/live-v2-usb.json)と[JSON Schema](config/crispawn/schemas/live-v2.schema.json)で定義しています。
以下の表は標準設定です。必須フィールドは省略できません。

USB起動時に、起動領域直下に`/crispawn-config.json`がある場合は、その内容でISO内蔵設定をすべて置き換えます。
内蔵設定の一部だけを上書きする方式ではありません。ファイルがない場合は内蔵設定を使用します。
不正なJSONや未知の項目、未対応機能を指定した場合はエラーで停止します。
こうしたエラーが起きたときに、別の保存先へ自動で切り替えません。
内蔵設定と対応機能の情報を保存する場所は[Builderが利用するISO内のファイル](docs/COMPATIBILITY.md#builder-and-stored-data-interfaces)に記載しています。

| 設定 | 選べる動作と標準値 |
| --- | --- |
| `sources` | `all_internal`、または安定したディスクIDを列挙する`selected_ids`。標準は対象となる内部ディスクすべて |
| `destination` | `kind: "usb_disk"`、`filesystem: "ntfs"`。起動ディスクのデータ領域、またはUUIDで指定した保存先。標準ディレクトリは`crispawn/backups` |
| `startup` | `auto_copy`・`menu`・`restore_select`。標準は30秒後の`auto_copy`。GUIの待ち時間は0～300秒 |
| `ui` | `gui`・`console`・`headless`、言語は`ja`・`en`。標準は日本語GUI |
| `processing` | 読み出しサイズ・圧縮方式・並列度を自動選択。標準の初期プロファイルは`balanced` |
| `logging` | `always`・`on_error`・`never`。標準はログをバックアップと同じ保存先へ常時同期し、`sync_interval_ms: 0`を使用 |
| `completion` | 0～60秒後の`poweroff`、または画面を維持する`stay`。標準は5秒後の電源OFF |
| `failure` | 任意項目で、`poweroff`・`stay`を指定。省略すると正常終了と同じ待ち時間で電源OFF。正常終了が`stay`の場合は5秒待つ |
| `network` | Communityは`mode: "none"`のみ。共通のJSON Schemaにネットワーク保存用の項目があっても、Communityでは利用できない |

コンソールで自動コピーする場合の開始待ちは0秒です。
画面なしの`headless`では待ち時間0秒の`auto_copy`のみを使用でき、画面を操作して復元元を選ぶことはできません。
設定全体の説明は[実行時設定](docs/CONFIGURATION.md)を参照してください。

### 起動メディアの構成情報

ISOには、起動用ISOの配置場所、データ領域のボリューム名、保存フォルダを示す`/boot/crispawn-media.json`を格納します。
BuilderはUSBへ書き込む前に、このファイルの5項目が以下の内容と完全一致することを確認します。

```json
{"kind":"crispawn.media_layout","schema_version":2,"boot_iso_path":"/crispawn.iso","data_volume_label":"CRISPAWN","data_directory":"crispawn"}
```

署名済みUSB起動では、FAT32のEFI領域直下に`/crispawn.iso`と任意の`/crispawn-config.json`を配置します。
データはラベル`CRISPAWN`のNTFS領域へ保存します。ハッシュ付きのISO名は代替名として扱いません。
起動用ISOがない場合はコピー開始前に`BOOT_ISO_NOT_FOUND`と期待するパスを表示します。
詳細は[USB起動時のファイル配置](docs/COMPATIBILITY.md#usb-media-layout-v2)に記載しています。

## コピー・復元の外部仕様

1. 準備：起動時にディスクを検出して保存先を確認します。試験読み取り・圧縮などの準備はGUIのカウントダウン終了を待たずに進めます。
   待ち時間が終了し、かつ準備が完了した時点でコピーを開始します。
2. 対象：IDによる事前指定がなければ、対象となる内部ディスクをすべて選択します。
   USB・リムーバブル・起動用・保存先のディスクは除外し、使用中のディスクは選択できません。
   複数台は1台ずつコピーし、コピー元のデータは変更しません。
3. コピー：パーティションテーブル、未使用領域、BitLocker暗号化領域を含め、ディスク全体のバイト列を保存します。
   暗号化データの復号は行いません。圧縮後の容量は実際のデータによって決まり、ファイルシステムの使用量と同じになるとは限りません。
4. 進捗・操作：進捗率、bytes・GB、残り時間、読み出しサイズ、圧縮方式・レベル、並列度を表示します。
   一時停止・再開に対応し、中止には確認を求めます。保存済みデータの確認はコピーと並行して行います。
5. 復元：保存済みの日時・セッションを選び、その中の元ディスクと復元先を選択して確認します。
   復元先には元ディスクのバイト数以上の容量が必要です。選んだ復元先を上書きし、書き戻した領域を読み返して確認しながら処理します。
6. 完了：選択したすべての処理と確認が完了してから成功を表示します。確認のためだけの独立した段階は設けません。
   電源OFF設定の場合は、完了メッセージと残り秒数を表示してから正常終了します。

圧縮により収まる場合は、元ディスクより小さい保存先を使用できます。
実際の空き容量が**1,000,000,000バイト未満**になった場合は停止します。
元ディスク容量分の空き容量を事前に確保しません。容量消費率と進捗率も比較しません。
一時的なI/Oエラーは設定回数まで再試行します（標準は3回、間隔100ms）。
回復不能なエラー、整合性確認の失敗、設定した進捗停止時間の超過は、成功ではなく失敗として扱います。
エラーメッセージは選択した言語で表示します。失敗ログは保存先が利用可能な範囲で残します。
`failure.action`で、エラー画面を表示したままにするか、待ち時間を表示してから電源OFFするかを選べます。

## 保存されるデータ

NTFS上の標準構成は次のとおりです。

```text
crispawn/
  backups/    圧縮したディスクデータとメタデータ
  logs/       コピー・復元・エラーのログ
  sessions/   保存日時・セッションとディスクの情報
  reports/    処理結果のレポート
  work/       処理中の一時データ
```

各バックアップには圧縮データファイル、`metadata.json`、`files.csv`、完了・整合性の記録を保存します。
データファイルごとに元ディスク上の連続領域を保存します。圧縮後のサイズをおおむね揃えるため、各ファイルに保存する元領域の長さは異なります。
各ファイルの上限は**2,000,000,000バイト**で、領域末尾の端数は小さくなる場合があります。
`files.csv`には相対ファイル名、`source_offset_bytes`、`source_length_bytes`、`stored_bytes`、`raw_blake3`、圧縮プロファイルを記録します。
BLAKE3はファイルごとの**圧縮前の元領域**から計算した値で、コピー・復元の確認に使用します。ディスク全体の単一ハッシュは必要としません。
圧縮ファイルとメタデータを合わせて1つのバックアップとして扱います。

## ビルド・配布情報

ISOの生成・試験は[BUILDING.md](BUILDING.md)、設定は[実行時設定](docs/CONFIGURATION.md)、
再配布条件は[ライセンス](docs/LICENSING.md)を参照してください。
開発初期版です。実際に確認した起動環境と制限は各Releaseの試験記録に示します。

