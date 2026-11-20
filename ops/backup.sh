#!/usr/bin/env bash
# For a fenced PITR sibling, pass its provider-confirmed restoration timestamp.
# The connection uses libpq's standard PGHOST, PGPORT, PGUSER and PGPASSFILE.
set -euo pipefail
umask 077

asset=database.dump.age
format=solo-tracker-pg-custom-age-v1
prefix=solo
point=''
if [[ "${1:-}" == --deletions ]]; then
    asset=deletions.csv.age
    format=solo-tracker-identity-billing-ledger-csv-age-v2
    prefix=solo-deletions
    shift
elif [[ "${1:-}" == --pitr-at ]]; then
    if [[ $# -ne 3 || -z "$2" ]]; then
        echo 'Usage: ops/backup.sh --pitr-at YYYY-MM-DDTHH:MM:SSZ BACKUP_DIRECTORY' >&2
        exit 2
    fi
    point=$2
    shift 2
fi
if [[ $# -ne 1 ]]; then
    echo 'Usage: ops/backup.sh [--deletions | --pitr-at YYYY-MM-DDTHH:MM:SSZ] BACKUP_DIRECTORY' >&2
    exit 2
fi
: "${PGDATABASE:?Set PGDATABASE to the source database name}"
: "${AGE_RECIPIENT:?Set AGE_RECIPIENT to the off-account recovery public key}"

backup_root=$(cd -- "$1" && pwd -P)
started=$(date -u +'%Y-%m-%dT%H:%M:%SZ')
if [[ -n "$point" ]]; then
    canonical=$(date -u --date="$point" +'%Y-%m-%dT%H:%M:%SZ' 2>/dev/null) || canonical=''
    if [[ ! "$point" =~ ^[0-9]{4}- || "$point" != "$canonical" || "$point" > "$started" ]]; then
        echo 'PITR time must be a valid UTC timestamp in YYYY-MM-DDTHH:MM:SSZ form at or before the capture time.' >&2
        exit 2
    fi
fi
staging=$(mktemp -d -- "$backup_root/.incomplete-XXXXXXXXXX")
trap 'rm -rf -- "$staging"' EXIT

# A single SELECT captures the three related sets in one database snapshot.
if [[ "$asset" == deletions.csv.age ]]; then
    psql --no-password --no-psqlrc --quiet --set ON_ERROR_STOP=1 <<'SQL'
COPY (
    SELECT 'deletion' AS kind, user_id, deleted_at AS recorded_at,
           NULL::uuid AS event_id, NULL::text AS customer_id, NULL::boolean AS provider_live
    FROM account_deletions
    UNION ALL
    SELECT 'identity', user_id, recovered_at, event_id, NULL, NULL
    FROM auth_recovery_events
    UNION ALL
    SELECT 'shutdown', owner_id, created_at, NULL, customer_id, provider_live
    FROM billing_shutdowns
    ORDER BY kind, user_id
) TO STDOUT WITH CSV;
SQL
else
    pg_dump --no-password --format=custom --no-owner --no-privileges
fi | age --encrypt --recipient "$AGE_RECIPIENT" > "$staging/$asset"

finished=$(date -u +'%Y-%m-%dT%H:%M:%SZ')
{
    printf 'format=%s\n' "$format"
    printf 'started_utc=%s\ncompleted_utc=%s\n' "$started" "$finished"
    if [[ -n "$point" ]]; then
        printf 'source_recovery_utc=%s\n' "$point"
    fi
    printf 'pg_dump=%s\n' "$(pg_dump --version)"
} > "$staging/manifest.txt"
(
    cd -- "$staging"
    sha256sum "$asset" manifest.txt > SHA256SUMS
)
archive="$backup_root/$prefix-${started//:/-}-${staging##*-}"
mv --no-clobber --no-target-directory -- "$staging" "$archive"
if [[ -d "$staging" ]]; then
    echo 'Backup directory collision; retry the backup.' >&2
    exit 1
fi
printf '%s\n' "$archive"
