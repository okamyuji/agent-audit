#!/usr/bin/env bash
# doclint: docs/ 配下の Markdown を機械検査する。ルールの正本はこのファイル。
# 使い方: bash tools/doclint.sh [dir...]   (既定: docs)
# 出力: [severity] path:line message と、末尾に Critical/High/Medium/Low の件数。
set -u
targets=("${@:-docs}")
export LC_ALL=en_US.UTF-8

find "${targets[@]}" -name '*.md' -print0 2>/dev/null | xargs -0 perl -CSD -Mutf8 -ne '
  BEGIN { %n = (Critical=>0, High=>0, Medium=>0, Low=>0); }
  if (/^\s*```/) { $code = !$code; next; }
  next if $code;
  my $line = $_; chomp $line;
  # コードスパンは半角英数1文字に置き換える。空文字にすると前後の空白が誤検出になる
  my $stripped = $line; $stripped =~ s/`[^`]*`/C/g; $stripped =~ s/\(https?:[^)]*\)//g;
  sub rep { my ($sev,$msg)=@_; $n{$sev}++; print "[$sev] $ARGV:$.: $msg\n"; }
  # 見出しレベルの飛び (## の次に ####)
  if ($line =~ /^(#+)\s/) {
    my $lvl = length $1;
    rep("High", "見出しレベルが飛んでいます (h$prev_h -> h$lvl)") if defined $prev_h && $lvl > $prev_h + 1;
    $prev_h = $lvl;
  }
  # 全角と半角英数の間の半角スペース (表・見出しの行は除外)
  if ($stripped !~ /^\s*(\||#)/ && $stripped =~ /[\p{Han}\p{Hiragana}\p{Katakana}] [A-Za-z0-9]|[A-Za-z0-9] [\p{Han}\p{Hiragana}\p{Katakana}]/) {
    rep("Medium", "全角と半角英数の間に半角スペースがあります");
  }
  # 太字強調
  rep("Medium", "アスタリスク2つの強調があります") if $stripped =~ /\*\*[^*]+\*\*/;
  # 文末コロン
  rep("Medium", "文末がコロンで終わっています") if $stripped =~ /[\p{Han}\p{Hiragana}\p{Katakana}][:：]\s*$/;
  # 未決定
  rep("Critical", "未決定の記述 (TBD/TODO) があります") if $stripped =~ /\b(TBD|TODO)\b/;
  # 常体の文末 (である。/ だ。)
  rep("Low", "常体の文末があります") if $stripped !~ /^\s*(\||#|-|\d+\.)/ && $stripped =~ /(である|だ)。\s*$/;
  END { print "Critical $n{Critical} / High $n{High} / Medium $n{Medium} / Low $n{Low}\n"; exit(($n{Critical}+$n{High}) ? 1 : 0); }
'
