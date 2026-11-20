#!/usr/bin/env bash
# Rehearse local logical recovery on a disposable cluster. Requires CREATEDB.
set -euo pipefail
umask 077
: "${PGDATABASE:?Set PGDATABASE to the disposable cluster maintenance database}"
: "${PGHOST:?Set PGHOST to the disposable cluster address}"

ops=$(cd -- "$(dirname -- "$0")" && pwd -P)
scratch=$(mktemp -d)
nonce=$(od -An -N8 -tx1 /dev/urandom | tr -d ' \n')
source_db=solo_drill_source_$nonce
restored_db=solo_drill_recovered_$nonce
sibling_db=solo_drill_sibling_$nonce
pitr_db=solo_drill_pitr_$nonce
created_source=false
created_restored=false
created_sibling=false
created_pitr=false
cleanup() {
    if [[ "$created_pitr" == true ]]; then
        dropdb --no-password --maintenance-db="$PGDATABASE" "$pitr_db"
    fi
    if [[ "$created_sibling" == true ]]; then
        dropdb --no-password --maintenance-db="$PGDATABASE" "$sibling_db"
    fi
    if [[ "$created_restored" == true ]]; then
        dropdb --no-password --maintenance-db="$PGDATABASE" "$restored_db"
    fi
    if [[ "$created_source" == true ]]; then
        dropdb --no-password --maintenance-db="$PGDATABASE" "$source_db"
    fi
    rm -rf -- "$scratch"
}
trap cleanup EXIT

