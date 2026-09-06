#!/usr/bin/env bash
# Deploy the infrastructure tier on infra-1: PostgreSQL, Redis, and SeaweedFS (S3).
#
# Containers are named after the topology resource IDs, so a runbook acting on
# `redis-mq` finds its container without a second lookup table.

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

log "deploying infra tier on $INFRA_VM ($INFRA_HOST)"
vm_root "$INFRA_VM" "mkdir -p /opt/broccoli/infra"

orb -m "$INFRA_VM" -u root tee /opt/broccoli/infra/s3.json >/dev/null <<JSON
{
  "identities": [
    {
      "name": "broccoli",
      "credentials": [
        { "accessKey": "$S3_ACCESS_KEY", "secretKey": "$S3_SECRET_KEY" }
      ],
      "actions": ["Admin", "Read", "Write", "List", "Tagging"]
    }
  ]
}
JSON

# SeaweedFS uses -ip both to advertise itself and to bind, so it cannot be given the
# machine address: the container does not own it and the master dies with
# "cannot assign requested address". The service name keeps master, filer, volume, and
# the S3 gateway talking over the compose network, while external clients reach the S3
# API through the published port -- the gateway proxies rather than redirecting, so no
# internal name ever has to resolve off-box.
orb -m "$INFRA_VM" -u root tee /opt/broccoli/infra/docker-compose.yml >/dev/null <<YAML
name: broccoli-infra
services:
  postgres-main:
    image: postgres:17-alpine
    container_name: postgres-main
    restart: unless-stopped
    ports: ['0.0.0.0:$PG_PORT:5432']
    environment:
      POSTGRES_USER: $PG_USER
      POSTGRES_PASSWORD: $PG_PASSWORD
      POSTGRES_DB: $PG_DB
    volumes: ['pg_data:/var/lib/postgresql/data']
    healthcheck:
      test: ['CMD-SHELL', 'pg_isready -U $PG_USER -d $PG_DB']
      interval: 5s
      timeout: 5s
      retries: 5

  redis-mq:
    image: redis:7-alpine
    container_name: redis-mq
    restart: unless-stopped
    command: ['redis-server', '--requirepass', '$REDIS_PASSWORD']
    ports: ['0.0.0.0:$REDIS_PORT:6379']
    volumes: ['redis_data:/data']
    healthcheck:
      test: ['CMD', 'redis-cli', '-a', '$REDIS_PASSWORD', 'ping']
      interval: 5s
      timeout: 5s
      retries: 5

  seaweedfs-storage:
    image: chrislusf/seaweedfs:4.15
    container_name: seaweedfs-storage
    restart: unless-stopped
    command: ['server', '-dir=/data', '-s3', '-s3.config=/etc/seaweedfs/s3.json', '-ip=seaweedfs-storage']
    ports:
      - '0.0.0.0:$SEAWEED_MASTER_PORT:9333'
      - '0.0.0.0:$SEAWEED_S3_PORT:8333'
    volumes:
      - 'seaweed_data:/data'
      - '/opt/broccoli/infra/s3.json:/etc/seaweedfs/s3.json:ro'
    healthcheck:
      test: ['CMD', 'wget', '-q', '-O', '/dev/null', 'http://127.0.0.1:9333/']
      interval: 3s
      timeout: 3s
      retries: 20

  # The blob bucket is not created by the server: docker-compose.e2e.yml creates it with
  # a one-shot weed shell, and the first upload fails without it. This runs inside the
  # compose network, so it reaches the filer on its internal port.
  seaweedfs-init:
    image: chrislusf/seaweedfs:4.15
    container_name: seaweedfs-init
    depends_on:
      seaweedfs-storage:
        condition: service_healthy
    entrypoint: ['sh', '-c']
    command:
      - "printf 's3.bucket.create -name $S3_BUCKET\\n' | weed shell -master=seaweedfs-storage:9333 -filer=seaweedfs-storage:8888"
    restart: 'no'

volumes:
  pg_data:
  redis_data:
  seaweed_data:
YAML

vm_root "$INFRA_VM" "cd /opt/broccoli/infra && docker compose up -d"

log "waiting for infra to report healthy"
for _ in $(seq 1 60); do
    state="$(vm_root "$INFRA_VM" "cd /opt/broccoli/infra && docker compose ps --format '{{.Name}} {{.State}} {{.Health}}'" 2>/dev/null || true)"
    [ "$(echo "$state" | grep -c healthy)" -ge 2 ] && break
    sleep 2
done
vm_root "$INFRA_VM" "cd /opt/broccoli/infra && docker compose ps --format '  {{.Name}}  {{.State}}  {{.Health}}'"

log "blob bucket"
vm_root "$INFRA_VM" "docker logs seaweedfs-init 2>&1 | tail -2" || true

log "reachability from this Mac"
for port in "$PG_PORT" "$REDIS_PORT" "$SEAWEED_S3_PORT" "$SEAWEED_MASTER_PORT"; do
    if nc -z -G 3 "$INFRA_HOST" "$port" 2>/dev/null; then
        echo "  $INFRA_HOST:$port open"
    else
        echo "  $INFRA_HOST:$port CLOSED"
    fi
done
