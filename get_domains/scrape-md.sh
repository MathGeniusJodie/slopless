#!/bin/bash

# Check if correct number of arguments are provided
if [ "$#" -ne 2 ]; then
    echo "Usage: $0 <url_to_markdown_file> <output_file.txt>"
    exit 1
fi

INPUT_URL=$1
OUTPUT_FILE=$2

echo "Downloading file and extracting domains..."

# 1. curl -s: Silently download the markdown content
# 2. grep -oE: Extract strings starting with http or https up to the next slash, space, or closing parenthesis
# 3. sed: Remove the "http://" or "https://" prefix
# 4. sort -u: Sort the list and remove duplicates
curl -s "$INPUT_URL" | \
grep -oE 'https?://[^/ )]+' | \
sed -E 's|https?://||' | \
sort -u > "$OUTPUT_FILE"

if [ $? -eq 0 ]; then
    echo "Success! Domains saved to $OUTPUT_FILE"
    echo "Total unique domains found: $(wc -l < "$OUTPUT_FILE")"
else
    echo "An error occurred."
fi