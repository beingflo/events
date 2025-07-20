#!/usr/bin/env bash
die() { echo "$*" 1>&2 ; exit 1; }

echo -e "Deploying events to production!"

[ -z "$(git status --porcelain)" ] || die "There are uncommitted changes"

docker --context arm compose --file compose.prod.yml pull || die "Failed to pull new image"

# Pass .env.prod such that docker substitutes the clickhouse credentials in the inline config
# This is NOT using the env_file directive on the otel-collector service
docker --context arm compose --env-file .env.prod --file compose.prod.yml up -d || die "Failed to bring compose up"