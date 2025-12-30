#!/bin/bash

# Check if n pages is provided, default to 1
SUBREDDIT=$1
PAGES=${2:-10}
#OUTPUT_FILE="reddit_iib.txt"
OUTPUT_FILE="${3:-reddit_${SUBREDDIT}.txt}"
TARGET_URL="https://old.reddit.com/r/$SUBREDDIT/top/?sort=top&t=all"
USER_AGENT="Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"



echo "Scraping $PAGES page(s) from r/InternetIsBeautiful..." >&2

CURRENT_URL=$TARGET_URL

LINK_LIST=()

for (( i=1; i<=$PAGES; i++ ))
do
    echo "Processing page $i..." >&2
    
    # Fetch the HTML content
    RESPONSE=$(curl -s -L -A "$USER_AGENT" "$CURRENT_URL")
    
    # Extract domains using htmlq
    LINK_LIST+=($(
        echo "$RESPONSE" | htmlq 'a.title.outbound' --attribute href
    ))
    
    # Find the 'next' button link for the next iteration
    NEXT_PAGE=$(echo "$RESPONSE" | htmlq 'span.next-button a' --attribute href)
    
    # Break if no next page is found
    if [ -z "$NEXT_PAGE" ]; then
        echo "No more pages found." >&2
        break
    fi
    
    CURRENT_URL="$NEXT_PAGE"
done

echo ${LINK_LIST[@]} | tr ' ' '\n' | \
grep -E '^https?://' | \
sed -E 's|^https?://([^/]+).*|\1|' | \
sort -u > "$OUTPUT_FILE"