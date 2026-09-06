#!/usr/bin/env bash
# Deploy the Broccoli server (and the frontend it serves) on app-1.
#
# Environment follows docker-compose.e2e.yml, with every dependency repointed from
# compose service names to infra-1 over the network -- which is exactly the difference
# the control plane is meant to observe.

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

SERVER_IMAGE="broccoli-server:testbed"

vm_root "$APP_VM" "docker image inspect $SERVER_IMAGE >/dev/null 2>&1" || {
    warn "$SERVER_IMAGE is missing on $APP_VM; run 03-build-images.sh first"
    exit 1
}

log "deploying broccoli-server on $APP_VM ($APP_HOST)"
vm_root "$APP_VM" "mkdir -p /opt/broccoli/app"

orb -m "$APP_VM" -u root tee /opt/broccoli/app/docker-compose.yml >/dev/null <<YAML
name: broccoli-app
services:
  broccoli-server:
    image: $SERVER_IMAGE
    container_name: broccoli-server
    restart: unless-stopped
    ports: ['0.0.0.0:$SERVER_PORT:3000']
    environment:
      BROCCOLI__SERVER__HOST: '0.0.0.0'
      BROCCOLI__SERVER__PORT: '3000'
      BROCCOLI__SERVER__ID: 'server-1'
      BROCCOLI__DATABASE__URL: 'postgres://$PG_USER:$PG_PASSWORD@$INFRA_HOST:$PG_PORT/$PG_DB'
      BROCCOLI__MQ__URL: 'redis://:$REDIS_PASSWORD@$INFRA_HOST:$REDIS_PORT'
      BROCCOLI__MQ__ENABLED: 'true'
      BROCCOLI__AUTH__JWT_SECRET: '$JWT_SECRET'
      # The console is served over plain HTTP, and browsers drop Secure cookies there.
      BROCCOLI__AUTH__SECURE_COOKIES: 'false'
      BROCCOLI__PLUGIN__PLUGINS_DIR: '/plugins'
      BROCCOLI__STORAGE__BACKEND: 'object_storage'
      BROCCOLI__STORAGE__OBJECT_STORAGE__BUCKET: '$S3_BUCKET'
      BROCCOLI__STORAGE__OBJECT_STORAGE__REGION: 'us-east-1'
      BROCCOLI__STORAGE__OBJECT_STORAGE__ENDPOINT: 'http://$INFRA_HOST:$SEAWEED_S3_PORT'
      BROCCOLI__STORAGE__OBJECT_STORAGE__ACCESS_KEY: '$S3_ACCESS_KEY'
      BROCCOLI__STORAGE__OBJECT_STORAGE__SECRET_KEY: '$S3_SECRET_KEY'
      BROCCOLI__STORAGE__OBJECT_STORAGE__PATH_STYLE: 'true'
      BROCCOLI__SUBMISSION__RATE_LIMIT_PER_MINUTE: '10000'
      # A first-run admin so the scenarios can drive the API without manual setup.
      BROCCOLI__BOOTSTRAP__ADMIN_USERNAME: '$ADMIN_USER'
      BROCCOLI__BOOTSTRAP__ADMIN_PASSWORD: '$ADMIN_PASSWORD'
      RUST_LOG: 'info'
    healthcheck:
      test: ['CMD', '/usr/local/bin/broccoli-server', '--healthcheck']
      interval: 5s
      timeout: 5s
      retries: 30
      start_period: 15s
YAML

vm_root "$APP_VM" "cd /opt/broccoli/app && docker compose up -d"

log "waiting for /healthz"
ok=0
for _ in $(seq 1 60); do
    code="$(curl -s -o /dev/null -w '%{http_code}' --max-time 4 "http://$APP_HOST:$SERVER_PORT/healthz" || true)"
    if [ "$code" = "200" ]; then ok=1; break; fi
    sleep 3
done

vm_root "$APP_VM" "cd /opt/broccoli/app && docker compose ps --format '  {{.Name}}  {{.State}}  {{.Health}}'"
if [ "$ok" = "1" ]; then
    log "http://$APP_HOST:$SERVER_PORT/healthz -> 200"
else
    warn "server did not become healthy; last 30 log lines:"
    vm_root "$APP_VM" "docker logs broccoli-server 2>&1 | tail -30"
    exit 1
fi
