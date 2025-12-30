#!/bin/bash

# Define the target URL
URL="$1"
OUTPUT_FILE="$2"

echo "Fetching links from $URL..."

# 1. Download the page with curl
# 2. Use htmlq to extract the 'href' attribute from all 'a' tags
# 3. Use grep to keep only absolute URLs (starting with http)
# 4. Use sed to strip the protocol (http/https) and everything after the domain
# 5. Sort and remove duplicates
curl -s "$URL" | \
htmlq -a href a | \
grep -E '^https?://' | \
sed -E 's|^https?://([^/]+).*|\1|' | \
sort -u > "$OUTPUT_FILE"

echo "Done! Domains saved to $OUTPUT_FILE"
echo "Total unique domains found: $(wc -l < "$OUTPUT_FILE")"