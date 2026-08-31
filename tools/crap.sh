#!/usr/bin/env bash
# tools/crap.sh: 関数ごとの CRAP = cc^2 * (1 - cov)^3 + cc を出し、15 以上があれば exit 1。
# cc は rust-code-analysis-cli の cyclomatic、cov は cargo llvm-cov の関数カバレッジ (0..1)。
#
# rust-code-analysis-cli はクロージャの関数名を全て "<anonymous>" として返す。
# llvm-cov のマングルされたシンボル名と文字列一致させる方式では、クロージャが
# 常にヒットせずカバレッジ0扱いになってしまう（実測で確認済み）。そのため
# ファイルパス＋行範囲でリージョンを突き合わせる方式に置き換えている。
set -euo pipefail
cd "$(dirname "$0")/.."
cargo llvm-cov --lib --tests --json --output-path target/llvm-cov.json >/dev/null
rm -rf target/rca
mkdir -p target/rca
rust-code-analysis-cli -m -O json -p src/ -o target/rca >/dev/null
python3 - <<'EOF'
import glob, json, sys

llcov = json.load(open("target/llvm-cov.json"))["data"][0]

# ファイルパス毎に region を集約する。同じソース関数でも、lib のユニットテスト
# バイナリと各結合テストバイナリでそれぞれ別クレートとしてコンパイルされるため、
# 同一 region が「実行された版」と「そのバイナリでは未実行の版」の複数エントリで
# 重複して現れる。単純に連結すると未実行の重複でカバレッジが薄まってしまうので、
# (開始行, 開始列, 終了行, 終了列) をキーに実行回数の最大値を取り、
# 「どれか1つのテストバイナリで実行されていれば実行済み」として扱う。
# llvm-cov のファイルパスは絶対パス、rust-code-analysis 側は起動時に渡したパス
# （相対）なので、末尾一致で対応づける。
regions_by_file = {}
for f in llcov["functions"]:
    for file in f["filenames"]:
        bucket = regions_by_file.setdefault(file, {})
        for r in f["regions"]:
            key = tuple(r[:4])
            bucket[key] = max(bucket.get(key, 0), r[4])


def regions_for(rca_file):
    merged = {}
    for lf, bucket in regions_by_file.items():
        if lf.endswith(rca_file) or rca_file.endswith(lf):
            for key, count in bucket.items():
                merged[key] = max(merged.get(key, 0), count)
    return merged


def cov_for_span(regions, start, end):
    in_span = [count for (line, *_rest), count in regions.items() if start <= line <= end]
    if not in_span:
        return 0.0
    executed = sum(1 for count in in_span if count > 0)
    return executed / len(in_span)


worst = []
for p in glob.glob("target/rca/**/*.json", recursive=True):
    d = json.load(open(p))
    file = d["name"]
    regions = regions_for(file)

    def walk(n):
        if n.get("kind") == "function":
            cc = n["metrics"]["cyclomatic"]["sum"]
            c = cov_for_span(regions, n["start_line"], n["end_line"])
            crap = cc * cc * (1 - c) ** 3 + cc
            worst.append((crap, n["name"], cc, c, f"{file}:{n['start_line']}"))
        for ch in n.get("spaces", []):
            walk(ch)

    walk(d)

worst.sort(reverse=True)
bad = [w for w in worst if w[0] >= 15]
for crap, name, cc, c, loc in worst[:10]:
    print(f"{crap:6.1f}  cc={cc:2.0f} cov={c:4.2f}  {name}  ({loc})")
sys.exit(1 if bad else 0)
EOF
