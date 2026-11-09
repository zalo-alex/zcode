cd ~
hyperfine -N --warmup 30 --runs 300 "$HOME/zcode-base --version" "$HOME/zt/release/zcode --version" 2>&1 | grep -E "Benchmark|Time \(mean|Range"
# idle RSS of the REPL (stdin held open)
for b in "$HOME/zcode-base" "$HOME/zt/release/zcode"; do
  mkdir -p /tmp/bh && cd /tmp/bh
  export ZCODE_CONFIG_CONTENT='{"provider":{"f":{"options":{"baseURL":"http://127.0.0.1:1/v1"}}},"model":"f/m"}'
  (sleep 3 | $b >/dev/null 2>&1 &) ; sleep 1.2
  pid=$(pgrep -n -f "^$b\$" || pgrep -n -x zcode-base || pgrep -n -x zcode)
  echo "$b idle RSS: $(grep VmRSS /proc/$pid/status 2>/dev/null)"
  sleep 2.5
done
