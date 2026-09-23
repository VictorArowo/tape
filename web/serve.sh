#!/bin/sh
# Serves web/dist locally for checking the engine in a browser.
exec python3 -m http.server "${1:-8767}" --directory "$(dirname "$0")/dist"
