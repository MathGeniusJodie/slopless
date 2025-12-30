#!/bin/bash

# loop through pages 1 to 5992
for i in $(seq 1 5992); do
    echo "Fetching page $i of 5992..."
    curl -s "https://wiby.org/?q=the&p=$i" | \
    htmlq -a href a | \
    grep -E '^https?://' | \
    sed -E 's|^https?://([^/]+).*|\1|' | \
    sort -u >> "wiby.txt"
done

sort -u wiby.txt -o wiby.txt