#!/usr/bin/env bash
# Deploy the judge worker on judge-1.
#
# The worker sandboxes submissions with isolate, which needs privileged mode and both
# seccomp and apparmor unconfined -- the same flags docker-compose.e2e.yml uses. It has
# no inbound port: it reaches Redis, PostgreSQL and the blob store on infra-1 and is
# observed through those, which is why the topology gives it a coverage gap rather than
# a probe.

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

WORKER_IMAGE="broccoli-worker:testbed"

vm_root "$JUDGE_VM" "docker image inspect $WORKER_IMAGE >/dev/null 2>&1" || {
    warn "$WORKER_IMAGE is missing on $JUDGE_VM; run 03-build-images.sh first"
    exit 1
}

log "deploying broccoli worker on $JUDGE_VM ($JUDGE_HOST)"
vm_root "$JUDGE_VM" "mkdir -p /opt/broccoli/judge"

orb -m "$JUDGE_VM" -u root tee /opt/broccoli/judge/docker-compose.yml >/dev/null <<YAML
name: broccoli-judge
services:
  worker-1:
    image: $WORKER_IMAGE
    container_name: worker-1
    restart: unless-stopped
    privileged: true
    security_opt:
      - seccomp=unconfined
      - apparmor=unconfined
    environment:
      BROCCOLI__WORKER__ID: 'worker-1'
      BROCCOLI__WORKER__SANDBOX_BACKEND: 'isolate'
      BROCCOLI__WORKER__ISOLATE_BIN: 'isolate'
      BROCCOLI__WORKER__ENABLE_CGROUPS: 'true'
      BROCCOLI__DATABASE__URL: 'postgres://$PG_USER:$PG_PASSWORD@$INFRA_HOST:$PG_PORT/$PG_DB'
      BROCCOLI__MQ__URL: 'redis://:$REDIS_PASSWORD@$INFRA_HOST:$REDIS_PORT'
      BROCCOLI__MQ__ENABLED: 'true'
      BROCCOLI__STORAGE__BACKEND: 'object_storage'
      BROCCOLI__STORAGE__OBJECT_STORAGE__BUCKET: '$S3_BUCKET'
      BROCCOLI__STORAGE__OBJECT_STORAGE__REGION: 'us-east-1'
      BROCCOLI__STORAGE__OBJECT_STORAGE__ENDPOINT: 'http://$INFRA_HOST:$SEAWEED_S3_PORT'
      BROCCOLI__STORAGE__OBJECT_STORAGE__ACCESS_KEY: '$S3_ACCESS_KEY'
      BROCCOLI__STORAGE__OBJECT_STORAGE__SECRET_KEY: '$S3_SECRET_KEY'
      BROCCOLI__STORAGE__OBJECT_STORAGE__PATH_STYLE: 'true'
      BROCCOLI__STORAGE__DATA_DIR: '/data'
      RUST_LOG: 'info'
    volumes:
      - 'worker_data:/data'

volumes:
  worker_data:
YAML

vm_root "$JUDGE_VM" "cd /opt/broccoli/judge && docker compose up -d"
sleep 5
vm_root "$JUDGE_VM" "cd /opt/broccoli/judge && docker compose ps --format '  {{.Name}}  {{.State}}'"
log "recent worker log"
vm_root "$JUDGE_VM" "docker logs worker-1 2>&1 | tail -12"
