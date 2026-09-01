# agent-audit

go-llm-agentがApache Iggyに送出する監査イベント（LLMへの入力と出力、ツール呼出とその結果、トークン使用量）を、セッション単位で再生表示するターミナルUIです。セッション一覧、タイムライン、選択したイベントの詳細をひとつの画面で確認できます。

Iggyそのものには閲覧画面がありません。`http://127.0.0.1:3000`はIggyのREST APIで、ブラウザで開いてもJSONが返るだけです。イベントの中身を人が読む手段がこのTUIです。

## 全体の流れ

1. Iggyサーバーを起動し、Personal Access Token（PAT）を発行します。
2. go-llm-agentの設定に`audit`節を追加し、環境変数`IGGY_PAT`を渡してエージェントを実行します。エージェントは実行ごとにIggyのtopic（セッション）へイベントを書き込みます。
3. `agent-audit`を起動すると、左ペインにセッションが並び、選ぶとタイムラインと詳細が表示されます。

## 前提

- Apache Iggyサーバー（apache/iggy:0.8.0相当）が起動していること
- 環境変数`IGGY_PAT`にPersonal Access Tokenを設定していること

### Iggyの起動

```bash
docker run -d --name iggy --restart unless-stopped \
  -p 3000:3000 -p 8090:8090 \
  -v iggy-data:/app/local_data \
  apache/iggy:0.8.0
```

macOSのcolimaでは、`-p`によるポート転送ではサーバーが接続を切る場合があります。その場合は`-p`の2行を`--network host --security-opt seccomp=unconfined`に置き換えてください。

初回起動時のrootパスワードはコンテナのログに出ます。

```bash
docker logs iggy 2>&1 | grep 'Generated root user password'
```

### PATの発行

REST API（ポート3000）でrootとしてログインし、PATを作ります。`expiry`はマイクロ秒単位の整数で、`0`は無期限です。

```bash
TOKEN=$(curl -s -X POST http://127.0.0.1:3000/users/login \
  -H 'Content-Type: application/json' \
  -d '{"username":"iggy","password":"<rootパスワード>"}' | jq -r .access_token.token)

curl -s -X POST http://127.0.0.1:3000/personal-access-tokens \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"name":"go-llm-agent","expiry":0}' | jq -r .token
```

得た値を`IGGY_PAT`として、エージェント側とビューア側の両方に渡します。シェルの設定ファイルで読み込む形にしておくと便利です。

```bash
export IGGY_PAT="$(cat ~/.config/agent-audit/iggy_pat)"
```

## go-llm-agent側の設定

go-llm-agentの設定ファイルに`audit`節を追加してください。PATは設定ファイルには書かず、環境変数`IGGY_PAT`だけから読む設計です。`IGGY_PAT`が未設定のときは警告を出して監査を無効にするので、エージェント自体はそのまま動きます。

```yaml
audit:
  iggy_url: http://127.0.0.1:3000   # IggyのREST URL
  wal_dir: ""                       # 空なら $HOME/.go-llm-agent/audit-wal
  stream: agent-audit
  message_expiry: ""                # 空なら90日
```

エージェントはイベントをまずローカルのWALに書き、Iggyへ非同期に送ります。Iggyが止まっていてもエージェントは止まらず、復旧後に未送信分を送る仕組みです。

セッション名は`agent serve`ではリクエストヘッダ`X-Session-Id`の値、`agent chat`では会話のセッションIDです。`agent run`のようにセッション名がない実行は`run-<実行ID>`という名前になり、一覧の末尾にまとまります。

## インストールと起動

```bash
cargo install --path .
agent-audit
```

`cargo run --`でも起動できます。オプションは次のとおりです。

| オプション | 既定値 | 内容 |
| --- | --- | --- |
| `--iggy-addr` | `127.0.0.1:8090` | IggyのTCPアドレス |
| `--stream` | `agent-audit` | 読むstream名。go-llm-agent側の`audit.stream`と合わせます |
| `--tls` | なし | TLSで接続します。ループバック以外のアドレスでは必須です |

TLSなしでループバック以外へ接続しようとすると、PATが平文で流れるため起動時に拒否します。

## 画面の見方

| ペイン | 内容 |
| --- | --- |
| セッション（左） | streamのtopic一覧です。5秒ごとに再取得します。`run-`で始まるものはセッション名なしの実行で、末尾にまとまります |
| タイムライン（中） | 選択したセッションのイベントを時系列に並べます。同じ実行の中は`seq`順、実行同士は最初のイベントの時刻順です。ツール呼出は`llm_response`、`tool_call`、`tool_result`をひとまとまりにして`▾ ツール呼出 <call_id>`の見出しで表示します。結果がまだ届いていないときは「結果待ち」、同じ呼出をやり直したときは「試行2」のように付きます |
| 詳細（右） | 選択したイベントの本文です。`llm_request`は`role: content`の形で会話履歴を、`llm_response`と`tool_result`は`content`をそのまま表示します。それ以外はJSONです。送出側で本文が大きすぎて切り詰められた場合は「切り詰め（nバイト）」と表示します |

画面上部には件数と、スキーマに合わずスキップしたイベント数、追尾中かどうかを表示します。接続が切れると赤いバナーに切り替わり、1秒から最大30秒まで倍増する間隔で再接続を試みるので、接続が回復すればバナーは自然に消えます。

フォーカスしているペインの見出しには`*`が付く表示です。

## キー操作

| キー | 動作 |
| --- | --- |
| `Tab` | セッション一覧、タイムライン、詳細の間でフォーカスを切り替える |
| `j` / `↓` | フォーカス中のペインで下に移動する。セッション一覧で動かすとそのセッションを読み込む |
| `k` / `↑` | フォーカス中のペインで上に移動する |
| `Enter` | タイムラインでツール呼出のまとまりを折りたたむ、または開く |
| `f` | 選択中セッションの末尾を追尾する状態を切り替える。500msごとに新着を取り込み、最新行を選択する |
| `PageUp` / `PageDown` | 詳細ペインをスクロールする |
| `q` | 終了する |

## 動作確認

エージェントを1回実行してから`agent-audit`を起動し、一覧に新しい`run-...`が現れ、選ぶとタイムラインに`llm_request`と`llm_response`が並べば、送出から表示までの経路がつながっています。LLMへの接続に失敗した実行でも、`llm_request`とエラー付きの`llm_response`が記録されます。

イベントのスキーマは`schema/event.schema.json`（v1）で、送出側と受け側の唯一の接点です。
