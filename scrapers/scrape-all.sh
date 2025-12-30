#!/bin/bash

./scrape-anchor-tags.sh "https://thenumb.at/Graphics-Blogroll/" "thenumb.txt"
./scrape-txt-dirty.sh "https://raw.githubusercontent.com/kagisearch/smallweb/refs/heads/main/smallweb.txt" "kagi-smallweb.txt"
./scrape-anchor-tags.sh "https://jodie.website" "jodie.txt"

./scrape-smallweb.sh "https://smallweb.cc/?sort=top&page=1" "smallweb-page1.txt"
./scrape-smallweb.sh "https://smallweb.cc/?sort=top&page=2" "smallweb-page2.txt"
# todo: automate more pages?

./scrape-anchor-tags.sh "https://xn--sr8hvo.ws/directory" "sr8hvo.txt"

python3 recurse_crawl.py https://ooh.directory -x https://ooh.directory/blog/ https://ooh.directory/feeds/ "https://o
oh.directory/updated/?d=" -o ooh.txt -w 20

./scrape-anchor-tags.sh "https://blogroll.org/" "blogrollorg.txt"
./scrape-anchor-tags.sh "https://1mb.club" "1mbclub.txt"

python3 recurse_crawl.py https://melonland.net/surf-club -o surfclub.txt -w 20

./scrape-json.sh "https://theinternetisshit.xyz/sites.json" "theinternetisshit.txt" '.sites[] .url'

# Not updated anymore so commented out, also take a long time to run
# python3 recurse_crawl.py https://longform.org/sections -o longform.txt -w 20 -x "https://longform.org/player/"
python3 recurse_crawl.py "https://webcurios.co.uk/all-curios/" -o webcurios.txt -w 20
python3 recurse_crawl.py "https://fromthesuperhighway.com/" -o fromthesuperhighway.txt -w 20

./scrape-txt-dirty.sh "https://raw.githubusercontent.com/MarginaliaSearch/PublicData/refs/heads/master/sets/blogs.txt" "marginalia-blogs.txt"
./scrape-txt-dirty.sh "https://raw.githubusercontent.com/MarginaliaSearch/PublicData/refs/heads/master/sets/random-domains.txt" "marginalia-random.txt"

python3 recurse_crawl.py "https://searchmysite.net/search/browse/" -o searchmysite.txt -w 20

./scrape-wiby.sh

./scrape-anchor-tags.sh "https://getindie.wiki/listings/" "getindiewiki.txt"