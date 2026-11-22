# CRISPAWN 通常GUI用の確定素材

2026-11-21に承認された[UIデザイン](../../../../../docs/CRISPAWN-UI-DESIGN.md)の原本。
提供されたPNGを、承認済み画面内の画像とbyte単位で照合して保存した。
この保管だけで、実GUI・ISOへ組み込んだ意味にはしない。

| ファイル | 原本 | 用途 |
| --- | --- | --- |
| `wordmark.png` | `codex-clipboard-a155d606-6c97-4b0a-8e6b-975d8fc67a1e.png`、1336×168 | 左上のロゴ文字 |
| `crystal/CRISPAWN_00.png`〜`CRISPAWN_09.png` | `CRISPAWN_10frames.zip`、各480×480 | 左上のクリスタルアニメーション |

10フレームは00から09の順に表示し、縦横比を維持する。ロゴ文字と組み合わせ、別の文字組みで製品名を重ねない。
親ディレクトリの `logo.png` はスプラッシュ用であり、本ディレクトリのGUI用素材とは用途が異なる。

各ファイルと元ZIPのSHA-256は[確定時の一覧](../../../../../docs/design/crispawn-ui-20261121.json)に記録する。
画像の再生成・減色・再圧縮は行っていない。

Copyright (c) 2026 bunbun-digital. **All rights reserved**。
[親ディレクトリのLICENSE](../LICENSE)がこのディレクトリにも適用される。OSSライセンスを付与しない。
