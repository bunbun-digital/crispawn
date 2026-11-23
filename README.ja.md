<p align="center">
  <img src="examples/crispawn/assets/crispawn/logo-hero.png" alt="CRISPAWN" width="860">
</p>

# CRISPAWN Community

小容量Linuxから起動し、PCの内部ディスク全体をUSBハードディスクへコピー・復元します。
Community版はネットワークに接続せず、USB／NTFSだけを保存先にします。

ISOとSHA-256は[Releases](https://github.com/bunbun-digital/crispawn/releases)から取得できます。
コードは[Apache-2.0](LICENSE)、名称・ロゴ・ブランド画像は[All rights reserved](examples/crispawn/assets/crispawn/LICENSE)です。
Linuxなどの第三者部品はそれぞれのライセンスを維持します。

## 動作デモ

![CRISPAWNの日本語UIによるディスクコピー](https://github.com/bunbun-digital/crispawn/releases/download/v0.1.0-alpha.3/crispawn-demo-ja.gif)

[English demo](README.md#demo)

実際のQEMU上の動作を6秒に早送りした映像です。スプラッシュ後の操作画面から正常な電源OFFまでを収録しています。
10GBの合成ディスクに、圧縮しやすいダミーデータを1GB入れています。デモの開始待ちは5秒です（既定は30秒）。

## 使い方

起動用USBはFAT32のEFI領域と、ラベルが`CRISPAWN`のNTFS領域を用意します。
独立した[CRISPAWN Builder](https://github.com/bunbun-digital/crispawn-builder)でISOと設定を書き込めます。
ISOにはBIOSと署名済みUEFIの光学起動用エントリーも含みます。DVD起動時の保存先USBは別途明示設定が必要です。

起動中に対象と保存先を確認し、既定30秒のカウント終了後にコピーします。
保存先は`crispawn/backups`、ログ等も`crispawn/`配下です。起動ディスク・保存先ディスクはコピー対象から除外します。
復元は対象選択と確認が必要で、選んだディスクを上書きします。

元ディスク全体を読み取り、圧縮後2,000,000,000バイト以下のファイルに分割します。
元の範囲に対応するBLAKE3を記録し、保存したファイルをコピーと並行して確認します。
圧縮により収まるなら、元ディスクより小さい保存先も使えます。実空き容量が1GB未満なら中止します。
開始待ち時間、正常終了・失敗時の電源OFF、言語等は[設定ファイル](docs/CONFIGURATION.md)で変更できます。

ビルド・試験は[BUILDING.md](BUILDING.md)、責務は[構成](docs/ARCHITECTURE.md)、権利表記は[ライセンス](docs/LICENSING.md)を参照してください。
開発初期版です。実際に確認した起動環境と制限は各Releaseの試験記録に示します。

