import sys
import queue
import threading
import argparse
from urllib.request import Request, urlopen
from urllib.parse import urljoin, urlparse
from html.parser import HTMLParser
from concurrent.futures import ThreadPoolExecutor

class LinkParser(HTMLParser):
    """Streaming HTML parser to extract <a> tags."""
    def __init__(self, base_url):
        super().__init__()
        self.base_url = base_url
        self.links = []

    def handle_starttag(self, tag, attrs):
        if tag == 'a':
            for name, value in attrs:
                if name == 'href':
                    # Normalize: handle relative paths and strip fragments (#)
                    full_url = urljoin(self.base_url, value).split('#')[0]
                    self.links.append(full_url)

class Crawler:
    def __init__(self, start_url, excludes, output_file, max_workers=10):
        self.start_url = start_url
        self.excludes = excludes
        self.output_file = output_file
        self.base_netloc = urlparse(start_url).netloc
        self.max_workers = max_workers
        
        # Thread-safe structures
        self.visited_urls = set()          # Hashmap for visited URLs
        self.discovered_domains = set()    # Hashmap for final unique domains
        self.url_queue = queue.Queue()
        self.lock = threading.Lock()

    def fetch_links(self, url):
        """Downloads a page and returns extracted links."""
        try:
            req = Request(url, headers={'User-Agent': 'Mozilla/5.0 (Crawler)'})
            with urlopen(req, timeout=10) as response:
                if 'text/html' not in response.headers.get('Content-Type', ''):
                    return []
                
                html = response.read().decode('utf-8', errors='ignore')
                parser = LinkParser(url)
                parser.feed(html)
                return parser.links
        except Exception:
            return []

    def is_excluded(self, url):
        """Checks if the URL starts with any of the excluded prefixes."""
        return any(url.startswith(prefix) for prefix in self.excludes)

    def worker(self):
        """Main loop for each thread."""
        while True:
            try:
                # Threads exit if the queue stays empty for 3 seconds
                current_url = self.url_queue.get(timeout=3)
            except queue.Empty:
                return

            # Extract and store the domain
            domain = urlparse(current_url).netloc
            if domain:
                with self.lock:
                    self.discovered_domains.add(domain)

            # Only follow/scrape if internal and not excluded
            is_internal = urlparse(current_url).netloc == self.base_netloc
            if is_internal and not self.is_excluded(current_url):
                links = self.fetch_links(current_url)
                
                with self.lock:
                    for link in links:
                        if link not in self.visited_urls:
                            self.visited_urls.add(link)
                            self.url_queue.put(link)
            
            # Progress indicator to stdout
            print(f"[*] Scanned: {current_url} | Found: {len(self.discovered_domains)} domains", end='\r')
            self.url_queue.task_done()

    def run(self):
        # Seed the crawl
        self.visited_urls.add(self.start_url)
        self.url_queue.put(self.start_url)

        print(f"Starting crawl: {self.start_url}")
        print(f"Output file: {self.output_file}")
        print(f"Excluding: {', '.join(self.excludes) if self.excludes else 'None'}\n")

        with ThreadPoolExecutor(max_workers=self.max_workers) as executor:
            for _ in range(self.max_workers):
                executor.submit(self.worker)

        self.url_queue.join()
        
        # Write unique domains to file
        try:
            with open(self.output_file, 'w') as f:
                for domain in sorted(self.discovered_domains):
                    f.write(f"{domain}\n")
            print(f"\n\nDone! {len(self.discovered_domains)} unique domains saved to '{self.output_file}'.")
        except Exception as e:
            print(f"\n\nError writing to file: {e}")

if __name__ == "__main__":
    parser = argparse.ArgumentParser(description="Parallel recursive domain crawler.")
    parser.add_argument("url", help="Starting URL (e.g., https://ooh.directory)")
    parser.add_argument("-o", "--output", default="domains.txt", 
                        help="Output file for unique domains (default: domains.txt)")
    parser.add_argument("-x", "--exclude", nargs="*", default=[], 
                        help="URL prefixes to exclude from scraping.")
    parser.add_argument("-w", "--workers", type=int, default=10, 
                        help="Number of concurrent threads (default: 10).")

    args = parser.parse_args()
    
    crawler = Crawler(args.url, args.exclude, args.output, args.workers)
    crawler.run()