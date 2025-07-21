#!/usr/bin/env bash
die() { echo "$*" 1>&2 ; exit 1; }

./build.sh || die "Failed to build docker image"
./deploy.sh || die "Failed to deploy to server"