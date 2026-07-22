#!/usr/bin/env bash
# Relaunch the long-running dashboard on :7777 with the freshly built binary.
set -euo pipefail
cd "$(dirname "$0")/.."
pkill -f myc-ui || true
sleep 1
cp target/release/myc /tmp/myc-ui
nohup /tmp/myc-ui ui --port 7777 > /tmp/myc-ui.log 2>&1 &
sleep 2
curl -s -o /dev/null -w 'index: %{http_code}\n' localhost:7777/
curl -s localhost:7777/ | grep -o 'script[^>]*src="[^"]*"'
curl -s -o /dev/null -w 'main.js: %{http_code}\n' localhost:7777/js/main.js
curl -s -o /dev/null -w 'vue: %{http_code}\n' localhost:7777/vendor/vue.js
curl -s -o /dev/null -w 'css: %{http_code}\n' localhost:7777/style.css
curl -s -o /dev/null -w 'stats: %{http_code}\n' localhost:7777/api/stats
