#!/bin/sh
# A stand-in for docker in Tern's lab: it lists one running container,
# lab-app, and runs what's asked "inside" it on this server itself, with
# TERN_CONTAINER set, so Tern's container checks have something to find.
case "$1" in
  ps)
    echo "docker|c0ffee00|lab-app|debian:bookworm-slim"
    ;;
  exec)
    shift
    # Options (-i, -t, -it), then the container.
    while [ "${1#-}" != "$1" ]; do shift; done
    shift
    TERN_CONTAINER=lab-app exec "$@"
    ;;
  *)
    echo "the lab's docker only lists lab-app and runs things in it" >&2
    exit 1
    ;;
esac
