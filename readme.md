# Slopless
A search engine with currated sources free from SEO slop. This project also aims to recommend websites to other aggregators and search engines to help them improve their own results.

# Why?
Google and Bing suck now, they're overrun by SEO slop. The internet as most people know it is a cesspool of garbage. The good internet that we are all nostalgic for is still out there, but it's not easy to find. This project aims to fix that.

# Related Projects
* https://github.com/MathGeniusJodie/slopless_embeddings
* https://github.com/MathGeniusJodie/tl_readability

# Status
- [x] Automated scraping of safelist sources
- [ ] Automated scraping of blocklist sources
- [x] Crawler
- [x] HTML content extraction initial implementation
- [ ] Fancy HTML content extraction
- [ ] Cross-language support
- [ ] Vector embedding index
- [ ] ANN search
- [ ] Website UI

### Safelist Sources
* Independent blogs
* Reputable news sources
* High quality publications
* Well moderated forums and subreddits
* Academic journals and papers
* Wikis
* Online encyclopedias
* Open source repositories
* User customizable additions

### Careful Consideration
* substack
* neocities
* reddit
* https://lobste.rs/
* outgoing links from sites in safelist
* hackernews

### Blocklist Sources
* SEO slop
* Fake news and propaganda
* Clickbait sites
* Uncurrated and unmoderated websites
* User customizable additions

### To Scrape for Domains
- [x] https://raw.githubusercontent.com/kagisearch/smallweb/refs/heads/main/smallweb.txt
- [x] https://thenumb.at/Graphics-Blogroll/
- [ ] wikipedia citations
- [ ] various webrings, blogrolls and directories
    - [x] jodie.website
    - [x] https://smallweb.cc
    - [x] https://xn--sr8hvo.ws/directory
    - [x] https://ooh.directory/
    - [x] https://blogroll.org/
    - [x] https://1mb.club
    - [ ] https://indieweb.org/blogroll a blogroll of blogrolls
    - [x] https://melonland.net/surf-club
    - [x] https://theinternetisshit.xyz/
    - [ ] https://brisray.com/web/webring-list.htm
    - [ ] https://www.404pagefound.com/
    - [ ] https://webring.theoldnet.com/
- [ ] aggregator blogs
    - [x] https://longform.org/
    - [ ] https://www.metafilter.com
    - [x] https://webcurios.co.uk/
    - [x] https://fromthesuperhighway.com/
- [x] https://marginalia-search.com/
- [x] https://searchmysite.net/
- [x] wiby
- [ ] google maps listings of brick and mortar places
- [x] https://en.wikipedia.org/wiki/Wikipedia:Reliable_sources/Perennial_sources
- [x] https://getindie.wiki/listings/
- [x] https://mwmbl.org/
- [x] https://マリウス.com/the-small-web-101/
- [x] https://fmhy.net
- [x] https://www.reddit.com/r/InternetIsBeautiful/
- [x] github.com/atakanaltok/awesome-useful-websites?tab=readme-ov-file
- [x] https://manuelmoreale.com/blogroll
- [x] https://webring.bucketfish.me/
- [x] minimal.gallery
- [ ] https://theforest.link/
- [x] https://laughingmeme.org/links/
- [x] https://maurycyz.com/real_pages/
- [x] https://randomdailyurls.com/archive

### To Scrape for Blocklist
- [ ] https://en.wikipedia.org/wiki/List_of_fake_news_websites
- [ ] https://github.com/popcar2/BadWebsiteBlocklist
- [ ] https://github.com/rjaus/awesome-ublacklist
- [ ] https://github.com/NotaInutilis/Super-SEO-Spam-Suppressor
- [ ] https://github.com/NotaInutilis/no-qanon
- [ ] https://danny0838.github.io/content-farm-terminator/en/
- [ ] https://github.com/FranklyRocks/OnlyHuman

### Youtube Lists to Scrape
- [ ] https://github.com/PrejudiceNeutrino/YouTube_Channels
- [ ] https://github.com/ErikCH/DevYouTubeList
- [ ] https://educational-channels.com
- [ ] kagi small yt

### Implementation Todos
- [ ] Scraper for automatically finding new safelist sources
- [ ] Actual search engine implementation
- [ ] Rankings algorithm
- [ ] Website UI
- [ ] Investigate https://yacy.net/
- [ ] Investigate https://github.com/medialab/hyphe
- [ ] Offline/datahoarder mode? https://www.httrack.com/ https://en.wikipedia.org/wiki/Heritrix
- [ ] Don't crawl websites that have search pages of their own and integrate their search instead


### AI Content
I want to add an option to filter out AI generated content because many people want that, but I don't want to make it the default. AI content that was prompted and well-curated by humans is kinda fine in my book.

# wikipedia perennial sources query todo: automate
```js
$$(".perennial-sources tr.s-gr td:last-of-type a").map(a=>a.href)
```