# agent-audit

go-llm-agentがApache Iggyに送出する監査イベントを、セッション単位で再生表示するターミナルUIです。セッション一覧、タイムライン、選択したイベントの詳細をひとつの画面で確認できます。

## 前提

- Apache Iggyサーバー（apache/iggy:0.8.0相当）が起動していること
- 環境変数IGGY_PATにPersonal Access Tokenを設定していること

```bash
docker run -d --name iggy -p 3000:3000 -p 8090:8090 apache/iggy:0.8.0
export IGGY_PAT=<token>
```

## 起動方法

```bash
cargo run -- --iggy-addr 127.0.0.1:8090 --stream agent-audit
```

`--iggy-addr`と`--stream`は省略でき、既定値はそれぞれ`127.0.0.1:8090`と`agent-audit`です。

## キー操作

| キー | 動作 |
| --- | --- |
| `Tab` | セッション一覧、タイムライン、詳細の間でフォーカスを切り替える |
| `j` / `↓` | 一覧またはタイムラインで下に移動する |
| `k` / `↑` | 一覧またはタイムラインで上に移動する |
| `Enter` | タイムラインでtool_callのまとまりを折りたたむ、または開く |
| `f` | 選択中セッションの末尾を追尾する状態を切り替える |
| `q` | 終了する |

接続が切れると画面上部にバナーで表示し、1秒から最大30秒まで倍増する間隔で再接続を試みます。接続が回復するとバナーは消えます。

## go-llm-agent側の設定

go-llm-agentの設定ファイルのaudit節でIggyへの送出先を指定し、送出側にも同じIGGY_PATを設定してください。詳細はgo-llm-agent側のドキュメントを参照してください。イベントのスキーマはv1です。
