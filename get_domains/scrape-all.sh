#!/bin/bash

./scrape-anchor-tags.sh "https://thenumb.at/Graphics-Blogroll/" "thenumb.txt"
./scrape-txt-dirty.sh "https://raw.githubusercontent.com/kagisearch/smallweb/refs/heads/main/smallweb.txt" "kagi-smallweb.txt"
./scrape-anchor-tags.sh "https://jodie.website" "jodie.txt"

./scrape-smallweb.sh "https://smallweb.cc/?sort=top&page=1" "smallweb-page1.txt"
./scrape-smallweb.sh "https://smallweb.cc/?sort=top&page=2" "smallweb-page2.txt"
# todo: automate more pages?

./scrape-anchor-tags.sh "https://xn--sr8hvo.ws/directory" "sr8hvo.txt"

python3 recurse_crawl.py https://ooh.directory -x https://ooh.directory/blog/ https://ooh.directory/feeds/ "https://ooh.directory/updated/?d=" -o ooh.txt -w 20

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
./scrape-anchor-tags.sh "https://マリウス.com/the-small-web-101/" "marius.txt"

python3 recurse_crawl.py "https://fmhy.net" -o fmhy.txt -w 20

./scrape_subreddit.sh InternetIsBeautiful

./scrape-md.sh "https://raw.githubusercontent.com/atakanaltok/awesome-useful-websites/refs/heads/main/README.md" "atakanaltok.txt"

# make empty file
> minimalgallery.txt
for i in $(seq 1 121); do
    echo "Scraping minimal.gallery page $i..."
    ./scrape-anchor-tags.sh "https://minimal.gallery/page/$i/" "temp_minimalgallery.txt"
    cat temp_minimalgallery.txt >> minimalgallery.txt
    rm temp_minimalgallery.txt
done

./scrape-anchor-tags.sh "https://manuelmoreale.com/blogroll" "manuelmoreale.txt"
./scrape-anchor-tags.sh "https://webring.bucketfish.me/" "bucketfishwebring.txt"
./scrape-anchor-tags.sh "https://laughingmeme.org/links/" "laughingmeme.txt"
./scrape-anchor-tags.sh "https://maurycyz.com/real_pages/" "maurycyz.txt"

python3 recurse_crawl.py "https://randomdailyurls.com/archive" -o randomdailyurls.txt -w 20


# concat all txt files into one big file and dedupe
cat *.txt | sort -u > all_domains.txt
wc -l ./all_domains.txt
