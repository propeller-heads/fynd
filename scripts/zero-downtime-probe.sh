#!/usr/bin/env bash
#
# zero-downtime-probe.sh — assert a rolling deploy of the hosted Fynd API drops
# zero requests.
#
# Drives steady quote load at a fixed rate against a target base URL for a fixed
# duration, then reports every non-2xx response (bucketed by status class) and
# the largest gap between two consecutive *successful* responses. The gap metric
# catches a silent stall — a window where requests hang or time out but no HTTP
# error status is ever returned — that a pure status-code check would miss.
#
# Exit 0 only if BOTH hold:
#   - non-2xx count == 0
#   - the success gap never exceeded the threshold (default 3s)
#
# Usage during a release (launch gate for the hosted API):
#   1. Start the probe against staging:
#        AUTH_TOKEN=<staging-token> \
#          scripts/zero-downtime-probe.sh -u https://fynd-api-staging.example/v1/eth
#   2. While it runs, trigger the rolling deploy (e.g. `helmwave up`).
#   3. Wait for the probe to finish (default 300s) or Ctrl-C for a partial summary.
#   4. Read the verdict: exit 0 = zero requests dropped, nonzero = investigate.
#
# The auth token is read from the AUTH_TOKEN environment variable only — never a
# flag, so it never appears in `ps` output. It is passed to curl via a mode-0600
# config file in a private temp dir, keeping it out of curl's argv too.
#
# The request body defaults to the WETH->USDC sell fixture checked in next to
# this script (zero-downtime-probe.quote.json); override with --body.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEFAULT_BODY="${SCRIPT_DIR}/zero-downtime-probe.quote.json"

BASE_URL=""
DURATION=300
RATE=5
MAX_GAP=3
TIMEOUT=10
BODY_FILE="${DEFAULT_BODY}"

usage() {
  cat <<EOF
Usage: $(basename "$0") -u BASE_URL [OPTIONS]

Drive steady quote load during a rolling deploy and assert zero dropped requests.

Required:
  -u, --url BASE_URL     Chain base URL, e.g. https://host/v1/eth
                         (the probe POSTs quote requests to BASE_URL/quote)

Options:
  -d, --duration SECONDS Probe duration            (default: ${DURATION})
  -r, --rate RPS         Requests per second        (default: ${RATE})
  -g, --max-gap SECONDS  Max allowed gap between successful responses
                                                    (default: ${MAX_GAP})
  -t, --timeout SECONDS  Per-request timeout        (default: ${TIMEOUT})
  -b, --body FILE        Quote request body JSON     (default: fixture beside
                         this script)
  -h, --help             Show this help

Environment:
  AUTH_TOKEN             Bearer token for the Authorization header (optional).
                         Read from the environment only, never a flag.

Exit status:
  0  zero non-2xx responses AND success gap never exceeded --max-gap
  1  requests were dropped, the load stalled, or no data was collected
  130 interrupted (SIGINT/SIGTERM) — a partial summary is still printed
EOF
}

die() {
  echo "error: $*" >&2
  exit 2
}

while [[ $# -gt 0 ]]; do
  case "$1" in
  -u | --url)
    BASE_URL="${2:-}"
    shift 2
    ;;
  -d | --duration)
    DURATION="${2:-}"
    shift 2
    ;;
  -r | --rate)
    RATE="${2:-}"
    shift 2
    ;;
  -g | --max-gap)
    MAX_GAP="${2:-}"
    shift 2
    ;;
  -t | --timeout)
    TIMEOUT="${2:-}"
    shift 2
    ;;
  -b | --body)
    BODY_FILE="${2:-}"
    shift 2
    ;;
  -h | --help)
    usage
    exit 0
    ;;
  *)
    echo "unknown option: $1" >&2
    usage >&2
    exit 2
    ;;
  esac
done

[[ -n "${BASE_URL}" ]] || die "missing required --url (see --help)"
[[ "${DURATION}" =~ ^[1-9][0-9]*$ ]] || die "--duration must be a positive integer (got '${DURATION}')"
[[ "${RATE}" =~ ^[1-9][0-9]*$ ]] || die "--rate must be a positive integer (got '${RATE}')"
[[ "${MAX_GAP}" =~ ^[1-9][0-9]*$ ]] || die "--max-gap must be a positive integer (got '${MAX_GAP}')"
[[ "${TIMEOUT}" =~ ^[1-9][0-9]*$ ]] || die "--timeout must be a positive integer (got '${TIMEOUT}')"
[[ -r "${BODY_FILE}" ]] || die "request body not readable: ${BODY_FILE}"

for dep in curl awk sort date; do
  command -v "${dep}" >/dev/null 2>&1 || die "required command not found: ${dep}"
done

# GNU date is required for millisecond epochs; BSD date returns a literal "3N".
[[ "$(date +%s%3N)" =~ ^[0-9]+$ ]] || die "GNU date with %3N (millisecond epoch) is required"

QUOTE_URL="${BASE_URL%/}/quote"
INTERVAL_MS=$((1000 / RATE))
((INTERVAL_MS > 0)) || INTERVAL_MS=1

WORKDIR="$(mktemp -d)"
RESULTS_DIR="${WORKDIR}/results"
mkdir -p "${RESULTS_DIR}"
cleanup() { rm -rf "${WORKDIR}"; }
trap cleanup EXIT

CURL_ARGS=(
  --silent
  --output /dev/null
  --write-out '%{http_code}'
  --max-time "${TIMEOUT}"
  --request POST
  --header 'Content-Type: application/json'
  --data-binary "@${BODY_FILE}"
)
if [[ -n "${AUTH_TOKEN:-}" ]]; then
  AUTH_CONFIG="${WORKDIR}/curl.cfg"
  (
    umask 077
    printf 'header = "Authorization: Bearer %s"\n' "${AUTH_TOKEN}" >"${AUTH_CONFIG}"
  )
  CURL_ARGS+=(--config "${AUTH_CONFIG}")
