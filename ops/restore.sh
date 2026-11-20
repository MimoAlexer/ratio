#!/usr/bin/env bash
# Recover a trusted backup to a new, isolated database on the selected cluster.
# Give the latest independent identity, erasure and billing ledger for cutover.
set -euo pipefail
umask 077

if [[ $# -lt 2 || $# -gt 3 ]]; then
    echo 'Usage: ops/restore.sh ARCHIVE_DIRECTORY NEW_DATABASE_NAME [LATEST_DELETIONS_DIRECTORY]' >&2
    exit 2
fi
: "${PGDATABASE:?Set PGDATABASE to the target cluster maintenance database}"
: "${AGE_IDENTITY_FILE:?Set AGE_IDENTITY_FILE to a recovery identity file}"

target=$2
if [[ ! "$target" =~ ^[a-z][a-z0-9_]{0,62}$ ]] ||
    [[ ! "$PGDATABASE" =~ ^[a-zA-Z0-9_.-]{1,63}$ ]]; then
    echo 'Use a simple maintenance database name and a lowercase SQL identifier for the new database.' >&2
    exit 2
fi
identity=$(realpath -e -- "$AGE_IDENTITY_FILE")
archive=$(realpath -e -- "$1")
verify_bundle() (
    cd -- "$1"
    # Compute expected file entries using fixed filenames from this script.
    if ! sha256sum "$2" manifest.txt | cmp -s - SHA256SUMS; then
        echo 'Archive checksum verification failed.' >&2
        exit 1
    fi
    if ! grep -Fxq "format=$3" manifest.txt; then
        echo 'Unrecognized backup format.' >&2
        exit 1
    fi
    # Read to EOF to authenticate the whole ciphertext before creating the DB.
    age --decrypt --identity "$identity" "$2" > /dev/null
)
verify_bundle "$archive" database.dump.age solo-tracker-pg-custom-age-v1
if [[ $# -eq 3 ]]; then
    ledger=$(realpath -e -- "$3")
    if grep -Fxq 'format=solo-tracker-deletions-csv-age-v1' "$ledger/manifest.txt"; then
        echo 'This legacy ledger contains only erasures. Cutover requires a complete v2 ledger, including recovery events and billing shutdowns.' >&2
        exit 1
    fi
    verify_bundle "$ledger" deletions.csv.age solo-tracker-identity-billing-ledger-csv-age-v2
    dump_start=$(sed -n 's/^started_utc=//p' "$archive/manifest.txt")
    ledger_start=$(sed -n 's/^started_utc=//p' "$ledger/manifest.txt")
    source_time=$dump_start
    if grep -q '^source_recovery_utc=' "$archive/manifest.txt"; then
        source_time=$(sed -n 's/^source_recovery_utc=//p' "$archive/manifest.txt")
    fi
    for stamp in "$dump_start" "$source_time" "$ledger_start"; do
        canonical=$(date -u --date="$stamp" +'%Y-%m-%dT%H:%M:%SZ' 2>/dev/null) || canonical=''
        if [[ ! "$stamp" =~ ^[0-9]{4}- || "$stamp" != "$canonical" ]]; then
            echo 'Invalid UTC timestamp in the backup or deletion ledger manifest.' >&2
            exit 1
        fi
    done
    if [[ "$source_time" > "$dump_start" || "$ledger_start" < "$source_time" ]]; then
        echo 'Use the latest deletion ledger captured at or after the source recovery point.' >&2
        exit 1
    fi
fi

createdb --no-password --maintenance-db="$PGDATABASE" --template=template0 "$target"
if ! age --decrypt --identity "$identity" "$archive/database.dump.age" |
        pg_restore --no-password --dbname="$target" --exit-on-error \
            --single-transaction --no-owner --no-privileges; then
    echo "Restore failed. Inspect the new database $target before retrying with a fresh name." >&2
    exit 1
fi
if [[ $# -eq 2 ]]; then
    printf 'Quarantined raw restore %s retains historical sessions and accounts. Keep it offline.\n' "$target"
    exit 0
fi
target_sql() {
    PGDATABASE="$target" psql --no-password --no-psqlrc --quiet --set ON_ERROR_STOP=1 "$@"
}
supports_ledger=$(target_sql -At -c "SELECT to_regclass('public.auth_recovery_events') IS NOT NULL
    AND to_regclass('public.billing_shutdowns') IS NOT NULL
    AND EXISTS (SELECT 1 FROM information_schema.columns WHERE table_schema='public'
                AND table_name='users' AND column_name='identity_current')")
if [[ "$supports_ledger" != t ]]; then
    echo "Legacy schema restored to $target. Keep it offline, apply the current app migrations, and back it up with --pitr-at $source_time before retrying this ledger with a new database name." >&2
    exit 1
fi
ledger_schema=$(cat <<'SQL'
CREATE TEMP TABLE recovery_ledger (
  kind text NOT NULL, owner_id uuid NOT NULL, recorded_at timestamptz NOT NULL,
  event_id uuid, customer_id text, provider_live boolean,
  PRIMARY KEY (kind, owner_id),
  CHECK ((kind='deletion' AND num_nonnulls(event_id,customer_id,provider_live)=0)
      OR (kind='identity' AND event_id IS NOT NULL AND customer_id IS NULL AND provider_live IS NULL)
      OR (kind='shutdown' AND event_id IS NULL AND customer_id IS NOT NULL AND provider_live IS NOT NULL))
);
SQL
)
reconcile=$(cat <<'SQL'
INSERT INTO account_deletions (user_id,deleted_at)
SELECT owner_id,recorded_at FROM recovery_ledger WHERE kind='deletion'
ON CONFLICT (user_id) DO NOTHING;

-- Compare bindings before deleting owners. The constraints reject conflicting
-- customer IDs or provider modes for an owner and customer reuse across owners.
CREATE TEMP TABLE shutdown_candidates (
  owner_id uuid PRIMARY KEY, customer_id text NOT NULL UNIQUE, provider_live boolean NOT NULL
);
INSERT INTO shutdown_candidates
SELECT owner_id,customer_id,provider_live FROM recovery_ledger WHERE kind='shutdown'
UNION
SELECT a.owner_id,a.customer_id,a.provider_live FROM billing_accounts a
JOIN account_deletions d ON d.user_id=a.owner_id WHERE a.customer_id IS NOT NULL
UNION
SELECT owner_id,customer_id,provider_live FROM billing_shutdowns;
INSERT INTO billing_shutdowns (owner_id,customer_id,provider_live)
SELECT owner_id,customer_id,provider_live FROM shutdown_candidates
ON CONFLICT (owner_id) DO UPDATE SET next_attempt_at=now();
DELETE FROM users USING account_deletions WHERE users.id=account_deletions.user_id;

-- Mark stale inboxes before copying the authoritative marker. Repeated restores
-- can refresh markers but only a verified operator decision releases a hold.
UPDATE users u SET identity_current=false FROM recovery_ledger l
WHERE l.kind='identity' AND u.id=l.owner_id AND l.event_id IS DISTINCT FROM
  (SELECT e.event_id FROM auth_recovery_events e WHERE e.user_id=u.id);
INSERT INTO auth_recovery_events (user_id,event_id,recovered_at)
SELECT l.owner_id,l.event_id,l.recorded_at FROM recovery_ledger l
JOIN users u ON u.id=l.owner_id WHERE l.kind='identity'
ON CONFLICT (user_id) DO UPDATE SET event_id=EXCLUDED.event_id,recovered_at=EXCLUDED.recovered_at;

TRUNCATE login_challenges,user_sessions,login_rate_limits,account_recovery;
DELETE FROM github_oauth_states;
DELETE FROM github_connections;
-- Consent or delivery could have changed after the snapshot. Keep the in-app
-- schedule and sent history. An owner can opt back into future email delivery.
UPDATE portfolio_digest_schedules SET email_enabled=false WHERE email_enabled;
UPDATE portfolio_digest_email_outbox SET status='canceled',lease_id=NULL WHERE status='pending';
SQL
)
if ! age --decrypt --identity "$identity" "$ledger/deletions.csv.age" |
    target_sql --single-transaction -c "$ledger_schema" \
        -c 'COPY recovery_ledger (kind,owner_id,recorded_at,event_id,customer_id,provider_live) FROM STDIN WITH CSV' \
        -c "$reconcile"; then
    echo "Cutover preparation failed. Keep the recovered database $target offline." >&2
    exit 1
fi
printf 'Restored %s with digest email disabled. Verify identity holds, erasures, billing shutdowns and the independent ledger through cutover using ops/railway.md.\n' "$target"
