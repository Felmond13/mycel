#!/usr/bin/env bash
# Médiane + p95 depuis un fichier .times (nanosecondes, une valeur par run).
set -euo pipefail

[ -s "$1" ] || { echo "fichier vide ou absent : $1" >&2; exit 1; }

sort -n "$1" | awk '
  { v[NR] = $1 }
  END {
    m = (NR % 2) ? v[(NR + 1) / 2] : (v[NR / 2] + v[NR / 2 + 1]) / 2
    p = v[int(NR * 0.95 + 0.999999)]
    mean = 0
    for (i = 1; i <= NR; i++) mean += v[i]
    mean /= NR
    ss = 0
    for (i = 1; i <= NR; i++) ss += (v[i] - mean)^2
    cv = (mean > 0) ? sqrt(ss / NR) / mean * 100 : 0
    printf "n=%d median_ms=%.3f p95_ms=%.3f mean_ms=%.3f cv_pct=%.1f\n", NR, m / 1e6, p / 1e6, mean / 1e6, cv
  }'