fi

# Perform one quote request and record "<completion_epoch_ms> <http_status>".
# Transport failures (connection refused, timeout) surface as status 000.
request_worker() {
  local idx="$1" code
  code="$(curl "${CURL_ARGS[@]}" "${QUOTE_URL}" 2>/dev/null)" || code="000"
  printf '%s %s\n' "$(date +%s%3N)" "${code:-000}" >"${RESULTS_DIR}/${idx}"
}

INTERRUPTED=0
on_signal() { INTERRUPTED=1; }
trap on_signal INT TERM

START_MS="$(date +%s%3N)"
DEADLINE_MS=$((START_MS + DURATION * 1000))
LAST_HB_MS="${START_MS}"
SENT=0

echo "Probing ${QUOTE_URL} at ${RATE} rps for ${DURATION}s (max-gap ${MAX_GAP}s, timeout ${TIMEOUT}s)" >&2

while ((INTERRUPTED == 0)); do
  now_ms="$(date +%s%3N)"
  ((now_ms >= DEADLINE_MS)) && break

  tick_ms=$((START_MS + SENT * INTERVAL_MS))
  if ((tick_ms > now_ms)); then
    sleep "$(awk -v d=$((tick_ms - now_ms)) 'BEGIN { printf "%.3f", d / 1000 }')" || true
    ((INTERRUPTED == 0)) || break
    now_ms="$(date +%s%3N)"
  fi

  request_worker "${SENT}" &
  SENT=$((SENT + 1))

  if ((now_ms - LAST_HB_MS >= 10000)); then
    echo "[probe] $(((now_ms - START_MS) / 1000))s/${DURATION}s elapsed, ${SENT} requests sent" >&2
    LAST_HB_MS="${now_ms}"
  fi
done

# Let in-flight requests finish (bounded by --timeout) so their outcomes count.
wait || true

END_MS="$(date +%s%3N)"

summarize() {
  local all counts gap
  all="$(cat "${RESULTS_DIR}"/* 2>/dev/null || true)"

  # Counts: total and per-status-class buckets over every recorded response.
  counts="$(printf '%s\n' "${all}" | awk '
    NF == 0 { next }
    {
      total++
      cls = substr($2, 1, 1)
      if ($2 == "000") err++
      else if (cls == "2") c2++
      else if (cls == "3") c3++
      else if (cls == "4") c4++
      else if (cls == "5") c5++
      else other++
    }
    END {
      printf "%d %d %d %d %d %d %d\n", \
        total + 0, c2 + 0, c3 + 0, c4 + 0, c5 + 0, err + 0, other + 0
    }')"
  read -r TOTAL C2XX C3XX C4XX C5XX CERR COTHER <<<"${counts}"

  # Gap: largest interval with no successful (2xx) completion, bounded by the
  # probe window [start, end]. Sorted so out-of-order concurrent completions are
  # ordered by wall-clock time.
  gap="$(printf '%s\n' "${all}" |
    awk '$2 ~ /^2/ { print $1 }' | sort -n |
    awk -v start="${START_MS}" -v end="${END_MS}" -v thresh="${MAX_GAP}" '
      BEGIN { prev = start; max = 0 }
      { g = $1 - prev; if (g > max) max = g; prev = $1 }
      END {
        g = end - prev; if (g > max) max = g
        printf "%.3f %d\n", max / 1000, (max / 1000 <= thresh) ? 1 : 0
      }')"
  read -r MAX_GAP_S GAP_OK <<<"${gap}"

  NON_2XX=$((TOTAL - C2XX))
  local verdict="PASS" code=0
  if ((INTERRUPTED == 1)); then
    verdict="INTERRUPTED"
    code=130
  elif ((TOTAL == 0)); then
    verdict="FAIL"
    code=1
  elif ((NON_2XX != 0)) || ((GAP_OK == 0)); then
    verdict="FAIL"
    code=1
  fi

  local elapsed_s=$(((END_MS - START_MS) / 1000))

  printf '\n'
  printf 'result=%s total=%d non_2xx=%d class_2xx=%d class_3xx=%d class_4xx=%d class_5xx=%d class_err=%d max_gap_s=%s max_gap_threshold_s=%d elapsed_s=%d rate=%d\n' \
    "${verdict}" "${TOTAL}" "${NON_2XX}" "${C2XX}" "${C3XX}" "${C4XX}" "${C5XX}" "${CERR}" \
    "${MAX_GAP_S}" "${MAX_GAP}" "${elapsed_s}" "${RATE}"
  printf '\n'
  printf '  %-24s %s\n' "verdict" "${verdict}"
  printf '  %-24s %d\n' "total requests" "${TOTAL}"
  printf '  %-24s %d\n' "2xx (success)" "${C2XX}"
  printf '  %-24s %d\n' "3xx" "${C3XX}"
  printf '  %-24s %d\n' "4xx" "${C4XX}"
  printf '  %-24s %d\n' "5xx" "${C5XX}"
  printf '  %-24s %d\n' "transport errors" "${CERR}"
  if ((COTHER > 0)); then
    printf '  %-24s %d\n' "other status" "${COTHER}"
  fi
  printf '  %-24s %d\n' "non-2xx (dropped)" "${NON_2XX}"
  printf '  %-24s %ss (threshold %ds)\n' "max success gap" "${MAX_GAP_S}" "${MAX_GAP}"
  printf '  %-24s %ds\n' "elapsed" "${elapsed_s}"

  return "${code}"
}

summarize
