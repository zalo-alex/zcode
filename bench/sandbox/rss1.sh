# usage: rss1.sh <binary> [zcode args...]  -> idle REPL VmRSS (kB), 5 samples
export ZCODE_CONFIG_CONTENT='{"provider":{"f":{"options":{"baseURL":"http://127.0.0.1:1/v1"}}},"model":"f/m"}'
mkdir -p /tmp/bh && cd /tmp/bh
b=$1; shift
for i in 1 2 3 4 5; do
  (sleep 2 | $b "$@" >/dev/null 2>&1 &) ; sleep 0.8
  pid=$(pgrep -n -f "^$b")
  grep VmRSS /proc/$pid/status | awk '{printf "%s ",$2}'
  sleep 1.5
done; echo