sql() { psql --no-psqlrc --no-password --quiet --tuples-only --no-align --set ON_ERROR_STOP=1 "$@"; }
createdb --no-password --maintenance-db="$PGDATABASE" --template=template0 "$source_db"
created_source=true
for migration in "$ops"/../migrations/*.sql; do
    PGDATABASE="$source_db" sql --single-transaction --file="$migration"
done
PGDATABASE="$source_db" sql <<'SQL'
INSERT INTO users(email) VALUES ('keeper@example.test') RETURNING id \gset keeper_
INSERT INTO users(email) VALUES ('erased@example.test') RETURNING id \gset erased_
INSERT INTO users(email) VALUES ('closed@example.test') RETURNING id \gset closed_
INSERT INTO users(email,identity_current) VALUES
('first-recovery@example.test',true),('new-recovery@example.test',true),
('current@example.test',true),('already-held@example.test',false);
INSERT INTO auth_recovery_events(user_id,event_id)
SELECT id,gen_random_uuid() FROM users WHERE email IN
('new-recovery@example.test','current@example.test','already-held@example.test');
INSERT INTO billing_accounts(owner_id,customer_id,provider_live)
VALUES (:'closed_id','cus_snapshot_fixture',false);
UPDATE users SET plan='max' WHERE id=:'keeper_id';
INSERT INTO portfolio_digest_schedules(owner_id,enabled,email_enabled,next_week)
VALUES (:'keeper_id',true,true,'2024-01-15');
INSERT INTO portfolio_digests(owner_id,week_start,new_issues,issues_done,milestone_targets,releases_published)
VALUES (:'keeper_id','2024-01-01',1,0,0,0),(:'keeper_id','2024-01-08',0,1,0,0);
INSERT INTO portfolio_digest_email_outbox(owner_id,week_start,status,lease_id)
VALUES (:'keeper_id','2024-01-01','sent',NULL),(:'keeper_id','2024-01-08','pending',gen_random_uuid());
INSERT INTO projects(owner_id,name,key) VALUES (:'erased_id','Private project','ERASE');
INSERT INTO projects(owner_id,name,key) VALUES (:'keeper_id','Recovery drill','TEST') RETURNING id \gset proj_
INSERT INTO issues(owner_id,project_id,number,title,status)
VALUES (:'keeper_id',:'proj_id',1,'Completed work','done'),(:'keeper_id',:'proj_id',2,'Carryover','ready');
INSERT INTO releases(owner_id,project_id,name) VALUES (:'keeper_id',:'proj_id','Pilot') RETURNING id \gset rel_
INSERT INTO release_membership_events(owner_id,project_id,release_id,issue_id,in_scope,status)
SELECT owner_id, project_id, :'rel_id', id, true, status FROM issues;
INSERT INTO notes(owner_id,project_id,title,path,body) VALUES
(:'keeper_id',:'proj_id','Status','Status.md','![pixel](assets/pixel.png)');
WITH image AS (
  SELECT decode('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+ip1sAAAAASUVORK5CYII=','base64') AS bytes
)
INSERT INTO transfer_blobs(owner_id,project_id,path,content_type,bytes,sha256)
SELECT :'keeper_id', :'proj_id', 'assets/pixel.png','image/png', bytes, encode(sha256(bytes),'hex') FROM image;
INSERT INTO user_sessions(user_id,token_hash) VALUES (:'keeper_id',sha256('recovery-drill'::bytea));
INSERT INTO login_challenges(email,token_hash) VALUES ('keeper@example.test',sha256('login'::bytea));
INSERT INTO account_recovery(user_id,code_hash) VALUES (:'keeper_id',sha256('recovery-code'::bytea));
INSERT INTO github_oauth_states(owner_id,state_hash,session_hash,expires_at)
VALUES (:'keeper_id',sha256('state'::bytea),sha256('session'::bytea),now()+interval '1 hour');
INSERT INTO github_connections(owner_id,user_remote_id,access_cipher)
VALUES (:'keeper_id',1,'fixture-only'::bytea);
INSERT INTO account_deletions(user_id) VALUES (gen_random_uuid());
SQL
export AGE_IDENTITY_FILE="$scratch/age-key"
age-keygen --output "$AGE_IDENTITY_FILE" 2>/dev/null
export AGE_RECIPIENT
AGE_RECIPIENT=$(age-keygen -y "$AGE_IDENTITY_FILE")
start=$(date +%s%N)
archive=$(PGDATABASE="$source_db" "$ops/backup.sh" "$scratch")
dumped=$(date +%s%N)

# Stand in for a fenced historical Railway sibling with a physical database
# clone of the same fixture. Keep the source watermark and ledger time distinct.
createdb --no-password --maintenance-db="$PGDATABASE" --template="$source_db" "$sibling_db"
created_sibling=true
point=$(sed -n 's/^started_utc=//p' "$archive/manifest.txt")
sleep 1
# Erasure happens after the archive. Capture a separately encrypted, newer ledger.
PGDATABASE="$source_db" sql --single-transaction <<'SQL'
-- This customer and its pending cancellation did not exist in the snapshot.
INSERT INTO billing_shutdowns(owner_id,customer_id,provider_live)
SELECT id,'cus_after_snapshot_fixture',true FROM users WHERE email='erased@example.test';
INSERT INTO account_deletions(user_id) SELECT id FROM users WHERE email IN ('erased@example.test','closed@example.test');
DELETE FROM users WHERE email IN ('erased@example.test','closed@example.test');
INSERT INTO auth_recovery_events(user_id,event_id)
SELECT id,'33333333-3333-4333-8333-333333333333' FROM users
WHERE email IN ('first-recovery@example.test','new-recovery@example.test')
ON CONFLICT (user_id) DO UPDATE SET event_id=EXCLUDED.event_id,recovered_at=now();
UPDATE users SET email=split_part(email,'@',1)||'-verified@example.test'
WHERE email IN ('first-recovery@example.test','new-recovery@example.test');
SQL
ledger=$(PGDATABASE="$source_db" "$ops/backup.sh" --deletions "$scratch")

recovering=$(date +%s%N)
"$ops/restore.sh" "$archive" "$restored_db" "$ledger"
created_restored=true

# Recover the chart point from its source events, verify the client's image path,
# recompute the stored digest and check erasure plus session invalidation.
verify_fixture() {
    local result
    result=$(PGDATABASE="$1" sql <<'SQL'
SELECT count(*) FILTER (WHERE e.in_scope), count(*) FILTER (WHERE e.in_scope AND e.status='done')
FROM releases r JOIN release_membership_events e ON e.release_id=r.id WHERE r.name='Pilot';
SELECT count(*),bool_and(b.sha256=encode(sha256(b.bytes),'hex')
    AND b.sha256='5e3d382db4dd83d59aa5742793ad6b7903409e865c83bcbc54835049f043bc15')
FROM notes n JOIN transfer_blobs b ON b.project_id=n.project_id
WHERE n.body='![pixel](' || b.path || ')';
SELECT (SELECT count(*) FROM account_deletions),(SELECT count(*) FROM users),
       (SELECT count(*) FROM projects WHERE key='ERASE');
SELECT (SELECT count(*) FROM user_sessions),(SELECT count(*) FROM login_challenges),
       (SELECT count(*) FROM account_recovery),(SELECT count(*) FROM github_oauth_states),
       (SELECT count(*) FROM github_connections);
SELECT count(*) FILTER (WHERE identity_current),count(*) FILTER (WHERE NOT identity_current),
       count(*) FILTER (WHERE u.email IN ('first-recovery@example.test','new-recovery@example.test')
          AND NOT identity_current AND e.event_id='33333333-3333-4333-8333-333333333333')
FROM users u LEFT JOIN auth_recovery_events e ON e.user_id=u.id;
SELECT count(*) FROM billing_shutdowns WHERE (customer_id='cus_after_snapshot_fixture' AND provider_live)
    OR (customer_id='cus_snapshot_fixture' AND NOT provider_live);
SELECT (SELECT count(*) FROM portfolio_digest_schedules WHERE enabled AND NOT email_enabled),
       count(*) FILTER (WHERE status='canceled' AND lease_id IS NULL),
       count(*) FILTER (WHERE status='sent'),count(*) FILTER (WHERE status='pending')
FROM portfolio_digest_email_outbox;
SQL
)
    if [[ "$result" != $'2|1\n1|t\n3|5|0\n0|0|0|0|0\n2|3|2\n2\n1|1|1|0' ]]; then
        echo "Restored data verification failed: $result" >&2
        exit 1
    fi
}
verify_fixture "$restored_db"
verified=$(date +%s%N)
printf 'Verified chart counts, linked PNG, restored access revocation, 2 erasures, 2 stale inbox holds, 2 billing shutdowns and withdrawn email consent.\n'
printf 'Local logical backup %d ms; restore through data checks %d ms; ciphertext %s bytes.\n' \
    "$(( (dumped - start)/1000000 ))" "$(( (verified - recovering)/1000000 ))" "$(stat -c %s "$archive/database.dump.age")"

# The off-account ledger predates this new logical dump of old state. Its lower
# bound must be the provider recovery point rather than today's copy timestamp.
sleep 1
pitr_copy=$(PGDATABASE="$sibling_db" "$ops/backup.sh" --pitr-at "$point" "$scratch")
ledger_time=$(sed -n 's/^started_utc=//p' "$ledger/manifest.txt")
copy_time=$(sed -n 's/^started_utc=//p' "$pitr_copy/manifest.txt")
[[ "$point" < "$ledger_time" && "$ledger_time" < "$copy_time" ]]
"$ops/restore.sh" "$pitr_copy" "$pitr_db" "$ledger"
created_pitr=true
verify_fixture "$pitr_db"
for invalid in '2001-02-30T00:00:00Z' '2024-01-01' "$(date -u --date=tomorrow +'%Y-%m-%dT%H:%M:%SZ')"; do
    if PGDATABASE="$sibling_db" "$ops/backup.sh" --pitr-at "$invalid" "$scratch" >/dev/null 2>&1; then
        echo "PITR backup accepted an invalid recovery point: $invalid" >&2
        exit 1
    fi
done
printf 'Verified sibling copy with intervening erasures and refused invalid, noncanonical and future PITR timestamps.\n'
