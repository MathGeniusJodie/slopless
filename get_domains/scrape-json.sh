#!/bin/bash

curl -sSL "$1" | jq -r "$3" | \
awk -F/ '{
    if ($1 ~ /^http(s?):$/) {
        print $3
    } else {
        print $1
    }
}' | sort -u > "$2"