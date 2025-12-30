#!/bin/bash
# Download and extract only the domain and subdomains from each URL, no intermediate file
curl -sSL "$1" |
awk -F/ '{
    if ($1 ~ /^http(s?):$/) {
        print $3
    } else {
        print $1
    }
}' > "$2"