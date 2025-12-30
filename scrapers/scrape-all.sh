#!/bin/bash

./scrape-anchor-tags.sh "https://thenumb.at/Graphics-Blogroll/" "thenumb.txt"
./scrape-txt-dirty.sh "https://raw.githubusercontent.com/kagisearch/smallweb/refs/heads/main/smallweb.txt" "kagi-smallweb.txt"
./scrape-anchor-tags.sh "https://jodie.website" "jodie.txt"

./scrape-smallweb.sh "https://smallweb.cc/?sort=top&page=1" "smallweb-page1.txt"
./scrape-smallweb.sh "https://smallweb.cc/?sort=top&page=2" "smallweb-page2.txt"
# todo: automate more pages?

./scrape-anchor-tags.sh "https://xn--sr8hvo.ws/directory" "sr8hvo.txt"